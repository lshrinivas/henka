//! Runtime configuration read from the process environment.
//!
//! All settings live in env vars so the plugin loader (Claude Code) can pass
//! them without a config file. See the configuration section of
//! `docs/lsp-proxy-plan.md`.

use std::env;

/// Default MCP endpoint when `HENKA_URL` is unset. Only useful for local
/// testing when the proxy and henka-server sit on the same host.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8181/mcp";

/// Environment-driven runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The MCP endpoint (typically `http://host.docker.internal:8181/mcp`).
    pub henka_url: String,
    /// Optional override for the derived project id — an escape hatch for a
    /// workspace whose directory name doesn't match its registered id.
    pub project_override: Option<String>,
}

impl Config {
    /// Read configuration from the environment.
    pub fn from_env() -> Self {
        Self {
            henka_url: env::var("HENKA_URL").unwrap_or_else(|_| DEFAULT_URL.into()),
            project_override: env::var("HENKA_PROXY_PROJECT")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }
}
