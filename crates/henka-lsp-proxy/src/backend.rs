//! The `LanguageServer` implementation the LSP framework drives.
//!
//! At `initialize` this connects to henka over MCP, works out which registered
//! project the workspace is, and asks henka what operations it offers there.
//! The capabilities advertised to the client are filtered from that catalog
//! (see the supported-surface section of `docs/lsp-proxy-plan.md`), so a
//! Rust-only project doesn't falsely claim Java-only code actions.
//!
//! When the catalog can't be read — henka has never seen the project, or the
//! lookup failed — the proxy advertises everything it implements instead.
//! Advertising nothing would be worse than advertising too much: a client only
//! sends requests for capabilities it was told about, so an inert session could
//! never recover, not even once the project is registered, whereas an
//! over-broad one costs one actionable error per request until it is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::OnceCell;
use tower_lsp_server::jsonrpc::{Error as LspError, Result as LspResult};
use tower_lsp_server::lsp_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::config::Config;
use crate::convert::{henka_edit_to_workspace_edit, uri_to_path, usages_to_locations};
use crate::mcp::{McpClient, McpClientError};
use crate::session::{Binding, OperationDescriptor, Session};

/// Java-only ops that live under `workspace/executeCommand` because they have
/// no LSP-standard shape (see the supported-surface section of the plan).
const EXEC_COMMAND_OPS: &[&str] = &["change-signature", "move"];

/// Op ids handled via LSP-standard methods; anything else in the catalog we
/// leave off the capability list (henka may grow ops that don't yet map).
const STANDARD_OP_REFERENCES: &str = "find-usages";
const STANDARD_OP_RENAME: &str = "rename";

/// The tower-lsp backend. Owns the MCP session once initialize has run.
pub struct Backend {
    pub client: Client,
    pub config: Config,
    /// Session is filled in during `initialize`; nothing else runs before it,
    /// so every other handler can `expect` it.
    session: OnceCell<Arc<Session>>,
}

impl Backend {
    pub fn new(client: Client, config: Config) -> Self {
        Self {
            client,
            config,
            session: OnceCell::new(),
        }
    }

