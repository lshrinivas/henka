//! Server-process configuration, loaded from an optional `henka.toml`.
//!
//! This is deliberately separate from the project registry (`projects.toml`):
//! the registry is rewritten whenever a project is registered, so operator
//! settings live in their own file where a registration can never disturb them,
//! and the command line does not have to grow a flag for every setting.
//!
//! Every setting the file carries already has a command-line flag or an
//! environment variable; the file is a fourth, lowest-precedence source for a
//! value those could also supply. Settings resolve with the command line winning
//! over the environment, which wins over the file, which wins over the built-in
//! default. Where a setting has no command-line form (`log`, `path_map`) or no
//! environment form (`[lsp] enabled`), the ordering simply collapses over the
//! missing rung.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::Deserialize;

use crate::pathmap::PathMap;

/// The default address the LSP surface binds when enabled: loopback, on the
/// port after the MCP default of 8181. Loopback keeps the unauthenticated
/// surface off the network unless an operator binds it wider on purpose.
pub const DEFAULT_LSP_BIND: &str = "127.0.0.1:8182";

/// The default address the MCP HTTP transport binds: loopback, so the
/// unauthenticated surface stays off the network until bound wider on purpose.
pub const DEFAULT_MCP_BIND: &str = "127.0.0.1:8181";

/// The default tracing filter when neither `HENKA_LOG` nor the file sets one.
pub const DEFAULT_LOG: &str = "info";

/// How clients connect to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Standard input/output, for a single local client.
    Stdio,
    /// Streamable HTTP, for a hosted multi-client service.
    Http,
}

/// The server configuration file, with every setting optional so an absent file
/// (or an absent section) simply leaves the defaults in place.
///
/// Unknown keys are rejected rather than ignored: a misspelled setting that
/// silently did nothing would look exactly like one that was honoured, which is
/// the failure this file exists to avoid.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The `[server]` section.
    #[serde(default)]
    pub server: ServerFileConfig,
    /// The `[lsp]` section.
    #[serde(default)]
    pub lsp: LspFileConfig,
    /// The `[path_map]` section: `"<caller prefix>" = "<local prefix>"` entries,
    /// each equivalent to one `HENKA_PATH_MAP` pair.
    #[serde(default)]
    pub path_map: BTreeMap<String, String>,
}

/// The `[server]` section of the server configuration file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFileConfig {
    /// How clients connect.
    pub transport: Option<Transport>,
    /// The address to bind when the transport is HTTP.
    pub bind: Option<String>,
    /// Additional `Host` header values to accept on the HTTP transport.
    pub allowed_hosts: Option<Vec<String>>,
    /// The tracing filter, in `tracing_subscriber`'s `EnvFilter` syntax.
    pub log: Option<String>,
}

/// The `[lsp]` section of the server configuration file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspFileConfig {
    /// Whether to serve the LSP surface.
    pub enabled: Option<bool>,
    /// The address to bind it to.
    pub bind: Option<String>,
}

/// The effective LSP settings after folding the command line, environment, and
/// file over the defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSettings {
    /// Whether the LSP surface should be served.
    pub enabled: bool,
    /// The address to bind it to.
    pub bind: String,
}

/// The effective MCP-surface settings after the same fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    /// How clients connect.
    pub transport: Transport,
    /// The address to bind when `transport` is HTTP.
    pub bind: String,
    /// Additional `Host` header values to accept on the HTTP transport.
    pub allowed_hosts: Vec<String>,
}

impl ServerConfig {
    /// Load the configuration from `path`, or return the defaults when the file
    /// does not exist. A present-but-unparseable file is an error rather than a
    /// silent fallback, so a typo in an operator's config is surfaced.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolve the default configuration path, mirroring the registry's own
    /// resolution: `$HENKA_SERVER_CONFIG`, else `$HENKA_DATA/henka.toml`, else
    /// `$XDG_CONFIG_HOME/henka/henka.toml`, else `$HOME/.config/henka/henka.toml`.
    pub fn default_path() -> PathBuf {
        if let Some(explicit) = std::env::var_os("HENKA_SERVER_CONFIG") {
            return PathBuf::from(explicit);
        }
        if let Some(data) = henka_core::data_root() {
            return data.join("henka.toml");
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from(".config"));
        base.join("henka").join("henka.toml")
    }

