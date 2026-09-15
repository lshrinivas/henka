//! Locating, launching, and driving the typescript-language-server.
//!
//! The generic document lifecycle lives in [`henka_lsp::LspSession`]; this
//! module adds the server's launch over Node, the `initialize` handshake, and
//! the per-extension language ids for TypeScript and JavaScript files.

use std::path::{Path, PathBuf};

use henka_core::provider::RequestGuard;
use henka_lsp::{LspClient, LspSession, path_to_file_uri};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::error::{Result, TsError};

/// The extensions this backend serves, each mapped to its LSP `languageId`.
const LANGS: &[(&str, &str)] = &[
    ("ts", "typescript"),
    ("mts", "typescript"),
    ("cts", "typescript"),
    ("tsx", "typescriptreact"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("jsx", "javascriptreact"),
];

/// Locate a typescript-language-server executable.
///
/// Searches, in order: `$HENKA_TYPESCRIPT_LANGUAGE_SERVER`,
/// `$TYPESCRIPT_LANGUAGE_SERVER`, the bundled
/// `./.cache/typescript-language-server/node_modules/.bin/...`, the same under
/// `$XDG_CACHE_HOME`/`~/.cache/henka`, and finally `PATH`. A candidate that
/// exists but cannot be executed is skipped, so one broken install early in the
/// order does not mask a working one later.
pub fn locate() -> Result<PathBuf> {
    const BIN: &str = "node_modules/.bin/typescript-language-server";
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in [
        "HENKA_TYPESCRIPT_LANGUAGE_SERVER",
        "TYPESCRIPT_LANGUAGE_SERVER",
    ] {
        if let Some(p) = std::env::var_os(var) {
            candidates.push(PathBuf::from(p));
        }
    }
    candidates.push(PathBuf::from(".cache/typescript-language-server").join(BIN));
    candidates.push(cache_base().join("typescript-language-server").join(BIN));

    // What each candidate turned out to be, for the error if none of them work.
    let mut looked: Vec<String> = Vec::new();
    for path in &candidates {
        if !path.is_file() {
            looked.push(path.display().to_string());
            continue;
        }
        // Absolutize without resolving symlinks — the bundled bin is a
        // symlink to a Node script, and the session runs the child from the
        // project root, so a relative path would not resolve.
        let path = std::path::absolute(path).unwrap_or_else(|_| path.clone());
        if runs(&path) {
            return Ok(path);
        }
        looked.push(format!("{} (found, does not run)", path.display()));
    }
    if which_on_path("typescript-language-server") && runs(Path::new("typescript-language-server"))
    {
        return Ok(PathBuf::from("typescript-language-server"));
    }
    looked.push("typescript-language-server (PATH)".to_string());

    Err(TsError::ServerNotFound(looked.join(", ")))
}

/// Whether `program` can actually be run, as opposed to merely existing: spawn
/// it with `--version` and wait for a clean exit.
///
/// Existence is the wrong test. The bundled server is a Node script behind a
/// symlink, so it is a present, executable file whether or not the Node it
/// needs is there, and an install can carry the server without the TypeScript
/// it drives. Left unprobed, either surfaces only on the first request, as the
/// language server connection closing for no stated reason. Probing costs one
/// short-lived child process, once, when the provider is built.
fn runs(program: &Path) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether `name` resolves to an executable on `PATH`.
fn which_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

/// The base cache directory: `$XDG_CACHE_HOME`/`~/.cache` under `henka`.
fn cache_base() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("henka")
}

/// A running, initialized typescript-language-server session for one project,
/// wrapping the generic [`LspSession`] with TS-specific startup.
pub struct TsSession {
    session: LspSession,
}

impl TsSession {
    /// Launch typescript-language-server (the executable at `program`) for
    /// `root` and perform the initialize handshake.
    pub async fn start(program: &Path, root: &Path) -> Result<Self> {
        let mut command = Command::new(program);
        command.arg("--stdio").current_dir(root).kill_on_drop(true);

        let client = LspClient::spawn(command)?;
        initialize(&client, root).await?;
        Ok(Self {
            session: LspSession::new(client, root, LANGS),
        })
    }

