# LSP Proxy for Claude Code ↔ Henka

Plan for a thin LSP server that Claude Code loads as a plugin and that
translates each supported LSP request into an MCP `tools/call` on a long-lived
`henka-server` instance.

## 1. Goals and non-goals

**Goals**
- Expose every henka operation as an LSP-shaped method, so any LSP-capable
  agent (Claude Code today, others tomorrow) can reach the full henka
  catalog through a single well-known protocol.
- Reuse a single long-lived henka daemon so the warm index survives across
  Claude Code sessions and across jj workspace switches.
- Correctly target jj workspaces mounted inside the dev container without
  requiring per-workspace project registrations in henka.
- Advertise per-session only the operations henka actually offers for the
  current project (queried via `list_operations` at startup).

**Non-goals**
- Full LSP conformance. Hover, completion, diagnostics, semantic tokens,
  inlay hints, formatting, code-lens, document/workspace symbol are out —
  henka has no ops behind them and stubbing them adds no value.
- Running henka in-process. Henka stays a separate daemon.
- Optimizing for Claude Code's specific call pattern. The surface is
  driven by henka's op catalog, not by what any one client happens to
  invoke.
- **Mutating henka's project registry.** The proxy must never call
  `register_project` or `unregister_project`. Project registration is
  an explicit host-side operator action performed once against
  henka-server, and the proxy treats the registry as read-only (see §3).

## 2. Deployment topology

Henka runs on the **host machine**, not inside the dev container. The
dev container is where Claude Code + the LSP proxy live; it reaches
henka over the host boundary.

```
┌──── host (macOS) ───────────────────────────────────────────────────┐
│                                                                     │
│  henka-server  ──▶ warm indexes (per repo)                          │
│      ▲                                                              │
│      │  MCP over HTTP  (host.docker.internal:8181)                  │
│      │                                                              │
│  ┌───┼───────────────── dev container ─────────────────────────┐    │
│  │   │                                                         │    │
│  │  lsp-proxy  ◀── stdio ──  Claude Code                       │    │
│  │                                                             │    │
│  │  bind mounts:                                               │    │
│  │    host  /Users/me/Projects/stargate       → /root/stargate │    │
│  │    host  /Users/me/Projects/stargate.foo   → /root/stargate.foo │
│  └─────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────┘
```

- `henka-server` runs on the host with `--transport http` (e.g.
  `henka-server --transport http --bind 127.0.0.1:8181`). One process
  per host, lifetime independent of dev containers or Claude Code
  sessions. Because the connection crosses the container boundary,
  either bind to an interface the container can reach or use
  `host.docker.internal` from inside the container; pass
  `--allowed-host host.docker.internal` so henka's DNS-rebinding guard
  accepts the connection.
- `lsp-proxy` is the Claude Code plugin binary. Runs inside the dev
  container, one instance per Claude Code session. Claude Code owns
  its lifecycle.
- The base repo (e.g. `stargate`) is the only henka project
  registration, registered by its **host** path
  (`/Users/me/Projects/stargate`). jj workspaces are not registered.

## 3. Path model (critical)

Paths exist on two sides of the container boundary and henka only sees
the host side. The proxy sends host paths on every call.

### The layout

```
host                                        dev container
────                                        ─────────────
/Users/me/Projects/stargate           ⇄     /root/stargate
/Users/me/Projects/stargate.foo       ⇄     /root/stargate.foo
```

- Naming convention: `<repo>` for the base, `<repo>.<workspace>` for a
  jj workspace of that repo.
- Only `<repo>` is registered with henka, using its host path.
- The container mounts each host directory 1:1 under `/root`.

### Translating paths

The proxy has to bridge the boundary: LSP requests arriving from Claude
Code carry container paths (`file:///root/stargate.foo/...`), but henka
needs host paths. Two options:

- **(preferred) Let henka do the rewrite via `HENKA_PATH_MAP`.** Set
  `HENKA_PATH_MAP=/root=/Users/me/Projects` on `henka-server`. Then the
  proxy sends the container-native paths it already has and henka
  translates on the way in (`crates/henka-server/src/pathmap.rs`).
  Cleanest — the proxy stays boundary-agnostic and there's one place
  to configure the mapping.