    /// Fold the command line over this file's `[lsp]` settings and the defaults.
    ///
    /// `cli_enabled` reflects the `--lsp` flag (which only turns the surface on,
    /// so either the flag or the file may enable it). `cli_bind` carries
    /// `--lsp-bind` or its `HENKA_LSP_BIND` environment fallback, and wins over
    /// the file when present.
    pub fn resolve_lsp(&self, cli_enabled: bool, cli_bind: Option<String>) -> LspSettings {
        LspSettings {
            enabled: cli_enabled || self.lsp.enabled.unwrap_or(false),
            bind: cli_bind
                .or_else(|| self.lsp.bind.clone())
                .unwrap_or_else(|| DEFAULT_LSP_BIND.to_string()),
        }
    }

    /// Fold the command line over this file's `[server]` settings and the
    /// defaults.
    ///
    /// Each argument carries the value clap resolved, which already folds the
    /// command line over any environment variable the flag declares — so a
    /// `Some` (or non-empty, for the repeatable `--allowed-host`) argument
    /// outranks the file, and `None`/empty means neither was given.
    pub fn resolve_server(
        &self,
        cli_transport: Option<Transport>,
        cli_bind: Option<String>,
        cli_allowed_hosts: &[String],
    ) -> ServerSettings {
        // Blank values are treated as absent throughout: a container that always
        // passes `--allowed-host "$HENKA_MCP_ALLOWED_HOST"` should not shadow the
        // file with an empty string when the variable is unset.
        let cli_allowed_hosts: Vec<String> = non_blank(cli_allowed_hosts.iter().cloned());
        ServerSettings {
            transport: cli_transport
                .or(self.server.transport)
                .unwrap_or(Transport::Stdio),
            bind: cli_bind
                .filter(|b| !b.trim().is_empty())
                .or_else(|| self.server.bind.clone())
                .unwrap_or_else(|| DEFAULT_MCP_BIND.to_string()),
            allowed_hosts: if cli_allowed_hosts.is_empty() {
                self.server
                    .allowed_hosts
                    .as_deref()
                    .map(|hosts| non_blank(hosts.iter().cloned()))
                    .unwrap_or_default()
            } else {
                cli_allowed_hosts
            },
        }
    }

    /// Fold `HENKA_LOG` over this file's `[server] log` and the default.
    ///
    /// There is no command-line flag for the log filter, so the environment is
    /// the top rung here. The value is returned unvalidated: an unusable filter
    /// spec falls back to `info` at initialization, exactly as an invalid
    /// `HENKA_LOG` does today.
    pub fn resolve_log(&self, env_log: Option<String>) -> String {
        env_log
            .filter(|spec| !spec.trim().is_empty())
            .or_else(|| self.server.log.clone())
            .unwrap_or_else(|| DEFAULT_LOG.to_string())
    }

    /// Fold `HENKA_PATH_MAP` over this file's `[path_map]` section.
    ///
    /// Like the log filter, the path map has no command-line form, so the
    /// environment outranks the file directly. A set-but-blank variable counts
    /// as absent, so a container that always exports it does not shadow the file
    /// with an empty map.
    pub fn resolve_path_map(&self, env_spec: Option<String>) -> PathMap {
        match env_spec.filter(|spec| !spec.trim().is_empty()) {
            Some(spec) => PathMap::parse(&spec),
            None => PathMap::from_entries(
                self.path_map
                    .iter()
                    .map(|(caller, local)| (caller.as_str(), local.as_str())),
            ),
        }
    }
}