    /// The underlying LSP client.
    pub fn client(&self) -> &LspClient {
        self.session.client()
    }

    /// The project root.
    pub fn root(&self) -> &Path {
        self.session.root()
    }

    /// The checkout results should quote their source from — the working copy
    /// of an active overlay, else the project root.
    pub fn content_root(&self) -> PathBuf {
        self.session.content_root()
    }

    /// Open a document if it isn't already, returning its URI.
    pub async fn ensure_open(&self, path: &Path) -> Result<String> {
        Ok(self.session.ensure_open(path).await?)
    }

    /// Open the project's sources once so cross-file results are complete.
    pub async fn ensure_indexed(&self) -> Result<()> {
        Ok(self.session.ensure_indexed().await?)
    }

    /// Search the project for symbols matching `query`, capped at `limit`,
    /// quoting each match's declaring line with `context_lines` extra lines.
    pub async fn symbol_search(
        &self,
        query: &str,
        limit: usize,
        context_lines: usize,
    ) -> Result<Value> {
        Ok(self
            .session
            .symbol_search(query, limit, context_lines)
            .await?)
    }

    /// Sync files changed on disk back into the server.
    pub async fn sync_changed_impl(&self, changed: &[PathBuf]) {
        self.session.sync_changed(changed).await;
    }

    /// Begin one request, serializing on the session and clearing leaked overlays.
    pub async fn begin_request_impl(&self) -> RequestGuard {
        self.session.begin_request().await
    }

    /// Overlay a working copy's content onto the shared index.
    pub async fn overlay_workspace_impl(
        &self,
        workspace_root: &Path,
        delta: &[PathBuf],
    ) -> Result<()> {
        Ok(self.session.overlay_workspace(workspace_root, delta).await?)
    }

    /// Restore the base index view after an overlay.
    pub async fn restore_overlay_impl(&self) {
        self.session.restore_overlay().await;
    }

    /// Shut the server down.
    pub async fn shutdown(&self) -> Result<()> {
        Ok(self.session.shutdown().await?)
    }
}

/// Send `initialize`/`initialized`, advertising the capabilities needed for
/// rename, references, and code-action refactors.
async fn initialize(client: &LspClient, root: &Path) -> Result<()> {
    let root_uri = path_to_file_uri(root);
    let params = json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": {
            "workspace": {
                "applyEdit": true,
                "workspaceEdit": { "documentChanges": true, "resourceOperations": ["create", "rename", "delete"] },
                "configuration": true,
            },
            "textDocument": {
                "synchronization": { "didSave": true, "dynamicRegistration": true },
                "rename": { "dynamicRegistration": true, "prepareSupport": true },
                "references": { "dynamicRegistration": true },
                "hover": { "dynamicRegistration": true, "contentFormat": ["markdown", "plaintext"] },
                "definition": { "dynamicRegistration": true },
                "implementation": { "dynamicRegistration": true },
                "documentSymbol": { "dynamicRegistration": true, "hierarchicalDocumentSymbolSupport": true },
                "callHierarchy": { "dynamicRegistration": true },
                "codeAction": {
                    "dynamicRegistration": true,
                    "codeActionLiteralSupport": {
                        "codeActionKind": {
                            "valueSet": ["", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source", "source.organizeImports"]
                        }
                    },
                    "resolveSupport": { "properties": ["edit"] }
                }
            }
        },
        "initializationOptions": {}
    });

    let _: Value = client.request("initialize", params).await?;
    client.notify("initialized", json!({})).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn runs_rejects_a_file_that_cannot_be_executed() {
        assert!(runs(Path::new("/usr/bin/true")));
        assert!(!runs(Path::new("/nonexistent/typescript-language-server")));

        // A present, executable file that still cannot be spawned, which is
        // what `is_file` alone happily accepts.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("typescript-language-server");
        std::fs::write(&fake, b"not a runnable script").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!runs(&fake));
    }
}
