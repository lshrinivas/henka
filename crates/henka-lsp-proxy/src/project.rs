//! Deriving Henka's project id from an LSP `workspaceFolders[0]` path.
//!
//! Convention (see `docs/lsp-proxy-plan.md` §3): a container path like
//! `/root/<repo>` is the base project; `/root/<repo>.<workspace>` is a jj
//! workspace of that repo. The base is what Henka registers; the jj workspace
//! name is passed separately as the `workspace` field on each MCP call.

use std::path::Path;

/// The result of splitting a workspace path into its project id and any jj
/// workspace suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    /// The Henka project id (a slug of the repo directory name).
    pub project_id: String,
    /// The jj workspace name if the directory carries a `.<suffix>` — `None`
    /// when the caller is on the base repo.
    pub jj_workspace: Option<String>,
}

/// Derive the project id and jj workspace name from a workspace path.
///
/// Splits the basename on the first `.`: `stargate` → `stargate` with no jj
/// workspace; `stargate.foo` → `stargate` + `foo`; `stargate.foo.bar` →
/// `stargate` + `foo.bar` (a jj workspace name may contain dots).
pub fn derive_identity(workspace_path: &Path) -> Option<WorkspaceIdentity> {
    let basename = workspace_path.file_name()?.to_str()?;
    let (repo, suffix) = match basename.split_once('.') {
        Some((repo, rest)) => (repo, Some(rest.to_string())),
        None => (basename, None),
    };
    let project_id = slugify(repo);
    if project_id.is_empty() {
        return None;
    }
    Some(WorkspaceIdentity {
        project_id,
        jj_workspace: suffix,
    })
}

/// Lowercase alphanumeric slug matching Henka's `derive_id` in
/// `crates/henka-core/src/registry.rs` (a-z, 0-9, `-`; anything else becomes
/// `-`; runs collapsed; trimmed).
fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_dash = true; // suppress leading dashes
    for ch in input.chars() {
        let mapped = match ch {
            'a'..='z' | '0'..='9' => Some(ch),
            'A'..='Z' => Some(ch.to_ascii_lowercase()),
            '-' | '_' | ' ' | '.' => Some('-'),
            _ => None,
        };
        match mapped {
            Some('-') => {
                if !prev_dash {
                    out.push('-');
                    prev_dash = true;
                }
            }
            Some(c) => {
                out.push(c);
                prev_dash = false;
            }
            None => {}
        }
    }
    // Trim trailing dash.
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn base_repo_has_no_jj_workspace() {
        let id = derive_identity(&PathBuf::from("/root/stargate")).unwrap();
        assert_eq!(id.project_id, "stargate");
        assert_eq!(id.jj_workspace, None);
    }

    #[test]
    fn dot_suffix_becomes_jj_workspace() {
        let id = derive_identity(&PathBuf::from("/root/stargate.feature1")).unwrap();
        assert_eq!(id.project_id, "stargate");
        assert_eq!(id.jj_workspace.as_deref(), Some("feature1"));
    }

    #[test]
    fn multi_dot_suffix_kept_verbatim() {
        // jj permits dots in workspace names; only the first `.` is the split.
        let id = derive_identity(&PathBuf::from("/root/stargate.foo.bar")).unwrap();
        assert_eq!(id.project_id, "stargate");
        assert_eq!(id.jj_workspace.as_deref(), Some("foo.bar"));
    }

    #[test]
    fn non_root_mount_still_derives() {
        let id = derive_identity(&PathBuf::from("/workspaces/svc-x")).unwrap();
        assert_eq!(id.project_id, "svc-x");
        assert_eq!(id.jj_workspace, None);
    }

    #[test]
    fn empty_or_hidden_basename_rejected() {
        // A basename starting with '.' would slugify to an empty string.
        assert!(derive_identity(&PathBuf::from("/root/.hidden")).is_none());
    }
}