    /// Access to the initialized session. Panics if `initialize` hasn't run;
    /// per LSP spec no other request may precede it.
    pub fn session(&self) -> &Session {
        self.session
            .get()
            .expect("session accessed before initialize")
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        // Reject clients that can't work in UTF-16 up front. Henka speaks
        // UTF-16 natively and we chose UTF-16-only for the proxy (see the
        // position-encoding open question in the plan); refusing here is
        // clearer than silently mis-computing offsets.
        if !accepts_utf16(&params) {
            return Err(LspError::invalid_params(
                "henka-lsp-proxy requires the client to accept UTF-16 position \
                 encoding, but the client only offered other encodings",
            ));
        }

        // Multi-workspace is out of scope (the plan's multi-workspace open
        // question resolves to rejecting N>1). Fail fast so a
        // misconfiguration is visible.
        let folders = params.workspace_folders.as_deref().unwrap_or(&[]);
        if folders.len() > 1 {
            return Err(LspError::invalid_params(format!(
                "henka-lsp-proxy supports a single workspace folder; \
                 initialize sent {}",
                folders.len()
            )));
        }
        let workspace_path = resolve_workspace_path(&params).ok_or_else(|| {
            LspError::invalid_params(
                "initialize did not carry a workspaceFolder or rootUri the proxy \
                 could resolve to a filesystem path",
            )
        })?;

        // Connect to henka. A connect failure is fatal for the session (no
        // op is going to succeed) — surface it via LSP error rather than
        // crashing the process.
        let mcp = McpClient::connect(&self.config.henka_url)
            .await
            .map_err(|e| LspError {
                code: tower_lsp_server::jsonrpc::ErrorCode::InternalError,
                message: e.to_string().into(),
                data: None,
            })?;

        let session = Arc::new(Session::new(
            mcp,
            workspace_path,
            self.config.project_override.clone(),
            &params,
        ));

        // Bind the workspace to a project and read its catalog. A failure is
        // not fatal: the session stays up, the binding is retried on the next
        // request, and until then we advertise everything so there is a
        // request to retry with.
        let capabilities = match session.binding().await {
            Ok(binding) => {
                log_project_status(&self.client, &session.mcp, &binding.project_id).await;
                advertise_capabilities(&binding.operations)
            }
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, e.message.to_string())
                    .await;
                unfiltered_capabilities()
            }
        };

        // OnceCell::set errs if already set; initialize runs once per session.
        let _ = self.session.set(session);

        Ok(InitializeResult {
            capabilities,
            server_info: Some(ServerInfo {
                name: env!("CARGO_PKG_NAME").into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("LSP session initialized");
    }

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.session()
            .documents
            .set(&params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Full-text sync: the last change carries the whole document.
        if let Some(change) = params.content_changes.into_iter().next_back() {
            self.session()
                .documents
                .set(&params.text_document.uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.session().documents.remove(&params.text_document.uri);
    }

    async fn references(&self, params: ReferenceParams) -> LspResult<Option<Vec<Location>>> {
        let session = self.session();
        let binding = session.binding().await?;
        let uri = &params.text_document_position.text_document.uri;
        let Some(file) = uri_to_path(uri) else {
            return Err(LspError::invalid_params(
                "textDocument.uri is not a file:// URI",
            ));
        };
        ensure_buffer_saved(session, uri, &file)?;
        let position = params.text_document_position.position;

        let args = binding.call_args(serde_json::json!({
            "file": file,
            "line": position.line,
            "character": position.character,
            "include_declaration": params.context.include_declaration,
        }));

        let response = session
            .mcp
            .call_tool("find-usages", args)
            .await
            .map_err(mcp_to_lsp)?;

        let locations =
            usages_to_locations(response, &binding.workspace_path).map_err(mcp_to_lsp)?;
        Ok(Some(locations))
    }

    async fn prepare_rename(
        &self,
        _params: TextDocumentPositionParams,
    ) -> LspResult<Option<PrepareRenameResponse>> {
        // Henka has no dedicated prepare-rename op; a speculative rename would
        // be much more expensive than letting the client use the identifier at
        // the cursor. defaultBehavior tells the client to do exactly that (see
        // the method-by-method section of the plan).
        Ok(Some(PrepareRenameResponse::DefaultBehavior {
            default_behavior: true,
        }))
    }

    async fn rename(&self, params: RenameParams) -> LspResult<Option<WorkspaceEdit>> {
        let session = self.session();
        let binding = session.binding().await?;
        let uri = &params.text_document_position.text_document.uri;
        let Some(file) = uri_to_path(uri) else {
            return Err(LspError::invalid_params(
                "textDocument.uri is not a file:// URI",
            ));
        };
        ensure_buffer_saved(session, uri, &file)?;
        let position = params.text_document_position.position;

        let args = binding.call_args(serde_json::json!({
            "file": file,
            "line": position.line,
            "character": position.character,
            "new_name": params.new_name,
            "dry_run": true,
        }));

        let response = session
            .mcp
            .call_tool("rename", args)
            .await
            .map_err(mcp_to_lsp)?;
        let workspace_edit = workspace_edit_from_response(session, &binding, &response, "rename")?;
        Ok(Some(workspace_edit))
    }
}

/// Pull the structured edit out of a dry-run response and translate it for this
/// client. Shared by every handler that turns an operation into a
/// `WorkspaceEdit`.
fn workspace_edit_from_response(
    session: &Session,
    binding: &Binding,
    response: &serde_json::Value,
    op_id: &str,
) -> LspResult<WorkspaceEdit> {
    let edit_value = response.get("edit").ok_or_else(|| LspError {
        code: tower_lsp_server::jsonrpc::ErrorCode::InternalError,
        message: format!(
            "`{op_id}` dry_run response carries no `edit` field — is henka-server new enough?"
        )
        .into(),
        data: None,
    })?;
    henka_edit_to_workspace_edit(
        edit_value,
        &binding.workspace_path,
        session.supports_document_changes,
    )
    .map_err(mcp_to_lsp)
}

/// Turn an MCP error into an LSP error, keeping the henka message verbatim in
/// `.message` so a client (Claude Code) surfaces it to the operator.
fn mcp_to_lsp(err: McpClientError) -> LspError {
    LspError {
        code: tower_lsp_server::jsonrpc::ErrorCode::InternalError,
        message: err.to_string().into(),
        data: None,
    }
}

/// Whether the client either offered UTF-16 explicitly or didn't declare a
/// list at all (LSP default is UTF-16).
fn accepts_utf16(params: &InitializeParams) -> bool {
    let Some(general) = params.capabilities.general.as_ref() else {
        return true;
    };
    let Some(encodings) = general.position_encodings.as_ref() else {
        return true;
    };
    encodings.iter().any(|e| e == &PositionEncodingKind::UTF16)
}

/// Extract a container-side path from `workspaceFolders[0]` (or `rootUri` /
/// `rootPath` as fallback).
#[allow(deprecated)] // root_uri and root_path are deprecated LSP fields but still populated by many clients
fn resolve_workspace_path(params: &InitializeParams) -> Option<PathBuf> {
    if let Some(folder) = params.workspace_folders.as_ref().and_then(|f| f.first()) {
        return uri_to_path(&folder.uri);
    }
    if let Some(uri) = params.root_uri.as_ref() {
        return uri_to_path(uri);
    }
    params.root_path.as_ref().map(PathBuf::from)
}

/// Ask henka for `project_status` and log the VCS state, so the operator sees
/// at a glance whether henka's copy matches theirs.
async fn log_project_status(client: &Client, mcp: &McpClient, project_id: &str) {
    let Ok(status) = mcp
        .call_tool("project_status", serde_json::json!({ "id": project_id }))
        .await
    else {
        return;
    };
    let vcs = status.get("vcs");
    let revision = vcs.and_then(|v| v.get("revision")).and_then(|v| v.as_str());
    let digest = vcs.and_then(|v| v.get("digest")).and_then(|v| v.as_str());
    let changed = vcs
        .and_then(|v| v.get("changed_files"))
        .and_then(|v| v.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    let message = format!(
        "henka sees project `{project_id}` at revision {rev} (digest {digest}, {changed} changed files)",
        rev = revision.unwrap_or("<none>"),
        digest = digest.unwrap_or("<clean>"),
    );
    tracing::info!("{message}");
    client.log_message(MessageType::INFO, message).await;
}

/// The capabilities that don't depend on henka's catalog at all.
fn base_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        // Not for editing: the proxy tracks open buffers only so it can refuse
        // a request whose coordinates no longer match the file henka reads.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        ..Default::default()
    }
}

/// Build the `ServerCapabilities` we advertise, filtered by what henka actually
/// exposes for this project.
fn advertise_capabilities(operations: &[OperationDescriptor]) -> ServerCapabilities {
    let mut caps = base_capabilities();

    let ids: std::collections::HashSet<&str> = operations.iter().map(|o| o.id.as_str()).collect();

    if ids.contains(STANDARD_OP_REFERENCES) {
        caps.references_provider = Some(OneOf::Left(true));
    }
    if ids.contains(STANDARD_OP_RENAME) {
        caps.rename_provider = Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        }));
    }

    let action_kinds: Vec<CodeActionKind> = operations
        .iter()
        .filter_map(|o| o.code_action_kind.clone().map(CodeActionKind::from))
        .collect();
    if !action_kinds.is_empty() {
        caps.code_action_provider =
            Some(CodeActionProviderCapability::Options(CodeActionOptions {
                code_action_kinds: Some(action_kinds),
                resolve_provider: Some(true),
                work_done_progress_options: Default::default(),
            }));
    }

    let exec_commands: Vec<String> = EXEC_COMMAND_OPS
        .iter()
        .filter(|op| ids.contains(**op))
        .map(|op| format!("henka.{op}"))
        .collect();
    if !exec_commands.is_empty() {
        caps.execute_command_provider = Some(ExecuteCommandOptions {
            commands: exec_commands,
            work_done_progress_options: Default::default(),
        });
    }

    caps
}

