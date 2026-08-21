//! Per-session state established at LSP `initialize` time.
//!
//! Holds the MCP client, what the client told us it can do, the open buffers,
//! and the project binding — which project id Henka answers to for this
//! workspace and what operations it offers there.
//!
//! The binding is resolved on demand rather than pinned at initialize. Henka
//! may not have the project registered when the editor starts, and a session
//! whose capabilities were computed from an empty catalog would be live but
//! unable to issue a single request for the rest of its life. Resolving it
//! late means registering the project is enough to make the session work.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::sync::RwLock;
use tower_lsp_server::jsonrpc::{Error as LspError, Result as LspResult};
use tower_lsp_server::lsp_types::InitializeParams;

use crate::documents::Documents;
use crate::mcp::{McpClient, McpClientError};
use crate::project::{RegisteredProject, derive_project_id, resolve_project_id};

/// Everything the proxy keeps for the life of one LSP session.
pub struct Session {
    pub mcp: McpClient,
    /// The workspace path the client opened, sent verbatim as the `workspace`
    /// field on every call so Henka doesn't have to infer which working copy
    /// an edit belongs in.
    pub workspace_path: PathBuf,
    /// `HENKA_PROXY_PROJECT`, when set: an explicit id that skips the lookup
    /// for a workspace Henka knows under a name nothing can derive.
    project_override: Option<String>,
    /// Whether the client understands `WorkspaceEdit.documentChanges`. A client
    /// that does must be handed those and not the `changes` map; one that
    /// doesn't can only be handed `changes`.
    pub supports_document_changes: bool,
    /// The buffers the client has open, so a request can be refused when its
    /// coordinates come from unsaved text.
    pub documents: Documents,
    /// The resolved binding, once a lookup has succeeded.
    binding: RwLock<Option<Binding>>,
}

/// The project a session acts on, as resolved against Henka's registry.
#[derive(Debug, Clone)]
pub struct Binding {
    /// The id Henka knows this workspace by.
    pub project_id: String,
    /// The workspace path, carried along so the call envelope can be built
    /// from the binding alone.
    pub workspace_path: PathBuf,
    /// The operation catalog for the project, used to advertise capabilities
    /// and to answer `textDocument/codeAction` without a round-trip per
    /// request.
    pub operations: Vec<OperationDescriptor>,
}

/// A minimal projection of henka's `OperationDescriptor` — just the fields
/// the proxy needs to answer LSP capability questions and dispatch code
/// actions.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OperationDescriptor {
    pub id: String,
    /// Human-readable name, shown as the code-action title.
    pub title: String,
    /// LSP CodeActionKind if this op is a code-action refactoring, else `None`.
    #[serde(default)]
    pub code_action_kind: Option<String>,
    /// The op's target shape — Position, Selection, File, or Project.
    /// Used to build the right coordinate payload when a code action fires.
    #[serde(default)]
    pub target: TargetKind,
    /// The languages the op applies to, as Henka's lowercase language ids.
    /// `list_operations` filters by project, not by file, so a project holding
    /// both Java and Rust returns Java-only ops too — without this the menu on
    /// a `.rs` buffer would offer them.
    #[serde(default)]
    pub languages: Vec<String>,
}

impl OperationDescriptor {
    /// Whether this operation applies to `file`, going by its language.
    ///
    /// An extension the proxy doesn't recognize is not a reason to hide an
    /// action: Henka may support a language this list predates, and it decides
    /// for itself when the op runs. The same goes for a descriptor that names
    /// no languages at all.
    pub fn applies_to(&self, file: &Path) -> bool {
        if self.languages.is_empty() {
            return true;
        }
        match language_of(file) {
            Some(language) => self.languages.iter().any(|l| l == language),
            None => true,
        }
    }
}

/// Henka's language id for a file, by extension. Mirrors
/// `henka_core::Language::from_path`; kept local so the proxy doesn't depend on
/// henka-core just to read a file extension.
fn language_of(file: &Path) -> Option<&'static str> {
    match file.extension().and_then(|e| e.to_str()) {
        Some("java") => Some("java"),
        Some("rs") => Some("rust"),
        Some("ts" | "tsx" | "mts" | "cts") => Some("typescript"),
        Some("js" | "jsx" | "mjs" | "cjs") => Some("javascript"),
        _ => None,
    }
}

/// Mirror of `henka_core::operation::TargetKind`. Kept as a small local enum
/// so the proxy's dependency on henka-core stays surface-level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    #[default]
    Position,
    Selection,
    File,
    Project,
}