- **(fallback) Translate in the proxy.** If `HENKA_PATH_MAP` can't be
  set on henka (e.g. it's already used for something else), the proxy
  translates every path from `/root/...` to `<host-projects>/...`
  before sending, driven by a `HENKA_HOST_ROOT` env var passed into
  the dev container.

Assume the preferred path in the rest of this doc.

### Resolving the current workspace

At `initialize`, take `workspaceFolders[0].uri` (fall back to
`rootUri`). Strip the `file://` prefix to get an absolute container
path — call this `workspace_path`. Typical values:
`/root/stargate`, `/root/stargate.foo`.

### Resolving the henka project id

The base repo is registered with henka **once**, out of band, by the
operator running `register_project` against henka-server on the host
with the host path (e.g. `/Users/me/Projects/stargate`). The proxy
treats the registry as read-only:

> **The proxy must NEVER call `register_project` or
> `unregister_project`.** Neither at startup, nor lazily on a missing
> project, nor as a "self-healing" step. If the derived id is not
> registered, that is an operator-fixable configuration error and the
> proxy surfaces it as such. Auto-registering would (a) hide setup
> mistakes, (b) create duplicate registrations keyed off container
> paths that don't exist on the host anyway, and (c) mutate shared
> state that other clients depend on.

Given `workspace_path` with basename `<name>`:
1. Split `<name>` on the first `.` — if two parts, the left is the
   repo, the right is the jj workspace name. If no `.`, the whole
   thing is the repo name and there is no jj workspace overlay.
2. The henka project id is the repo part (lowercased, matching henka's
   `derive_id` slug rule — see `crates/henka-core/src/registry.rs`).
3. Verify with `tools/call list_projects` at startup; if the derived
   id is not registered, log an actionable error naming the id and
   the expected host path, and stay up. Every subsequent tool call
   will fail with a clear "project not registered — run
   `register_project` on henka-server" message rather than silently
   misrouting or auto-registering.

### Per-request payload shape

Every `tools/call` to henka carries:

```jsonc
{
  "project":  "<derived repo id>",       // e.g. "stargate"
  "workspace": "/root/stargate.foo",     // container path — henka rewrites via HENKA_PATH_MAP
  "target":   { "file": "...", ... },    // per-op, also a container path
  // op-specific params
}
```

`workspace` MUST be sent unconditionally, even when the user is on the
default workspace (`/root/stargate`, no dot suffix) — henka accepts
that too and it removes a special case in the proxy. See
`crates/henka-server/src/mcp.rs:523-539` for how henka resolves
`workspace`; `crates/henka-server/src/pathmap.rs` handles the host↔
container rewrite.

### Verification at startup

After `initialize`, call `tools/call project_status { id }` once. Log
`revision`, `changed_files`, and `digest`. Compare `revision` against
a local `jj log -r @ -T change_id` (or `git rev-parse HEAD` fallback)
run in `workspace_path`. If they diverge, log a warning — this is the
"henka sees a stale tree" failure mode and it's worth surfacing early.
Do not fail startup: dirty tree divergence is normal and expected.

## 4. Supported LSP surface

Every henka operation gets an LSP method. The mapping falls into three
buckets: ops that have a standard LSP method, ops that fit LSP's
`textDocument/codeAction` refactor slot, and ops with no LSP-standard
shape that live under `workspace/executeCommand`.

Verified against
`crates/henka-lang-{java,rust,ts}/src/operations.rs`:

| Henka op            | LSP method                     | Bucket        | Langs      |
|---------------------|--------------------------------|---------------|------------|
| `find-usages`       | `textDocument/references`      | standard      | java, rust, ts |
| `rename`            | `textDocument/rename` (+ `prepareRename`) | standard | java, rust, ts |
| `extract-variable`  | `textDocument/codeAction` (`refactor.extract.variable`)  | code-action | java |
| `extract-constant`  | `textDocument/codeAction` (`refactor.extract.constant`)  | code-action | java |
| `extract-field`     | `textDocument/codeAction` (`refactor.extract.field`)     | code-action | java |
| `extract-method`    | `textDocument/codeAction` (`refactor.extract.function`)  | code-action | java |
| `inline`            | `textDocument/codeAction` (`refactor.inline`)            | code-action | java |
| `organize-imports`  | `textDocument/codeAction` (`source.organizeImports`)     | code-action | java |
| `change-signature`  | `workspace/executeCommand henka.change-signature`        | command     | java |
| `move`              | `workspace/executeCommand henka.move`                    | command     | java |

The six `code-action` ops all come from `CodeActionOp::java_set()`
(`crates/henka-lang-java/src/operations.rs:146-196`) and already carry
LSP-standard `CodeActionKind` values — no client-side translation
needed. `change-signature` and `move` are separate top-level ops
(`ChangeSignatureOp`, `MoveOp` in `crates/henka-lang-java/src/provider.rs:96-104`),
have no matching `CodeActionKind`, and take structured parameters, so
they live under `executeCommand`.

Tenancy tools (`register_project`, `unregister_project`, `list_projects`,
`project_status`, `list_operations`) stay MCP-only — they're
administrative and have no LSP shape. The proxy uses them internally at
startup.

### Capability advertisement (dynamic, per project)

At `initialize` time, after resolving the project id (§3), call
`tools/call list_operations { project: <id> }` and advertise only the
matching capabilities:

- If the catalog contains `find-usages` → `referencesProvider: true`.
- If it contains `rename` → `renameProvider: { prepareProvider: true }`.
- If it contains any `code-action` descriptor → `codeActionProvider:
  { codeActionKinds: [...], resolveProvider: true }` with the kinds
  drawn from the descriptors.
- If it contains `change-signature` or `move` → include them in
  `executeCommandProvider.commands` as `henka.<op-id>`.

This keeps the surface honest per language: a Rust-only project won't
falsely advertise code actions or `change-signature`.

### LSP methods intentionally not implemented

`definitionProvider`, `documentSymbolProvider`, `workspaceSymbolProvider`,
`hoverProvider`, `completionProvider`, `signatureHelpProvider`,
`typeDefinitionProvider`, `implementationProvider`,
`documentFormattingProvider`, and everything else — henka has no ops for
them. The proxy returns "method not found" per LSP spec; do not stub.

### Lifecycle / sync

`textDocument/didOpen` / `didChange` / `didClose` / `didSave` are
received but effectively no-ops: henka reads content from disk in the
workspace path and overlays uncommitted changes via `jj diff` on each
request. The proxy tracks open documents only for its own request-id /
cancellation bookkeeping.

## 5. Method-by-method translation

### `textDocument/references` → `find-usages`

- Input: `{ textDocument.uri, position: { line, character }, context.includeDeclaration }`.
- Translate URI → absolute path (strip `file://`, percent-decode).
- Call:
  ```jsonc
  tools/call find-usages {
    "project":  "<id>",
    "workspace": "<workspace_path>",
    "target":   { "file": "<abs path>", "line": <line>, "character": <char> }
  }
  ```
- Confirm LSP is 0-based (line, character) — henka's `Position` is also
  0-based (verify in `crates/henka-core` before wiring).
- Confirm character encoding: LSP defaults to UTF-16 code units.
  Check `initialize.capabilities.general.positionEncodings` — if the
  client offers UTF-8 or UTF-32, negotiate. If it insists on UTF-16 and
  henka uses UTF-8 offsets, either transcode in the proxy or refuse
  UTF-16-only clients with a clear error. This is the single biggest
  correctness risk — do not skip.
- Response: map henka's JSON usages → `Location[]` (URI = `file://` +
  absolute workspace-relative path).
