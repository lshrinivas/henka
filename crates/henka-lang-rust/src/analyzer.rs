//! Locating, launching, and driving the rust-analyzer language server.
//!
//! The generic document lifecycle lives in [`henka_lsp::LspSession`]; this
//! module adds rust-analyzer's launch, the `initialize` handshake, and the
//! readiness wait (its `experimental/serverStatus` quiescent signal).

use std::path::{Path, PathBuf};

use henka_core::provider::RequestGuard;
use henka_lsp::{LspClient, LspSession, path_to_file_uri};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::time::Duration;

use crate::error::{Result, RustError};

/// Locate a rust-analyzer binary.
///
/// Searches, in order: `$HENKA_RUST_ANALYZER`, `$RUST_ANALYZER`, the bundled
/// `./.cache/rust-analyzer/rust-analyzer`, the same under `$XDG_CACHE_HOME`/
/// `~/.cache/henka`, and finally `rust-analyzer` on `PATH`. A candidate that
/// exists but cannot be executed is skipped, so one stale binary early in the
/// order does not mask a working one later.
pub fn locate() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in ["HENKA_RUST_ANALYZER", "RUST_ANALYZER"] {
        if let Some(p) = std::env::var_os(var) {
            candidates.push(PathBuf::from(p));
        }
    }
    candidates.push(PathBuf::from(".cache/rust-analyzer/rust-analyzer"));
    candidates.push(cache_base().join("rust-analyzer/rust-analyzer"));

    // What each candidate turned out to be, for the error if none of them work.
    let mut looked: Vec<String> = Vec::new();
    for path in &candidates {
        if !path.is_file() {
            looked.push(path.display().to_string());
            continue;
        }
        // Absolutize: the session sets the child's working directory to the
        // project root, so a relative program path would not resolve.
        let path = path.canonicalize().unwrap_or_else(|_| path.clone());
        if runs(&path) {
            return Ok(path);
        }
        looked.push(format!("{} (found, does not run)", path.display()));
    }
    // Fall back to PATH lookup by name; the OS resolves it at spawn time.
    if which_on_path("rust-analyzer") && runs(Path::new("rust-analyzer")) {
        return Ok(PathBuf::from("rust-analyzer"));
    }
    looked.push("rust-analyzer (PATH)".to_string());

    Err(RustError::RustAnalyzerNotFound(looked.join(", ")))
}

/// Whether `program` can actually be run, as opposed to merely existing: spawn
/// it with `--version` and wait for a clean exit.
///
/// Existence is the wrong test. A cache shared with a container holds a binary
/// for the wrong platform, and a rustup shim for an uninstalled component sits
/// on `PATH` looking executable; both are present files that die at spawn. Left
/// unprobed, that surfaces only on the first request, as the language server
/// connection closing for no stated reason. Probing costs one short-lived child
/// process, once, when the provider is built.
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

/// A running, initialized rust-analyzer session for one project, wrapping the
/// generic [`LspSession`] with rust-analyzer-specific startup.
pub struct RaSession {
    session: LspSession,
}

impl RaSession {
    /// Launch rust-analyzer (the binary at `program`) for `root` and perform the
    /// initialize handshake, waiting until analysis settles.
    pub async fn start(program: &Path, root: &Path) -> Result<Self> {
        let mut command = Command::new(program);
        command.current_dir(root).kill_on_drop(true);

        let client = LspClient::spawn(command)?;
        // Subscribe before initializing so the readiness signal isn't missed.
        let mut status = client.subscribe();
        initialize(&client, root).await?;
        wait_for_ready(&mut status).await;
        Ok(Self {
            session: LspSession::new(client, root, &[("rs", "rust")]),
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
/// rename and references plus rust-analyzer's server-status notification.
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
            },
            "window": { "workDoneProgress": true },
            // Ask rust-analyzer to report when it has finished analysis.
            "experimental": { "serverStatusNotification": true }
        },
        "initializationOptions": {}
    });

    let _: Value = client.request("initialize", params).await?;
    client.notify("initialized", json!({})).await?;
    Ok(())
}

/// How long to wait for rust-analyzer to finish its initial analysis. Reaching
/// the deadline is non-fatal — requests may see an incomplete index.
const READY_TIMEOUT: Duration = Duration::from_secs(180);

/// Wait until rust-analyzer reports it is quiescent (done analyzing), via its
/// `experimental/serverStatus` notification.
async fn wait_for_ready(status: &mut broadcast::Receiver<(String, Value)>) {
    let waited = tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            match status.recv().await {
                Ok((method, params)) if method == "experimental/serverStatus" => {
                    if params.get("quiescent").and_then(Value::as_bool) == Some(true) {
                        return;
                    }
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
    .await;
    if waited.is_err() {
        tracing::warn!("timed out waiting for rust-analyzer to become ready; proceeding");
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn runs_rejects_a_file_that_cannot_be_executed() {
        assert!(runs(Path::new("/usr/bin/true")));
        assert!(!runs(Path::new("/nonexistent/rust-analyzer")));

        // A present, executable file that still cannot be spawned: the shape of
        // a cached binary built for another platform, which is what `is_file`
        // alone happily accepts.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("rust-analyzer");
        std::fs::write(&fake, b"\x7fELF and nothing that runs").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!runs(&fake));
    }
}
