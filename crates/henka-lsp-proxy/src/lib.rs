//! LSP proxy translating Language Server Protocol requests into MCP `tools/call`
//! invocations against a long-lived `henka-server`.
//!
//! Only the LSP surface backed by an actual Henka operation is implemented —
//! see `docs/lsp-proxy-plan.md`. Every request has one matching MCP call; no
//! batching, no speculative work. Paths carried over the wire are the paths the
//! LSP client sends (container-side when the proxy runs in a dev container);
//! Henka rewrites them through `HENKA_PATH_MAP` on the way in and back to the
//! caller's namespace on the way out, so the proxy never has to know the
//! mapping itself.

pub mod backend;
pub mod config;
pub mod documents;
pub mod mcp;
pub mod project;
pub mod session;
