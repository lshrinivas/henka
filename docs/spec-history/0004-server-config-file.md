# Server configuration file — Spec

Change: `server-config-file` (proposed; not yet implemented)

## 1. What this change is

Every server setting today — transport, HTTP bind address, allowed hosts, the LSP surface and its
bind address, the caller-path map, the log filter — is a command-line flag or an environment
variable, supplied fresh on every invocation. That works for a one-off run, but it means a fixed
deployment's whole startup has to be retyped, or re-templated into a launcher script, every time
the server starts:

```
HENKA_LOG=debug HENKA_PATH_MAP=/root=/Users/me/Projects \
  henka --transport http --allowed-host host.docker.internal --lsp
```

This change gives Henka a **server configuration file**, `henka.toml`, so an operator can write a
deployment's settings down once and start the server with none of the above repeated. The file is
**deliberately separate from the project registry** (`projects.toml`): the registry is rewritten
whenever a project is registered, so operator settings need a file of their own where a
registration can never disturb them, and the command line does not have to grow a flag for every
setting it might otherwise hold.

The file is **optional and additive**. A deployment that never creates it is unaffected: every
setting still has its command-line flag or environment variable, and the built-in defaults are
unchanged. `henka.toml` is a fourth, lowest-precedence way to supply a value that a flag or
environment variable could also supply — never a new way to configure something that couldn't be
configured before.

## 2. Settings the file covers

Each key mirrors an existing CLI flag or environment variable, with the same meaning:

| `henka.toml` key | Equivalent today |
|---|---|
| `[server] transport` | `--transport <stdio\|http>` |
| `[server] bind` | `--bind <ADDR>` (used only when `transport = "http"`) |
| `[server] allowed_hosts` | `--allowed-host <HOST>` (repeatable) / `HENKA_MCP_ALLOWED_HOST` (space-separated) |
| `[server] log` | `HENKA_LOG` (tracing `EnvFilter` syntax) |
| `[path_map]` | `HENKA_PATH_MAP=<host-prefix>=<container-prefix>` |
| `[lsp] enabled` | `--lsp` |
| `[lsp] bind` | `--lsp-bind` / `HENKA_LSP_BIND` |

`[path_map]` is a table, generalizing the single `prefix=prefix` pair `HENKA_PATH_MAP` carries to
any number of mappings, each entry equivalent to one `HENKA_PATH_MAP` value:

```toml
[path_map]
"/root" = "/Users/me/Projects"
```

`allowed_hosts` is an array, equivalent to repeating `--allowed-host` or space-separating
`HENKA_MCP_ALLOWED_HOST`. A full file for the deployment above looks like:

```toml
[server]
transport = "http"
bind = "127.0.0.1:8181"
allowed_hosts = ["host.docker.internal"]
log = "debug"

[path_map]
"/root" = "/Users/me/Projects"

[lsp]
enabled = true
```

With this file in place, the operator's invocation reduces to `henka` — no flags, no environment
variables — while `henka --transport http --allowed-host other.example` still overrides the file's
`transport`/`allowed_hosts` for a one-off run.

## 3. Precedence

Settings resolve with a fixed precedence, highest first:

```
command line  >  environment variable  >  file  >  built-in default
```

For settings that have no environment-variable form — today, only `--lsp`'s `enabled` flag — the
ordering collapses to `command line > file > default`. For settings that have no command-line
form — `log` and `path_map` — it collapses to `environment variable > file > default`. Each
setting follows the analogous shape for whichever inputs actually exist for it; no setting gains an
input form it didn't already have.

A **malformed file is an error, not a silent fallback**: a typo in an operator's config must be
surfaced, not swallowed into a default. An **absent file, or an absent section within it**, simply
leaves every affected setting exactly where the command line and environment already put it — the
file only ever narrows the gap between "nothing configured" and "everything configured," never
introduces a surprise when it's missing.

## 4. File location

The file's path resolves, in order:

1. `$HENKA_SERVER_CONFIG`, if set.
2. `$HENKA_DATA/henka.toml`, if `$HENKA_DATA` is set — the same root the project registry uses for
   `projects.toml`, so a deployment that already points `HENKA_DATA` somewhere gets both files
   there without further configuration.
3. `$XDG_CONFIG_HOME/henka/henka.toml`, else `$HOME/.config/henka/henka.toml` — the conventional
   per-user config location on a system with no `HENKA_DATA` set.

## 5. What stays the same

- **Every existing flag and environment variable keeps working, unmodified**, at the same
  precedence relative to each other. The file adds a lower-precedence source underneath them; it
  removes nothing.
- **`projects.toml` (the project registry) is untouched** and remains the file that registration
  rewrites; `henka.toml` is hand-edited operator settings only, never written to by the server
  itself.
- **No new settings are introduced.** Every key this file exposes already exists as a flag or
  environment variable; the file gives it an additional, persistent way to be set — not new
  behavior.

## 6. Out of scope

- Reloading the file without restarting the server.
- Per-project overrides of these settings (they are process-wide, matching their current CLI/env
  form).
- Any setting that does not already have a CLI flag or environment variable — this change gives
  the file coverage of existing knobs; it does not audit whether new knobs are needed.