impl Session {
    /// Build a session over a connected MCP client.
    pub fn new(
        mcp: McpClient,
        workspace_path: PathBuf,
        project_override: Option<String>,
        params: &InitializeParams,
    ) -> Self {
        Self {
            mcp,
            workspace_path,
            project_override,
            supports_document_changes: client_supports_document_changes(params),
            documents: Documents::default(),
            binding: RwLock::new(None),
        }
    }

    /// The project binding, resolving it against Henka on first use and again
    /// after every failed attempt.
    ///
    /// The error is the one the operator has to act on — which project was
    /// looked for and how to register it — so handlers can return it verbatim.
    pub async fn binding(&self) -> LspResult<Binding> {
        if let Some(binding) = self.binding.read().await.clone() {
            return Ok(binding);
        }
        let mut slot = self.binding.write().await;
        // Another request may have resolved it while this one waited.
        if let Some(binding) = slot.clone() {
            return Ok(binding);
        }
        let binding = self.resolve().await?;
        *slot = Some(binding.clone());
        Ok(binding)
    }

    /// One resolution attempt: find the project id, then read its catalog.
    /// Nothing is cached unless both succeed.
    async fn resolve(&self) -> LspResult<Binding> {
        let project_id = match &self.project_override {
            Some(id) => id.clone(),
            None => self.lookup_project_id().await?,
        };
        let operations = self.fetch_operations(&project_id).await?;
        tracing::info!(
            project = %project_id,
            operations = operations.len(),
            workspace = %self.workspace_path.display(),
            "bound workspace to henka project"
        );
        Ok(Binding {
            project_id,
            workspace_path: self.workspace_path.clone(),
            operations,
        })
    }

    async fn lookup_project_id(&self) -> LspResult<String> {
        let value = self
            .mcp
            .call_tool("list_projects", serde_json::json!({}))
            .await
            .map_err(internal_error)?;
        let projects: Vec<RegisteredProject> = serde_json::from_value(value).map_err(|e| {
            internal_error(McpClientError::BadJson {
                tool: "list_projects".into(),
                source: e,
            })
        })?;
        resolve_project_id(&projects, &self.workspace_path)
            .ok_or_else(|| LspError::invalid_params(unregistered_message(&self.workspace_path)))
    }

    async fn fetch_operations(&self, project: &str) -> LspResult<Vec<OperationDescriptor>> {
        let value = self
            .mcp
            .call_tool("list_operations", serde_json::json!({ "project": project }))
            .await
            .map_err(internal_error)?;
        serde_json::from_value(value).map_err(|e| {
            internal_error(McpClientError::BadJson {
                tool: "list_operations".into(),
                source: e,
            })
        })
    }
}

impl Binding {
    /// The descriptor for an op id, if the catalog holds one.
    pub fn operation(&self, id: &str) -> Option<&OperationDescriptor> {
        self.operations.iter().find(|d| d.id == id)
    }

    /// The envelope every tool call must carry: project id + explicit
    /// workspace path. Sent unconditionally even for the base workspace so
    /// henka doesn't have to infer it — `dispatch_operation` on the server side
    /// resolves the working copy an edit lands in from these two fields.
    pub fn call_args(&self, params: Value) -> Value {
        merge_envelope(params, &self.project_id, &self.workspace_path)
    }
}