/// Everything the proxy implements, advertised without a catalog to filter by.
///
/// Used when the workspace couldn't be bound to a project. The client then has
/// a request to send for every surface, each of which re-attempts the binding
/// and either succeeds or answers with the message naming what to register.
fn unfiltered_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        references_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        // No kind list: without a catalog there is nothing to enumerate, and a
        // client that asks for `only: [...]` is filtered against the catalog in
        // the codeAction handler once one is available.
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: None,
            resolve_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: EXEC_COMMAND_OPS
                .iter()
                .map(|op| format!("henka.{op}"))
                .collect(),
            work_done_progress_options: Default::default(),
        }),
        ..base_capabilities()
    }
}

/// Refuse a request whose coordinates came from a buffer that no longer matches
/// the file henka will read. `path` is the file the request targets.
fn ensure_buffer_saved(session: &Session, uri: &Uri, path: &Path) -> LspResult<()> {
    session
        .documents
        .check_saved(uri, path)
        .map_err(LspError::invalid_params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str, kind: Option<&str>) -> OperationDescriptor {
        OperationDescriptor {
            id: id.into(),
            title: id.into(),
            code_action_kind: kind.map(str::to_string),
        }
    }

    #[test]
    fn empty_catalog_advertises_nothing_but_the_base() {
        // A project henka knows but has no operations for: there is genuinely
        // nothing to offer, and the base capabilities stand alone.
        let caps = advertise_capabilities(&[]);
        assert!(caps.references_provider.is_none());
        assert!(caps.rename_provider.is_none());
        assert!(caps.code_action_provider.is_none());
        assert!(caps.execute_command_provider.is_none());
        assert_eq!(caps.position_encoding, Some(PositionEncodingKind::UTF16));
        assert!(caps.text_document_sync.is_some());
    }

    #[test]
    fn unbound_session_advertises_every_surface() {
        // Nothing to filter by, so nothing is filtered out — otherwise the
        // client would never send a request that could retry the binding.
        let caps = unfiltered_capabilities();
        assert!(caps.references_provider.is_some());
        assert!(caps.rename_provider.is_some());
        assert!(caps.execute_command_provider.is_some());
        let Some(CodeActionProviderCapability::Options(opts)) = caps.code_action_provider else {
            panic!("code_action_provider not set");
        };
        assert_eq!(opts.resolve_provider, Some(true));
        assert!(
            opts.code_action_kinds.is_none(),
            "no catalog means no kind list to advertise"
        );
        assert_eq!(caps.position_encoding, Some(PositionEncodingKind::UTF16));
    }

    #[test]
    fn references_op_advertises_references_provider() {
        let caps = advertise_capabilities(&[desc("find-usages", None)]);
        assert!(matches!(caps.references_provider, Some(OneOf::Left(true))));
    }

    #[test]
    fn code_action_ops_are_collected_into_kinds() {
        let ops = vec![
            desc("extract-variable", Some("refactor.extract.variable")),
            desc("inline", Some("refactor.inline")),
        ];
        let caps = advertise_capabilities(&ops);
        let Some(CodeActionProviderCapability::Options(opts)) = caps.code_action_provider else {
            panic!("code_action_provider not set");
        };
        assert_eq!(opts.resolve_provider, Some(true));
        let kinds = opts.code_action_kinds.unwrap();
        assert!(kinds.iter().any(|k| k.as_str() == "refactor.extract.variable"));
        assert!(kinds.iter().any(|k| k.as_str() == "refactor.inline"));
    }

    #[test]
    fn exec_command_ops_gate_execute_command_provider() {
        let caps = advertise_capabilities(&[desc("change-signature", None), desc("move", None)]);
        let opts = caps.execute_command_provider.unwrap();
        assert!(opts.commands.contains(&"henka.change-signature".to_string()));
        assert!(opts.commands.contains(&"henka.move".to_string()));
    }

    #[test]
    fn utf16_client_default_is_accepted() {
        // A client that declared no encodings list defaults to UTF-16.
        let params = InitializeParams::default();
        assert!(accepts_utf16(&params));
    }

    #[test]
    fn utf8_only_client_rejected() {
        let mut params = InitializeParams::default();
        params.capabilities.general = Some(GeneralClientCapabilities {
            position_encodings: Some(vec![PositionEncodingKind::UTF8]),
            ..Default::default()
        });
        assert!(!accepts_utf16(&params));
    }
}
