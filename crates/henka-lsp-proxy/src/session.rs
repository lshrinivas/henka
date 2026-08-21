//! Per-session state established at LSP `initialize` time.
//!
//! Holds the MCP client, the derived project identity, and the cached
//! operation catalog for the project — all captured once at startup so
//! subsequent request handlers don't re-query them per call.

use std::path::PathBuf;

use serde_json::Value;

use crate::mcp::McpClient;
use crate::project::WorkspaceIdentity;

/// Snapshot of what the proxy learned about the workspace at initialize.
#[derive(Debug)]
pub struct SessionInfo {
    /// The container-side workspace path (LSP `workspaceFolders[0]`).
    pub workspace_path: PathBuf,
    /// The derived henka project id + optional jj workspace suffix.
    pub identity: WorkspaceIdentity,
    /// The raw list_operations response, cached so codeAction and dynamic
    /// capability advertisement can filter descriptors client-side without
    /// another MCP round-trip per request.
    pub operations: Vec<OperationDescriptor>,
    /// Whether the derived project id was registered on henka at startup.
    /// When `false`, every op will fail with a project-not-registered error;
    /// we track it so requests can surface that up front.
    pub project_registered: bool,
}

/// A tenanted view over the shared MCP client.
///
/// Every LSP handler goes through this so it can attach the envelope fields
/// (`project`, `workspace`) that henka expects on every tool call — see
/// `crates/henka-server/src/mcp.rs:523-539`.
pub struct Session {
    pub mcp: McpClient,
    pub info: SessionInfo,
}

/// A minimal projection of henka's `OperationDescriptor` — just the fields the
/// proxy needs to answer LSP capability questions.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OperationDescriptor {
    pub id: String,
    /// LSP CodeActionKind if this op is a code-action refactoring, else `None`.
    #[serde(default)]
    pub code_action_kind: Option<String>,
}

impl Session {
    /// The envelope every tool call must carry: project id + explicit
    /// workspace path. Sent unconditionally even for the base workspace so
    /// henka doesn't have to infer.
    pub fn envelope(&self) -> Value {
        serde_json::json!({
            "project": self.info.identity.project_id,
            "workspace": self.info.workspace_path,
        })
    }

    /// Merge `envelope()` fields into `params` (envelope wins on conflict).
    pub fn call_args(&self, params: Value) -> Value {
        let mut obj = match params {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        obj.insert(
            "project".into(),
            Value::String(self.info.identity.project_id.clone()),
        );
        obj.insert(
            "workspace".into(),
            Value::String(self.info.workspace_path.display().to_string()),
        );
        Value::Object(obj)
    }
}