/// Drop blank entries, trimming the rest.
fn non_blank(values: impl IntoIterator<Item = String>) -> Vec<String> {
    values
        .into_iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("henka.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Load a config from an inline TOML body.
    fn load_config(body: &str) -> ServerConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, body);
        ServerConfig::load(&path).unwrap()
    }

    // ---- the file is read at all -------------------------------------------

    #[test]
    fn absent_file_is_all_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(
            config.resolve_lsp(false, None),
            LspSettings {
                enabled: false,
                bind: DEFAULT_LSP_BIND.to_string()
            }
        );
        assert_eq!(
            config.resolve_server(None, None, &[]),
            ServerSettings {
                transport: Transport::Stdio,
                bind: DEFAULT_MCP_BIND.to_string(),
                allowed_hosts: vec![],
            }
        );
        assert_eq!(config.resolve_log(None), DEFAULT_LOG);
        assert!(config.resolve_path_map(None).is_empty());
    }

    #[test]
    fn a_full_file_supplies_every_setting() {
        // The whole of a deployment's startup, written down: this is the file
        // that replaces `HENKA_LOG=debug HENKA_PATH_MAP=/root=/Users/me/Projects
        // henka --transport http --allowed-host host.docker.internal --lsp`.
        let config = load_config(
            r#"
            [server]
            transport = "http"
            bind = "0.0.0.0:9181"
            allowed_hosts = ["host.docker.internal"]
            log = "debug"

            [path_map]
            "/root" = "/Users/me/Projects"

            [lsp]
            enabled = true
            bind = "0.0.0.0:9182"
            "#,
        );

        assert_eq!(
            config.resolve_server(None, None, &[]),
            ServerSettings {
                transport: Transport::Http,
                bind: "0.0.0.0:9181".to_string(),
                allowed_hosts: vec!["host.docker.internal".to_string()],
            }
        );
        assert_eq!(config.resolve_log(None), "debug");
        assert_eq!(
            config.resolve_lsp(false, None),
            LspSettings {
                enabled: true,
                bind: "0.0.0.0:9182".to_string()
            }
        );
        assert_eq!(
            config.resolve_path_map(None).map(Path::new("/root/henka")),
            PathBuf::from("/Users/me/Projects/henka")
        );
    }

    #[test]
    fn an_absent_section_leaves_only_its_own_settings_at_the_default() {
        // `[server]` given, `[lsp]`/`[path_map]` absent: the file must not be
        // all-or-nothing.
        let config = load_config("[server]\ntransport = \"http\"\n");
        assert_eq!(config.resolve_server(None, None, &[]).transport, Transport::Http);
        assert_eq!(config.resolve_server(None, None, &[]).bind, DEFAULT_MCP_BIND);
        assert!(!config.resolve_lsp(false, None).enabled);
        assert!(config.resolve_path_map(None).is_empty());
    }

    #[test]
    fn path_map_takes_more_than_one_entry() {
        let config = load_config(
            r#"
            [path_map]
            "/root" = "/Users/me/Projects"
            "/data/repos" = "/mnt/repos"
            "#,
        );
        let map = config.resolve_path_map(None);
        assert_eq!(
            map.map(Path::new("/root/henka")),
            PathBuf::from("/Users/me/Projects/henka")
        );
        assert_eq!(
            map.map(Path::new("/data/repos/svc")),
            PathBuf::from("/mnt/repos/svc")
        );
    }

    // ---- precedence: cli > env > file > default ----------------------------

    #[test]
    fn transport_precedence_cli_over_file_over_default() {
        // No env form for `--transport`, so the ordering collapses to cli > file.
        let file = load_config("[server]\ntransport = \"http\"\n");
        let empty = ServerConfig::default();

        // default
        assert_eq!(
            empty.resolve_server(None, None, &[]).transport,
            Transport::Stdio
        );
        // file over default
        assert_eq!(
            file.resolve_server(None, None, &[]).transport,
            Transport::Http
        );
        // cli over file — including back to the value that is also the default,
        // which only a non-defaulted CLI option can express
        assert_eq!(
            file.resolve_server(Some(Transport::Stdio), None, &[]).transport,
            Transport::Stdio
        );
    }

    #[test]
    fn bind_precedence_cli_over_file_over_default() {
        let file = load_config("[server]\nbind = \"0.0.0.0:9181\"\n");
        assert_eq!(
            ServerConfig::default().resolve_server(None, None, &[]).bind,
            DEFAULT_MCP_BIND
        );
        assert_eq!(file.resolve_server(None, None, &[]).bind, "0.0.0.0:9181");
        assert_eq!(
            file.resolve_server(None, Some("127.0.0.1:7000".to_string()), &[])
                .bind,
            "127.0.0.1:7000"
        );
    }

    #[test]
    fn allowed_hosts_precedence_cli_or_env_over_file_over_default() {
        // clap folds `--allowed-host` over `HENKA_MCP_ALLOWED_HOST` before this
        // point, so both arrive as the same argument and both outrank the file.
        let file = load_config("[server]\nallowed_hosts = [\"from.file\"]\n");
        assert!(
            ServerConfig::default()
                .resolve_server(None, None, &[])
                .allowed_hosts
                .is_empty()
        );
        assert_eq!(
            file.resolve_server(None, None, &[]).allowed_hosts,
            vec!["from.file".to_string()]
        );
        assert_eq!(
            file.resolve_server(None, None, &["from.cli".to_string()])
                .allowed_hosts,
            vec!["from.cli".to_string()]
        );
        // The CLI/env list replaces the file's rather than adding to it, so an
        // operator can narrow what a deployment's file trusts.
        assert_eq!(
            file.resolve_server(None, None, &["a".to_string(), "b".to_string()])
                .allowed_hosts,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn log_precedence_env_over_file_over_default() {
        // No CLI flag for the log filter, so the environment is the top rung.
        let file = load_config("[server]\nlog = \"debug\"\n");
        assert_eq!(ServerConfig::default().resolve_log(None), DEFAULT_LOG);
        assert_eq!(file.resolve_log(None), "debug");
        assert_eq!(file.resolve_log(Some("trace".to_string())), "trace");
    }

    #[test]
    fn path_map_precedence_env_over_file_over_default() {
        let file = load_config("[path_map]\n\"/root\" = \"/from/file\"\n");
        assert!(ServerConfig::default().resolve_path_map(None).is_empty());
        assert_eq!(
            file.resolve_path_map(None).map(Path::new("/root/p")),
            PathBuf::from("/from/file/p")
        );
        // The environment replaces the file's map wholesale.
        let env = file.resolve_path_map(Some("/root=/from/env".to_string()));
        assert_eq!(
            env.map(Path::new("/root/p")),
            PathBuf::from("/from/env/p")
        );
    }

    #[test]
    fn lsp_precedence_cli_over_file_over_default() {
        let file = load_config("[lsp]\nenabled = true\nbind = \"0.0.0.0:9182\"\n");
        assert_eq!(
            ServerConfig::default().resolve_lsp(false, None),
            LspSettings {
                enabled: false,
                bind: DEFAULT_LSP_BIND.to_string()
            }
        );
        assert_eq!(
            file.resolve_lsp(false, None),
            LspSettings {
                enabled: true,
                bind: "0.0.0.0:9182".to_string()
            }
        );
        // `--lsp-bind` (or HENKA_LSP_BIND, folded in by clap) wins over the file.
        assert_eq!(
            file.resolve_lsp(false, Some("127.0.0.1:7000".to_string())).bind,
            "127.0.0.1:7000"
        );
    }

    #[test]
    fn the_lsp_flag_only_ever_enables() {
        // `--lsp` is a bare flag: absent means "unset", not "disabled", so it
        // cannot turn off what the file switched on.
        let config = ServerConfig::default();
        assert!(config.resolve_lsp(true, None).enabled);
        assert_eq!(config.resolve_lsp(true, None).bind, DEFAULT_LSP_BIND);

        let file = config_enabled_false();
        assert!(!file.resolve_lsp(false, None).enabled);
        assert!(file.resolve_lsp(true, None).enabled);
    }

    fn config_enabled_false() -> ServerConfig {
        load_config("[lsp]\nenabled = false\n")
    }

    // ---- blank values count as absent -------------------------------------

    #[test]
    fn blank_env_and_cli_values_do_not_shadow_the_file() {
        // A container that always exports the variable (or always passes the
        // flag) leaves a blank behind when it is unconfigured; that must fall
        // through to the file rather than wiping it.
        let file = load_config(
            r#"
            [server]
            bind = "0.0.0.0:9181"
            log = "debug"

            [path_map]
            "/root" = "/from/file"
            "#,
        );
        assert_eq!(
            file.resolve_server(None, Some("  ".to_string()), &[]).bind,
            "0.0.0.0:9181"
        );
        assert_eq!(
            file.resolve_server(None, None, &["".to_string()])
                .allowed_hosts,
            Vec::<String>::new()
        );
        assert_eq!(file.resolve_log(Some("".to_string())), "debug");
        assert_eq!(
            file.resolve_path_map(Some("".to_string()))
                .map(Path::new("/root/p")),
            PathBuf::from("/from/file/p")
        );
    }

    // ---- malformed input is surfaced, not swallowed ------------------------

    #[test]
    fn a_malformed_file_is_an_error_not_a_silent_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "[lsp]\nenabled = \"not-a-bool\"\n");
        assert!(ServerConfig::load(&path).is_err());
    }

    #[test]
    fn a_bad_transport_value_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "[server]\ntransport = \"htp\"\n");
        assert!(ServerConfig::load(&path).is_err());
    }

    #[test]
    fn a_misspelled_key_is_an_error_rather_than_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        // A setting that quietly did nothing is the failure mode this file
        // exists to avoid, so unknown keys are rejected at every level.
        for body in [
            "[server]\nalowed_hosts = [\"h\"]\n",
            "[lsp]\nenable = true\n",
            "[serverr]\ntransport = \"http\"\n",
        ] {
            let path = write(&dir, body);
            assert!(
                ServerConfig::load(&path).is_err(),
                "expected an error for: {body}"
            );
        }
    }
}
