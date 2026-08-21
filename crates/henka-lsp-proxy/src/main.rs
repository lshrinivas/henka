//! Entry point for the LSP proxy binary.
//!
//! Speaks LSP over stdio (stdout is reserved for the LSP transport; all logs
//! go to stderr). The server itself is a `LanguageServer` implementation
//! driven by `tower-lsp-server`; on `initialize` it opens an MCP session to
//! henka and every request thereafter becomes one MCP `tools/call`.

use std::process::ExitCode;

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
    Server::new(stdin, stdout, socket).serve(service).await;
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
