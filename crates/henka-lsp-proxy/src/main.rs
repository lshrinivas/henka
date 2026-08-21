//! Entry point for the LSP proxy binary.
//!
//! Speaks LSP over stdio (stdout is reserved for the LSP transport; all logs
//! go to stderr). The server itself is a `LanguageServer` implementation
//! driven by `tower-lsp-server`; on `initialize` it opens an MCP session to
//! henka and every request thereafter becomes one MCP `tools/call`.
//!
//! # Cancellation
//!
//! LSP `$/cancelRequest` is handled by tower-lsp-server itself: when the
//! notification arrives, the framework drops the pending request future,
//! which drops the in-flight `McpClient::call_tool` future. Henka has no
//! MCP-side cancellation, so the op keeps running to completion on the
//! server; the client just stops waiting. That's the documented limitation
//! from plan §6 / open-question #4.

use std::process::ExitCode;

use tokio::signal::unix::{SignalKind, signal};
use tower_lsp_server::{LspService, Server};
use tracing_subscriber::EnvFilter;

use henka_lsp_proxy::backend::Backend;
use henka_lsp_proxy::config::Config;

fn main() -> ExitCode {
    if let Err(err) = init_tracing() {
        eprintln!("henka-lsp-proxy: failed to init tracing: {err}");
        return ExitCode::FAILURE;
    }
    let config = Config::from_env();
    tracing::info!(url = %config.henka_url, "starting henka-lsp-proxy");

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(error = %err, "failed to start tokio runtime");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(run(config));
    ExitCode::SUCCESS
}

async fn run(config: Config) {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(move |client| Backend::new(client, config.clone()));

    // The LSP server drives shutdown itself on `exit` (stdin closes). SIGTERM
    // and SIGHUP are treated as an equivalent orderly shutdown — otherwise a
    // parent that kills the child would leave the tokio runtime running.
    let server = Server::new(stdin, stdout, socket).serve(service);
    tokio::select! {
        _ = server => {
            tracing::info!("LSP server loop exited");
        }
        reason = wait_for_signal() => {
            tracing::info!(reason, "received termination signal, exiting");
        }
    }
}

/// Wait for the first of SIGTERM or SIGHUP. Returns the signal name so
/// tracing can record which one arrived. Errors constructing a signal
/// handler collapse to a never-resolving future — signal handling is a
/// best-effort niceness, not a correctness requirement.
async fn wait_for_signal() -> &'static str {
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "cannot install SIGTERM handler");
            return futures_never().await;
        }
    };
    let mut hup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "cannot install SIGHUP handler");
            return futures_never().await;
        }
    };
    tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = hup.recv() => "SIGHUP",
    }
}

/// A future that never completes. Used when a signal handler failed to
/// install — the LSP loop should still be the primary shutdown path.
async fn futures_never() -> &'static str {
    std::future::pending().await
}

/// Initialize `tracing` writing to stderr (stdout is the LSP transport).
fn init_tracing() -> Result<(), Box<dyn std::error::Error>> {
    let filter = EnvFilter::try_from_env("HENKA_PROXY_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|e| e.to_string())?;
    Ok(())
}
