//! The LSP access surface: a TCP listener that speaks the Language Server
//! Protocol over the same operation catalog as the MCP surface.
//!
//! A client reaches it through a generic stdio-to-TCP relay (e.g. `socat`), so
//! nothing Henka-specific runs on the client side. Each accepted connection is
//! one independent LSP session; they share the process-wide registries and
//! provider sessions, which are already request-serialized.

mod map;
mod session;

use crate::mcp::HenkaMcp;

/// Serve the LSP surface on `bind` until the process ends, one session per
/// accepted connection. Binding beyond loopback exposes every registered
/// project — the surface is unauthenticated, like the MCP transport.
pub async fn serve(handler: HenkaMcp, bind: &str) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "serving LSP over TCP");
    loop {
        // A transient accept error (a client aborting mid-handshake, momentary
        // fd exhaustion) must not tear down the listener for the rest of the
        // process — log it and keep accepting.
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "LSP accept failed; continuing to listen");
                continue;
            }
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            if let Err(error) = session::run(handler, stream).await {
                tracing::warn!(%error, %peer, "LSP session ended with an error");
            }
        });
    }
}
