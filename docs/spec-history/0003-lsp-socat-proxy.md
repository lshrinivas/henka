# LSP access surface — Spec

Change: `lsp-proxy` (jj `nlnvsupl`)

## 1. What this change is

Henka is reached entirely through MCP (SPEC.md §3). But the clients that would benefit most from
its semantic queries and refactorings — Claude Code, and editors generally — already speak the
**Language Server Protocol**. They know how to launch a language server as a subprocess and drive
it over stdio; they do not speak MCP. Bridging that gap has meant putting a Henka-specific
component on the client side to translate, which has to be built, shipped, and kept in step with
the server everywhere a client runs.

This change removes that need by giving Henka a **second application surface**: the server speaks
LSP directly, over a network port, in the same process that serves MCP and over the same projects
and operation catalog. A client reaches it with nothing Henka-specific of its own — any generic
byte relay (such as `socat`) forwards the client's stdio LSP channel to the port. All the
intelligence — the mapping from LSP requests to operations, project identity, edit handling —
lives in the server, versioned with the catalog it depends on.

The surface is additive. It introduces no new operations and changes no results; it exposes the
existing catalog (SPEC.md §8, and specs 0001–0002) through a second protocol. MCP is unchanged and
remains the primary surface.

## 2. The LSP surface

The LSP surface sits alongside the MCP surface (SPEC.md §3), served by the **same single server
process** (SPEC.md §2.1) over the same registered projects and the same catalog. It is reached over
LSP's JSON-RPC on a **TCP port distinct from the MCP port** — `8182` by default, the port after the
MCP default of `8181`.

The surface is **opt-in**: unless it is explicitly enabled (§9), the server behaves exactly as it
does today and does not listen for LSP at all. When enabled, the LSP listener and the MCP transport
run **concurrently in the one process** — a single Henka serves both at once, each on its own port.

## 3. Connecting a client

An LSP client launches its language server as a subprocess and talks to it over stdio. To reach
Henka's LSP port, the client's configured "language server" is a **thin relay** — a generic
stdio-to-TCP forwarder such as `socat` — that carries the client's stdio LSP channel to the port
and back. Nothing Henka-specific runs on the client side; the relay copies bytes, and LSP's
message framing passes through untouched.

The port is **unauthenticated** and grants the same access as MCP: reading and refactoring every
registered project. It therefore **binds loopback by default**, matching the MCP transport's
posture (SPEC.md §12). Binding it beyond loopback is an explicit operator choice that exposes every
project to anyone who can reach the port.

## 4. Projects and workspaces over LSP

LSP has no notion of a Henka project id; it presents workspace folders and file URIs. The surface
resolves these to the tenancy model (SPEC.md §2.2) so that no client-side configuration of ids is
needed:

- The **project** and an optional **workspace** are derived from the client's primary workspace
  folder: its directory name, split on the first `.`, gives the project id and — when present — a
  named working copy under it. A folder `stargate` names project `stargate`; a folder
  `stargate.rewind` names project `stargate` with working copy `rewind`.
- The base project must be registered, or auto-registered under a workspaces mount, exactly as for
  MCP; the named working copy selects which working copy an edit lands in, the same role the
  `workspace` argument plays over MCP (see `docs/deploying.md`).
- File URIs are resolved to filesystem paths and run through the server's existing path mapping, so
  a client that speaks its own (e.g. host) paths is understood without further configuration.

## 5. What the surface exposes

Each LSP request maps to one operation in the catalog. The mapping covers the semantic queries
(SPEC.md §8; specs 0001–0002) and the edit operations:

**Queries (read-only):**

- `textDocument/references` → **find-usages**
- `textDocument/definition` → **go-to-definition**
- `textDocument/implementation` → **find-implementations**
- `textDocument/hover` → **describe-symbol**
- `textDocument/documentSymbol` → **file-outline**
- `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`,
  `callHierarchy/outgoingCalls` → **call hierarchy**

**Edits:**

- `textDocument/prepareRename`, `textDocument/rename` → **rename**
- `textDocument/codeAction`, `codeAction/resolve` → any operation carrying a code-action kind
- `workspace/executeCommand` → **change-signature**, **move**, and other operations without a
  standard LSP request

A request the surface does not map to an operation is answered with an explicit "not supported"
result rather than a silent or malformed one (SPEC.md §11); an unmapped request never disrupts the
session.

## 6. Results carry their source

Navigation over this surface must preserve the enrichment established in spec 0002 §3.1: every
location a query returns carries **the source text it points at**, plus an optional surrounding
context window. A bare coordinate — file, line, column — leaves an agent no better off than a text
search, which already showed it the line; dropping the source when mapping a result into an LSP
response would forfeit the measured benefit that motivated the enrichment. The LSP surface
therefore conveys the enriched result, not a reduced one; how the enrichment is carried within LSP
responses is settled during implementation.

## 7. What the server reads

The server answers from the **working copy on disk together with its version-control overlay**
(SPEC.md §9), not from document buffers synchronized by the client. Buffer-synchronization
notifications from the client are accepted but are not the source of truth; unsaved in-editor
changes are out of scope for now. Positions follow LSP's default UTF-16 encoding, matching the
semantic backends Henka already drives.

## 8. Editing over LSP

Edit operations behave over the LSP surface exactly as they do over MCP (SPEC.md §7): they keep
Henka's **preview-before-apply** posture, and the **server applies the edit to the working tree** —
it does not hand back an unapplied edit for the client to apply itself. The client receives the
summary and diff of what changed, the same currency an MCP caller receives. This keeps a single
edit model across both surfaces and preserves the principles that every edit can be previewed
before it touches disk and that the server operates while the project's own version control records
history (SPEC.md §13). An operation that cannot be performed safely fails with a clear reason and
changes nothing (SPEC.md §11).

## 9. Configuration & operation

This surface extends SPEC.md §12.

- The LSP surface is **enabled by a command-line flag**, and the same settings — whether it is
  enabled, and the address and port it binds — may instead be given in a **server configuration
  file**. That file, `henka.toml`, is **separate from the project registry** (`projects.toml`): the
  registry is rewritten whenever a project is registered, so server settings live apart from it
  where registrations cannot disturb them, and the command line does not have to grow a flag for
  every setting.
- Where a setting is given in more than one place, the **command line wins over the configuration
  file, which wins over the built-in default**. The default is the loopback address on port `8182`,
  with the surface disabled until opted in.
- The LSP and MCP surfaces are served by **one process**; enabling LSP does not change how MCP is
  configured or served.
- The surface performs **no authentication**. Its trust model is identical to the MCP HTTP
  transport: safe on loopback, and exposed to whoever can reach the port when bound wider — a
  choice left to the operator, made explicit in the flag's description.

## 10. What stays the same

- **One catalog, more than one surface.** The LSP surface adds no operations and alters no results;
  it presents the existing catalog through a second protocol.
- **Semantics over text, preview before harm, the server operates but does not version** (SPEC.md
  §13) hold identically on the LSP surface.
- **MCP is unchanged.** It remains the primary surface; a deployment that does not enable LSP is
  indistinguishable from today's.