- Respect `includeDeclaration` if henka doesn't already filter it.

### `textDocument/rename` → `rename` (dry-run)

- Input: `{ textDocument.uri, position, newName }`.
- Call `tools/call rename { project, workspace, target, newName, dry_run: true }`.
- Response: convert henka's returned `files` preview (list of edits per file)
  into an LSP `WorkspaceEdit { changes: { uri: TextEdit[] } }`.
- **Do not apply the edit in the proxy.** Return the `WorkspaceEdit` and let
  Claude Code / the editor apply it. Henka's `dry_run: true` path is exactly
  for this — see `crates/henka-server/src/mcp.rs:503-506`.

### `textDocument/prepareRename`

- Cheapest impl: return `{ defaultBehavior: true }` — the client uses
  the identifier at the cursor. Ship this as the default; henka has no
  dedicated validation op, so a richer impl would just re-invoke
  `rename` speculatively.

### `textDocument/codeAction` → Java code-action set

- Input: `{ textDocument.uri, range, context.diagnostics, context.only }`.
- The Java provider exposes a set of code actions via `CodeActionOp` —
  see `crates/henka-lang-java/src/operations.rs` and the
  `CodeActionOp::java_set()` call in `provider.rs`.
- Query strategy:
  1. Cache the code-action descriptors from `list_operations` at startup.
  2. On `codeAction`, filter by `context.only` (LSP `CodeActionKind`
     prefixes) against the descriptors' kinds.
  3. For each matching descriptor, return a lightweight `CodeAction`
     entry with `data` carrying enough to resolve later: descriptor id,
     project, workspace, target file, range.
