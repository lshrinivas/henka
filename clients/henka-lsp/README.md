# Henka LSP client bridge

Henka serves the Language Server Protocol natively on a port, alongside its MCP
surface. An editor reaches it through a generic stdio-to-TCP relay — nothing
Henka-specific runs on the client side.

- `henka-lsp.sh` — the relay. It execs `socat STDIO TCP:$HENKA_LSP_ADDR`, so the
  editor's stdio LSP channel is forwarded to Henka's LSP listener. Requires
  `socat` on `PATH` (`apt-get install -y socat`; present in most devcontainer
  bases).
- `.lsp.json` — an editor plugin declaration that runs the relay, with
  `HENKA_LSP_ADDR` pointing at the listener.

## Setup

1. On the server, enable the LSP surface: run Henka with `--lsp` (and, if you
   want a non-default address, `--lsp-bind <addr>`), or set `[lsp] enabled = true`
   in `henka.toml`. The default listener address is `127.0.0.1:8182`.
2. Put `henka-lsp.sh` on the client's `PATH` and install `socat`.
3. Drop `.lsp.json` where the editor looks for it, and set `HENKA_LSP_ADDR` for
   the deployment:
   - containerized client reaching a server on the host:
     `host.docker.internal:8182` (the default);
   - a server on the same host as a native client: `localhost:8182`.

The port in `HENKA_LSP_ADDR` must match the server's `--lsp-bind` port.

See `docs/deploying.md` for the server side and the `henka-lsp` skill for how a
project's identity is derived and how to diagnose problems.
