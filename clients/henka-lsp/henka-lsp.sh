#!/usr/bin/env sh
# Bridge an editor's stdio LSP channel to Henka's LSP listener over TCP.
#
# Henka serves the Language Server Protocol natively on a port (see
# `--lsp` / `--lsp-bind`). This relay is all a client needs — no Henka-specific
# code runs here; socat just forwards bytes, and the LSP message framing passes
# through untouched. Point HENKA_LSP_ADDR at the listener (host:port).
#
# In an editor's LSP config, set the server command to this script, e.g. the
# accompanying `.lsp.json`.
set -eu

addr="${HENKA_LSP_ADDR:-host.docker.internal:8182}"
host="${addr%:*}"
port="${addr##*:}"

# -T bounds an idle connection so a dropped client doesn't leak a half-open
# socket on the server; the editor relaunches this command when it reconnects.
exec socat -T"${HENKA_LSP_IDLE_TIMEOUT:-3600}" STDIO "TCP:${host}:${port}"