- Do **not** compute the edit here. Real work happens in `codeAction/resolve`.

### `codeAction/resolve`

- Input: the `CodeAction` the client selected, with our `data` blob.
- Call `tools/call <descriptor-id> { project, workspace, target, ...params, dry_run: true }`.
- Fill in `edit` on the `CodeAction` from henka's returned `files`
  preview, translated to `WorkspaceEdit`. Return.

### `workspace/executeCommand henka.change-signature`

- Only advertised for Java projects.
- Arguments (LSP `ExecuteCommandParams.arguments`): a single JSON object
  matching henka's `change-signature` params — target coordinate plus the
  new signature spec. Read the descriptor's JSON schema
  (`list_operations`) and pass args straight through, no per-field
  translation.
- Call with `dry_run: true`. Return the resulting `WorkspaceEdit` as the
  command result, and additionally send `workspace/applyEdit` to the
  client so an editor-driven flow works. (Claude Code / other agents
  that consume the result directly will use the returned object; a
  human-driven editor will use the applyEdit round-trip.)

### `workspace/executeCommand henka.move`

- Only advertised for Java projects.
- Same pattern as `change-signature`: pass args through, `dry_run: true`,
  return `WorkspaceEdit`, also send `workspace/applyEdit`.

## 6. Startup, session, and error handling

### Startup
1. Read env: `HENKA_URL` (typically
   `http://host.docker.internal:8181/mcp` in the dev container).
2. Read stdin/stdout for LSP. Log to stderr.
3. On `initialize`: open MCP HTTP session to henka on the host. Send
   `initialize` + `notifications/initialized` on the MCP side. Surface
   connection failures as an actionable LSP error (`internalError`
   with a message that mentions `HENKA_URL`, `HENKA_MCP_ALLOWED_HOST`,
   and that henka runs on the host) — do not crash the proxy.
4. Cache `tools/list` and `list_operations` for the derived project.
5. Return LSP `InitializeResult` with the narrow capability set from §4.

### Per-request
- Every LSP request has a matching MCP call. No batching, no speculative work.
- Propagate LSP `$/cancelRequest` to MCP by dropping the pending future
  (henka doesn't currently support MCP-side cancellation of a running
  operation; note as a known limitation).
- On MCP error, translate to LSP error response with the henka message
  verbatim in `.message`. Never crash the proxy — a failing tool call is
  a request-scoped failure.

### Shutdown
- LSP `shutdown` → drain in-flight, close MCP session politely.
- LSP `exit` → process exits 0.
- SIGTERM / SIGHUP → same as `exit`.

## 7. Configuration

### Proxy (runs in the dev container)

- `HENKA_URL` — MCP endpoint on the host. Typical:
  `http://host.docker.internal:8181/mcp`. Default falls back to
  `http://127.0.0.1:8181/mcp` (only useful when henka is
  co-located, e.g. for local testing).
- `HENKA_PROXY_LOG` — `tracing` filter, default `info`. Logs to stderr.
- `HENKA_PROXY_PROJECT` — override the derived project id (escape hatch
  for a repo whose directory name doesn't match its registered id).
- `HENKA_HOST_ROOT` — only used if the proxy translates paths itself
  (fallback path in §3). Absolute host directory that the container's
  `/root` mount corresponds to (e.g. `/Users/me/Projects`). Leave
  unset when relying on henka-side `HENKA_PATH_MAP`.

### Henka-server (runs on the host)

Not the proxy's concern in general, but two settings are load-bearing
for this topology:

- `HENKA_PATH_MAP=/root=/Users/me/Projects` — rewrite the container's
  `/root/...` paths onto the host's `/Users/me/Projects/...` paths
  before henka touches the filesystem.
- `--allowed-host host.docker.internal` (or `HENKA_MCP_ALLOWED_HOST`)
  — accept the `Host` header the container sends. Without this, the
  streamable-HTTP DNS-rebinding guard rejects the connection.

Bind explicitly. `--bind 127.0.0.1:8181` only accepts host-local
connections; for a container to reach it, either bind to an interface
the container's default route can reach (e.g. `--bind 0.0.0.0:8181`
inside a private network) or use Docker's `host.docker.internal`
route. Do **not** expose henka on a public interface — it is
unauthenticated.

