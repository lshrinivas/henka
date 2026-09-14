//! The Henka MCP server binary.

mod mcp;
mod ops;
mod pathmap;
mod server_config;

use std::path::PathBuf;

use clap::Parser;
use henka_core::{ProjectRegistry, ProviderRegistry, default_config_path};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use crate::mcp::HenkaMcp;
use crate::server_config::{ServerConfig, Transport};

/// Multi-tenant MCP server for code refactorings.
///
/// Every setting below can also be given in the server configuration file
/// (`henka.toml`); the command line wins over the environment, which wins over
/// the file, which wins over the built-in default.
#[derive(Debug, Parser)]
#[command(name = "henka", version, about)]
struct Cli {
    /// How clients connect. Defaults to stdio, or `[server] transport` from the
    /// server configuration file.
    #[arg(long, value_enum)]
    transport: Option<Transport>,

    /// Address to bind when `--transport http`. Defaults to loopback on 8181,
    /// or `[server] bind` from the server configuration file; pass
    /// `0.0.0.0:<port>` to listen on all interfaces. The server is
    /// unauthenticated, so binding beyond loopback exposes every registered
    /// project to anyone who can reach the port.
    #[arg(long)]
    bind: Option<String>,

    /// Path to the project registry file. Defaults to
    /// `$XDG_CONFIG_HOME/henka/projects.toml`.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Path to the server configuration file (`henka.toml`), separate from the
    /// project registry. Defaults to `$HENKA_SERVER_CONFIG`, else
    /// `$HENKA_DATA/henka.toml`, else `$XDG_CONFIG_HOME/henka/henka.toml`.
    #[arg(long)]
    server_config: Option<PathBuf>,

    /// Serve the LSP surface in addition to MCP, on its own port. Can also be
    /// enabled with `[lsp] enabled = true` in the server configuration file;
    /// this flag only ever turns it on.
    #[arg(long)]
    lsp: bool,

    /// Address to bind the LSP surface to when enabled. Defaults to
    /// `127.0.0.1:8182`, or `[lsp] bind` from the server configuration file.
    /// Like the MCP transport, the LSP surface is unauthenticated, so binding
    /// beyond loopback exposes every registered project to anyone who can reach
    /// the port.
    #[arg(long, value_name = "ADDR", env = "HENKA_LSP_BIND")]
    lsp_bind: Option<String>,

    /// Additional `Host` header value to accept on `--transport http`, beyond
    /// the loopback defaults (localhost, 127.0.0.1, ::1). The HTTP transport
    /// rejects other hosts as a DNS-rebinding guard; add the host your client
    /// connects as — e.g. `--allowed-host host.docker.internal` for a client in
    /// a container. A value without a port matches any port. Repeatable, or set
    /// `HENKA_MCP_ALLOWED_HOST` to a space-separated list (handy in a container,
    /// where the command is the image default), or list them under
    /// `[server] allowed_hosts` in the server configuration file.
    #[arg(
        long = "allowed-host",
        value_name = "HOST",
        env = "HENKA_MCP_ALLOWED_HOST",
        value_delimiter = ' '
    )]
    allowed_hosts: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Load the server config before anything else — it can supply the log
    // filter, so tracing cannot be initialized until it is read. An invalid
    // config therefore fails the process before any project work begins, with
    // the error surfacing through `main`'s return rather than the log.
    let server_config_path = cli.server_config.unwrap_or_else(ServerConfig::default_path);
    let server_config = ServerConfig::load(&server_config_path)?;

    init_tracing(&server_config.resolve_log(std::env::var("HENKA_LOG").ok()));

    // Resolve the surfaces' settings up front (command line over the
    // environment over the file over the defaults).
    let lsp = server_config.resolve_lsp(cli.lsp, cli.lsp_bind);
    let server = server_config.resolve_server(cli.transport, cli.bind, &cli.allowed_hosts);
    let path_map = server_config.resolve_path_map(std::env::var("HENKA_PATH_MAP").ok());
    if lsp.enabled {
        tracing::info!(bind = %lsp.bind, "LSP surface enabled");
    }

    let config_path = cli.config.unwrap_or_else(default_config_path);
    tracing::info!(config = %config_path.display(), "loading project registry");
    let registry = ProjectRegistry::load(&config_path)?;
    tracing::info!(projects = registry.len(), "registry loaded");

    let providers = build_providers();
    let handler = HenkaMcp::with_path_map(registry, providers, path_map);
    // Auto-register projects sitting under the workspace roots, so a client can
    // operate on them without a manual register_project call.
    handler.warm_registry().await;

    match server.transport {
        Transport::Stdio => {
            let service = handler.serve(stdio()).await?;
            service.waiting().await?;
        }
        Transport::Http => serve_http(handler, &server.bind, &server.allowed_hosts).await?,
    }
    Ok(())
}

