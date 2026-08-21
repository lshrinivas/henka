//! Deriving Henka's project id from an LSP `workspaceFolders[0]` path.
//!
//! Henka registers a project under a slug of its root directory's name (see
//! the path model section of `docs/lsp-proxy-plan.md`), so the workspace path
//! the client opens is enough to guess an id. The guess is only a fallback:
//! the same slug rule is reproduced here, character for character, from
//! Henka's own `derive_id`, but a project may have been registered under an
//! explicit id that has nothing to do with its directory name.

use std::path::{Path, PathBuf};

/// One entry of Henka's `list_projects` response — the fields needed to tell
/// which registered project is the workspace the client opened.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RegisteredProject {
    pub id: String,
    pub root: PathBuf,
}

/// Which registered project is `workspace_path`.
///
/// The root is the primary key: a jj workspace is registered as a project in
/// its own right, and its directory name is a poor guess at the id (the id may
/// have been given explicitly at registration, and `/root/trino.io` and
/// `/root/trino` are different trees whose names slug differently anyway).
/// Only when no root matches — Henka seeing the tree under a mount path the
/// client doesn't share — does the basename slug stand in, and then only if
/// some registered project actually answers to it.
pub fn resolve_project_id(
    projects: &[RegisteredProject],
    workspace_path: &Path,
) -> Option<String> {
    let canonical = workspace_path.canonicalize().ok();
    let matches_workspace = |root: &Path| {
        root == workspace_path || canonical.as_deref().is_some_and(|c| root == c)
    };
    if let Some(project) = projects.iter().find(|p| matches_workspace(&p.root)) {
        return Some(project.id.clone());
    }

    let derived = derive_project_id(workspace_path)?;
    projects
        .iter()
        .any(|p| p.id == derived)
        .then_some(derived)
}

/// Guess Henka's project id for a workspace path: the slug of its basename.
///
/// `None` only when the path has no basename at all (`/`). Otherwise the guess
/// is whatever Henka's own `derive_id` would have produced, down to its
/// `project` fallback for a basename with no usable characters.
pub fn derive_project_id(workspace_path: &Path) -> Option<String> {
    let basename = workspace_path.file_name()?.to_string_lossy();
    Some(slugify(&basename))
}

/// Byte-for-byte reproduction of Henka's `derive_id`
/// (`crates/henka-core/src/registry.rs`): lowercase, every character outside
/// `[a-z0-9]` becomes `-` (runs are *not* collapsed), then `-` is trimmed from
/// both ends, with `project` standing in for an empty result. Any divergence
/// here is a guess that silently names a different project, so it copies the
/// original rather than paraphrasing it.
fn slugify(input: &str) -> String {
    let slug: String = input
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "project".to_string()
    } else {
        slug.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn basename_becomes_the_id() {
        assert_eq!(
            derive_project_id(&PathBuf::from("/root/stargate")).as_deref(),
            Some("stargate")
        );
    }

    #[test]
    fn dots_are_part_of_the_slug() {
        // A jj workspace directory (`<repo>.<workspace>`) is a project root in
        // its own right, and a dotted name like `trino.io` is not a workspace
        // suffix at all. Both slugify whole, the way Henka's derive_id does.
        assert_eq!(
            derive_project_id(&PathBuf::from("/root/stargate.feature1")).as_deref(),
            Some("stargate-feature1")
        );
        assert_eq!(
            derive_project_id(&PathBuf::from("/root/trino.io")).as_deref(),
            Some("trino-io")
        );
    }

    #[test]
    fn non_root_mount_still_derives() {
        assert_eq!(
            derive_project_id(&PathBuf::from("/workspaces/svc-x")).as_deref(),
            Some("svc-x")
        );
    }

    #[test]
    fn basename_without_usable_characters_falls_back_like_henka() {
        // Henka's derive_id names such a project `project`; guessing anything
        // else would miss a project that is in fact registered.
        assert_eq!(
            derive_project_id(&PathBuf::from("/root/...")).as_deref(),
            Some("project")
        );
    }

    #[test]
    fn root_has_no_basename() {
        assert!(derive_project_id(&PathBuf::from("/")).is_none());
    }

    fn registered(id: &str, root: &str) -> RegisteredProject {
        RegisteredProject {
            id: id.into(),
            root: PathBuf::from(root),
        }
    }

    #[test]
    fn registered_root_wins_over_the_basename_guess() {
        // `/root/trino.io` slugs to `trino-io`, but it is registered as
        // `trino-website`. Matching the root gets the right project; guessing
        // from the name would either miss or, worse, hit a different tree.
        let projects = vec![
            registered("trino", "/root/trino"),
            registered("trino-website", "/root/trino.io"),
        ];
        assert_eq!(
            resolve_project_id(&projects, &PathBuf::from("/root/trino.io")).as_deref(),
            Some("trino-website")
        );
    }

    #[test]
    fn basename_guess_covers_a_root_the_client_cannot_see() {
        // Henka reads the tree under its own mount, so no root matches the
        // client's path. The slug still names a registered project.
        let projects = vec![registered("stargate", "/workspaces/stargate")];
        assert_eq!(
            resolve_project_id(&projects, &PathBuf::from("/home/me/src/stargate")).as_deref(),
            Some("stargate")
        );
    }

    #[test]
    fn unregistered_workspace_resolves_to_nothing() {
        // Neither root nor id matches: the caller has to say so rather than
        // send requests against a project that would act on another tree.
        let projects = vec![registered("trino", "/root/trino")];
        assert!(resolve_project_id(&projects, &PathBuf::from("/root/stargate")).is_none());
    }
}