## 8. Implementation

- **Language:** Rust.
- **Deps:**
  - `tower-lsp-server` (or `tower-lsp`) — LSP server framework.
  - `lsp-types` — LSP schema.
  - `rmcp` with `client` + `transport-streamable-http-client` features —
    same crate henka-server uses, pinned to henka's version to avoid
    dialect drift.
  - `tokio` (`rt-multi-thread`, `macros`).
  - `serde`, `serde_json`, `url`, `percent-encoding`.
  - `tracing`, `tracing-subscriber`.
- **Location:** new crate `crates/henka-lsp-proxy` in this workspace.
- **Binary name:** `henka-lsp-proxy`.
- **Optional dep on `henka-core`** to reuse `Position` / `Range` /
  `Target` types where they cross the wire, reducing the risk of
  parameter-shape drift.

## 9. Testing

- **Unit tests** in the proxy crate:
  - Workspace-path → project-id derivation (`/root/stargate`,
    `/root/stargate.feature1`, `/root/stargate.foo.bar` edge case,
    non-standard mount paths).
  - LSP `Position` ↔ henka `Position` (with UTF-16 negotiation).
  - `WorkspaceEdit` construction from a synthetic henka `files` preview.
- **Integration test**: spin up `henka-server --transport http` on a
  random port against a fixture repo, drive the proxy over stdio with
  canned LSP messages, assert on the returned `Location[]` and
  `WorkspaceEdit`. Model on `crates/henka-lang-java/tests/workspaces.rs`.
- **Manual smoke test**: run inside the actual dev container against
  `/root/stargate` and a `/root/stargate.feature1` workspace with local
  edits; verify `find-usages` sees the workspace's edits and `rename`
  produces edits scoped to the workspace paths.

## 10. Open questions (resolve before coding)

1. **Position encoding.** Confirm henka's `Position` semantics (byte
   offset vs. UTF-16 code unit vs. character). If they differ from what
   the LSP client sends, decide where the transcode lives (proxy vs.
   henka). This is the single biggest correctness risk.
2. **Code-action kind in the descriptor.** `CodeActionOp` currently
   stores `action_kind` as an internal field (`operations.rs:111`); the
   `OperationDescriptor` returned by `descriptor()` does not carry it
   yet (see `operations.rs:220-231`). The proxy needs the kind to
   populate `codeActionProvider.codeActionKinds` and to filter by
   `context.only`. Either (a) add `kind` to `OperationDescriptor` for
   code-action ops, or (b) have the proxy maintain a hardcoded id→kind
   table matching the six entries in `java_set()`. (a) is cleaner and
   avoids drift; do it in henka before starting the proxy.
3. **`workspace/executeCommand` result vs. `applyEdit`.** Decide which
   flow is primary. Recommend: return the `WorkspaceEdit` as the
   command result *and* send `workspace/applyEdit` — costs nothing and
   supports both agent-driven and editor-driven consumers.
4. **Cancellation.** Confirm henka's behavior for a dropped MCP request
   mid-operation; if it keeps running to completion, document as a
   known limitation.
5. **Multi-workspace.** If a session opens multiple `workspaceFolders`,
   the proxy currently picks `[0]`. Decide whether to support N>1 or
   reject.
6. **Project not registered.** Auto-registration is explicitly off
   the table (see §1 non-goals, §3). Remaining choice: (a) log and
   fail subsequent requests with an actionable message, or (b) fail
   `initialize` outright so Claude Code sees the plugin as
   unavailable. Recommend (a) — the LSP session stays up, the error
   surfaces per-request with the exact `register_project` command to
   run on the host.