/// Pure form of [`Binding::call_args`], factored out for unit testing.
///
/// The envelope always wins over a client-supplied `project` / `workspace` so
/// a caller can't accidentally target a different project. Non-object
/// `params` (unlikely — the handlers all build objects) are treated as an
/// empty map.
pub fn merge_envelope(params: Value, project: &str, workspace: &Path) -> Value {
    let mut obj = match params {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    obj.insert("project".into(), Value::String(project.into()));
    obj.insert(
        "workspace".into(),
        Value::String(workspace.display().to_string()),
    );
    Value::Object(obj)
}

/// The message an operator has to act on when no registered project matches
/// the workspace: what was looked for, and the two ways out.
fn unregistered_message(workspace_path: &Path) -> String {
    let derived = derive_project_id(workspace_path).unwrap_or_default();
    format!(
        "henka has no project rooted at `{path}`, and none registered under the \
         id `{derived}` its directory name would imply. Register it on the host \
         with `register_project` against that path, or set HENKA_PROXY_PROJECT \
         to the id it is registered under — the proxy will not auto-register.",
        path = workspace_path.display(),
    )
}

fn internal_error(err: McpClientError) -> LspError {
    LspError {
        code: tower_lsp_server::jsonrpc::ErrorCode::InternalError,
        message: err.to_string().into(),
        data: None,
    }
}

/// Whether the client declared support for `WorkspaceEdit.documentChanges`.
fn client_supports_document_changes(params: &InitializeParams) -> bool {
    params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.workspace_edit.as_ref())
        .and_then(|e| e.document_changes)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tower_lsp_server::lsp_types::{
        ClientCapabilities, WorkspaceClientCapabilities, WorkspaceEditClientCapabilities,
    };

    #[test]
    fn envelope_wins_over_caller_supplied_project() {
        // A client that accidentally sets `project` gets it overridden — the
        // id resolved for the session is authoritative.
        let merged = merge_envelope(
            json!({ "project": "wrong", "file": "Foo.java" }),
            "stargate",
            Path::new("/root/stargate"),
        );
        assert_eq!(merged["project"], json!("stargate"));
        assert_eq!(merged["file"], json!("Foo.java"));
    }

    #[test]
    fn workspace_carries_the_full_container_path() {
        let merged = merge_envelope(json!({}), "stargate", Path::new("/root/stargate.feature1"));
        assert_eq!(merged["workspace"], json!("/root/stargate.feature1"));
    }

    #[test]
    fn non_object_params_still_produce_an_envelope() {
        let merged = merge_envelope(json!("nonsense"), "stargate", Path::new("/root/stargate"));
        assert_eq!(
            merged,
            json!({ "project": "stargate", "workspace": "/root/stargate" })
        );
    }

    #[test]
    fn document_changes_support_defaults_to_false() {
        // Absent capability means the client can only be handed `changes`.
        assert!(!client_supports_document_changes(
            &InitializeParams::default()
        ));
    }

    #[test]
    fn document_changes_support_is_read_from_capabilities() {
        let params = InitializeParams {
            capabilities: ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    workspace_edit: Some(WorkspaceEditClientCapabilities {
                        document_changes: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(client_supports_document_changes(&params));
    }

    #[test]
    fn target_kind_deserializes_from_lowercase() {
        // Henka serializes TargetKind with lowercase names. The proxy must
        // decode them the same way to keep the code-action target dispatch
        // honest — a mis-decoded kind sends a position where a selection was
        // meant.
        let desc: OperationDescriptor = serde_json::from_value(json!({
            "id": "extract-variable",
            "title": "Extract variable",
            "code_action_kind": "refactor.extract.variable",
            "target": "selection"
        }))
        .unwrap();
        assert_eq!(desc.target, TargetKind::Selection);
    }

    #[test]
    fn missing_target_defaults_to_position() {
        // Older henka-server payloads didn't include `target`; the default
        // preserves the previous behaviour for those descriptors.
        let desc: OperationDescriptor = serde_json::from_value(json!({
            "id": "find-usages",
            "title": "Find usages",
            "code_action_kind": null
        }))
        .unwrap();
        assert_eq!(desc.target, TargetKind::Position);
        assert_eq!(desc.code_action_kind, None);
    }

    #[test]
    fn an_operation_applies_only_to_its_languages() {
        let desc: OperationDescriptor = serde_json::from_value(json!({
            "id": "organize-imports",
            "title": "Organize imports",
            "code_action_kind": "source.organizeImports",
            "languages": ["java"],
        }))
        .unwrap();
        assert!(desc.applies_to(Path::new("/root/p/src/Foo.java")));
        // The catalog is per-project, so a Java-only op comes back for a
        // project that also holds Rust. It must not be offered on the buffer.
        assert!(!desc.applies_to(Path::new("/root/p/src/lib.rs")));
    }

    #[test]
    fn an_unknown_extension_is_left_to_henka() {
        let desc: OperationDescriptor = serde_json::from_value(json!({
            "id": "organize-imports",
            "title": "Organize imports",
            "languages": ["java"],
        }))
        .unwrap();
        // Not something the proxy's extension table knows: hiding the action
        // would be a guess, and henka rejects the op itself if it doesn't fit.
        assert!(desc.applies_to(Path::new("/root/p/notes.txt")));
    }

    #[test]
    fn a_descriptor_naming_no_languages_applies_everywhere() {
        let desc: OperationDescriptor = serde_json::from_value(json!({
            "id": "rename",
            "title": "Rename",
        }))
        .unwrap();
        assert!(desc.applies_to(Path::new("/root/p/src/lib.rs")));
    }

    #[test]
    fn unregistered_message_names_both_ways_out() {
        let message = unregistered_message(Path::new("/root/trino.io"));
        assert!(message.contains("/root/trino.io"), "got: {message}");
        assert!(message.contains("trino-io"), "got: {message}");
        assert!(message.contains("register_project"), "got: {message}");
        assert!(message.contains("HENKA_PROXY_PROJECT"), "got: {message}");
    }
}
