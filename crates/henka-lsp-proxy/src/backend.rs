//! The `LanguageServer` implementation the LSP framework drives.
//!
//! At `initialize` this connects to henka over MCP, derives the project id
//! from `workspaceFolders[0]`, and asks henka for the operation catalog. The
//! set of capabilities we advertise is filtered from that catalog per §4 of
//! `docs/lsp-proxy-plan.md`, so a Rust-only project doesn't falsely claim
//! Java-only code actions and a project henka has never seen still lets the
//! LSP session stay up (per-op errors surface the misconfiguration).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::OnceCell;
use tower_lsp_server::jsonrpc::{Error as LspError, Result as LspResult};
use tower_lsp_server::lsp_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::config::Config;
use crate::convert::{henka_edit_to_workspace_edit, uri_to_path, usages_to_locations};
use crate::mcp::{McpClient, McpClientError};
use crate::project::{WorkspaceIdentity, derive_identity};
use crate::session::{OperationDescriptor, Session, SessionInfo};

/// Java-only ops that live under `workspace/executeCommand` because they
/// have no LSP-standard shape (see plan §4).
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
        // UTF-16 natively and we chose UTF-16-only for the proxy (see plan
        // open-question #1); refusing here is clearer than silently
        // mis-computing offsets.
        if !accepts_utf16(&params) {
            return Err(LspError::invalid_params(
                "henka-lsp-proxy requires the client to accept UTF-16 position \
                 encoding, but the client only offered other encodings",
            ));
        }

        // Multi-workspace is out of scope (plan open-question #5: reject if
        // >1). Fail fast so a misconfiguration is visible.
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

        // Derive the henka project id, honoring the override env var when set.
        let mut identity = derive_identity(&workspace_path).ok_or_else(|| {
            LspError::invalid_params(format!(
                "cannot derive a henka project id from workspace path `{}`",
                workspace_path.display()
            ))
        })?;
        if let Some(id) = self.config.project_override.clone() {
            tracing::info!(
                derived = %identity.project_id,
                override = %id,
                "HENKA_PROXY_PROJECT overrides the derived project id"
            );
            identity.project_id = id;
        }
        tracing::info!(
            project = %identity.project_id,
            jj_workspace = ?identity.jj_workspace,
            workspace = %workspace_path.display(),
            "resolved workspace identity"
        );

        // Connect to henka. A connect failure is fatal for the session (no
        // op is going to succeed) — surface it via LSP error rather than
        // crashing the process.
        let mcp = McpClient::connect(&self.config.henka_url).await.map_err(|e| {
            LspError {
                code: tower_lsp_server::jsonrpc::ErrorCode::InternalError,
                message: e.to_string().into(),
                data: None,
            }
        })?;

        // Verify the project exists on henka and pull the operation catalog.
        // Neither is fatal: an unregistered project is an operator-fixable
        // config error and per-op errors will surface it (plan §3).
        let project_registered = check_project_registered(&mcp, &identity).await;
        let operations = if project_registered {
            fetch_operations(&mcp, &identity.project_id).await
        } else {
            self.client
                .log_message(
                    MessageType::ERROR,
                    format!(
                        "henka has no project registered with id `{id}`. \
                         Register it on the host with `register_project` \
                         against the host path of the base repo — the proxy \
                         will not auto-register.",
                        id = identity.project_id
                    ),
                )
                .await;
            Vec::new()
        };

        // A one-shot check that henka sees roughly the same tree we do.
        if project_registered {
            log_project_status(&self.client, &mcp, &identity).await;
        }

        let capabilities = advertise_capabilities(&operations);

        let info = SessionInfo {
            workspace_path,
            identity,
            operations,
            project_registered,
        };
        let session = Arc::new(Session { mcp, info });
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

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> LspResult<Option<Vec<Location>>> {
        let session = self.session();
        let Some(file) = uri_to_path(&params.text_document_position.text_document.uri) else {
            return Err(LspError::invalid_params(
                "textDocument.uri is not a file:// URI",
            ));
        };
        let position = params.text_document_position.position;

        let args = session.call_args(serde_json::json!({
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

        let locations = usages_to_locations(response, &session.info.workspace_path)
            .map_err(mcp_to_lsp)?;
        Ok(Some(locations))
    }

    async fn prepare_rename(
        &self,
        _params: TextDocumentPositionParams,
    ) -> LspResult<Option<PrepareRenameResponse>> {
        // Henka has no dedicated prepare-rename op; a speculative rename would
        // be much more expensive than letting the client use the identifier
        // at the cursor. defaultBehavior tells the client to do exactly that
        // (plan §5).
        Ok(Some(PrepareRenameResponse::DefaultBehavior {
            default_behavior: true,
        }))
    }

    async fn rename(
        &self,
        params: RenameParams,
    ) -> LspResult<Option<WorkspaceEdit>> {
        let session = self.session();
        let Some(file) = uri_to_path(&params.text_document_position.text_document.uri) else {
            return Err(LspError::invalid_params(
                "textDocument.uri is not a file:// URI",
            ));
        };
        let position = params.text_document_position.position;

        let args = session.call_args(serde_json::json!({
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

        let edit_value = response.get("edit").ok_or_else(|| LspError {
            code: tower_lsp_server::jsonrpc::ErrorCode::InternalError,
            message: "rename dry_run response missing `edit` field — is henka-server new enough?".into(),
            data: None,
        })?;
        let workspace_edit =
            henka_edit_to_workspace_edit(edit_value, &session.info.workspace_path)
                .map_err(mcp_to_lsp)?;
        Ok(Some(workspace_edit))
    }
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
    if let Some(folder) = params
        .workspace_folders
        .as_ref()
        .and_then(|f| f.first())
    {
        return uri_to_path(&folder.uri);
    }
    if let Some(uri) = params.root_uri.as_ref() {
        return uri_to_path(uri);
    }
    params.root_path.as_ref().map(PathBuf::from)
}

/// Ask henka whether `identity.project_id` is registered. Returns `false` on
/// any transport / parse failure — the caller logs an actionable error.
async fn check_project_registered(mcp: &McpClient, identity: &WorkspaceIdentity) -> bool {
    match mcp.call_tool("list_projects", serde_json::json!({})).await {
        Ok(value) => value
            .as_array()
            .map(|projects| {
                projects
                    .iter()
                    .any(|p| p.get("id").and_then(|v| v.as_str()) == Some(&identity.project_id))
            })
            .unwrap_or(false),
        Err(e) => {
            tracing::warn!(error = %e, "list_projects failed at initialize");
            false
        }
    }
}

/// Fetch the operation catalog for the project. On any failure returns an
/// empty catalog; the proxy will advertise nothing and every request will
/// return method-not-found rather than fail the whole session.
async fn fetch_operations(mcp: &McpClient, project: &str) -> Vec<OperationDescriptor> {
    let result = mcp
        .call_tool("list_operations", serde_json::json!({ "project": project }))
        .await;
    match result {
        Ok(value) => serde_json::from_value(value).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "list_operations returned unexpected shape");
            Vec::new()
        }),
        Err(e) => {
            tracing::warn!(error = %e, "list_operations failed");
            Vec::new()
        }
    }
}

/// Ask henka for `project_status` and log the VCS state, so the operator sees
/// at a glance whether henka's copy matches theirs.
async fn log_project_status(client: &Client, mcp: &McpClient, identity: &WorkspaceIdentity) {
    let Ok(status) = mcp
        .call_tool("project_status", serde_json::json!({ "id": identity.project_id }))
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
        "henka sees project `{id}` at revision {rev} (digest {digest}, {changed} changed files)",
        id = identity.project_id,
        rev = revision.unwrap_or("<none>"),
        digest = digest.unwrap_or("<clean>"),
    );
    tracing::info!("{message}");
    client.log_message(MessageType::INFO, message).await;
}

/// Build the `ServerCapabilities` we advertise, filtered by what henka
/// actually exposes for this project (see plan §4).
fn advertise_capabilities(operations: &[OperationDescriptor]) -> ServerCapabilities {
    let mut caps = ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        ..Default::default()
    };

    let ids: std::collections::HashSet<&str> =
        operations.iter().map(|o| o.id.as_str()).collect();

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
        caps.code_action_provider = Some(CodeActionProviderCapability::Options(CodeActionOptions {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str, kind: Option<&str>) -> OperationDescriptor {
        OperationDescriptor {
            id: id.into(),
            code_action_kind: kind.map(str::to_string),
        }
    }

    #[test]
    fn empty_catalog_advertises_nothing_but_encoding() {
        let caps = advertise_capabilities(&[]);
        assert!(caps.references_provider.is_none());
        assert!(caps.rename_provider.is_none());
        assert!(caps.code_action_provider.is_none());
        assert!(caps.execute_command_provider.is_none());
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
