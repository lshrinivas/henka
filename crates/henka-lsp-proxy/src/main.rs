//! Entry point for the LSP proxy binary. Reads config from the environment,
//! initializes tracing (to stderr; stdout is the LSP transport), and runs the
//! LSP server loop against henka over MCP.

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    if let Err(err) = init_tracing() {
        eprintln!("henka-lsp-proxy: failed to init tracing: {err}");
        return ExitCode::FAILURE;
    }
    let config = henka_lsp_proxy::config::Config::from_env();
    tracing::info!(url = %config.henka_url, "starting henka-lsp-proxy");

    // The LSP server main loop is added in a follow-up commit. This stub keeps
    // the binary buildable and lets `--help`-style probes (Claude Code's plugin
    // discovery) succeed while the full surface is being wired up.
    ExitCode::SUCCESS
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