/// Serve the handler over streamable HTTP at `/mcp`, one MCP session per client.
async fn serve_http(
    handler: HenkaMcp,
    bind: &str,
    allowed_hosts: &[String],
) -> anyhow::Result<()> {
    // Keep rmcp's loopback-only DNS-rebinding guard, extended with any hosts the
    // operator explicitly trusts. Ignore blanks (e.g. an unset
    // HENKA_MCP_ALLOWED_HOST passed through as an empty string).
    let extra: Vec<String> = allowed_hosts
        .iter()
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .collect();
    let mut config = StreamableHttpServerConfig::default();
    config.allowed_hosts.extend(extra.iter().cloned());
    if !extra.is_empty() {
        tracing::info!(allowed_hosts = ?config.allowed_hosts, "accepting additional Host headers");
    }

    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        std::sync::Arc::new(LocalSessionManager::default()),
        config,
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "serving MCP over streamable HTTP at /mcp");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Assemble the language providers. A provider that cannot start (e.g. Java
/// when no jdtls distribution is available) is logged and skipped, so the
/// server still serves the languages that are ready.
fn build_providers() -> ProviderRegistry {
    let mut providers = ProviderRegistry::new();
    match henka_lang_java::JavaProvider::new() {
        Ok(java) => {
            tracing::info!("Java provider ready (jdtls located)");
            providers.register(std::sync::Arc::new(java));
        }
        Err(e) => {
            tracing::warn!(error = %e, "Java provider unavailable; Java operations disabled");
        }
    }
    match henka_lang_rust::RustProvider::new() {
        Ok(rust) => {
            tracing::info!("Rust provider ready (rust-analyzer located)");
            providers.register(std::sync::Arc::new(rust));
        }
        Err(e) => {
            tracing::warn!(error = %e, "Rust provider unavailable; Rust operations disabled");
        }
    }
    match henka_lang_ts::TsProvider::new() {
        Ok(ts) => {
            tracing::info!("TypeScript/JavaScript provider ready (typescript-language-server located)");
            providers.register_for(henka_lang_ts::LANGUAGES, std::sync::Arc::new(ts));
        }
        Err(e) => {
            tracing::warn!(error = %e, "TypeScript/JavaScript provider unavailable; TS/JS operations disabled");
        }
    }
    providers
}

/// Initialize tracing to stderr — stdout is reserved for the MCP stdio channel.
///
/// `spec` is the already-resolved filter (`HENKA_LOG` over the configuration
/// file's `[server] log` over the default). An unusable spec falls back to
/// `info` rather than failing startup, matching how an invalid `HENKA_LOG` has
/// always behaved; the file's TOML is still validated strictly at load.
fn init_tracing(spec: &str) {
    use tracing_subscriber::EnvFilter;
    let filter =
        EnvFilter::try_new(spec).unwrap_or_else(|_| EnvFilter::new(server_config::DEFAULT_LOG));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
}
