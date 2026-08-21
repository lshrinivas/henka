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

/// A minimal projection of henka's `OperationDescriptor` — just the fields
/// the proxy needs to answer LSP capability questions and dispatch code
/// actions.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OperationDescriptor {
    pub id: String,
    /// LSP CodeActionKind if this op is a code-action refactoring, else `None`.
    #[serde(default)]
    pub code_action_kind: Option<String>,
    /// The op's target shape — Position, Selection, File, or Project.
    /// Used to build the right coordinate payload when a code action fires.
    #[serde(default)]
    pub target: TargetKind,
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
        merge_envelope(
            params,
            &self.info.identity.project_id,
            &self.info.workspace_path,
        )
    }
}

/// Pure form of [`Session::call_args`], factored out for unit testing.
///
/// The envelope always wins over a client-supplied `project` / `workspace` so
/// a caller can't accidentally target a different project. Non-object
/// `params` (unlikely — the handlers all build objects) are treated as an
/// empty map.
pub fn merge_envelope(params: Value, project: &str, workspace: &std::path::Path) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn envelope_wins_over_caller_supplied_project() {
        // A client that accidentally sets `project` gets it overridden — the
        // proxy's derived id is authoritative for the session.
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
        let merged = merge_envelope(
            json!({}),
            "stargate",
            Path::new("/root/stargate.feature1"),
        );
        assert_eq!(merged["workspace"], json!("/root/stargate.feature1"));
    }

    #[test]
    fn target_kind_deserializes_from_lowercase() {
        // Henka serializes TargetKind with lowercase names. The proxy must
        // decode them the same way to keep the code-action target dispatch honest.
        let desc: OperationDescriptor = serde_json::from_value(json!({
            "id": "extract-variable",
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
            "code_action_kind": null
        }))
        .unwrap();
        assert_eq!(desc.target, TargetKind::Position);
        assert_eq!(desc.code_action_kind, None);
    }
}
