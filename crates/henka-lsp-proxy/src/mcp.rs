//! Thin async wrapper around Henka's MCP HTTP endpoint.
//!
//! The proxy speaks MCP over streamable HTTP to a single long-lived
//! `henka-server` (typically on the host, reached via `host.docker.internal`
//! from inside the dev container). Each LSP request becomes one `tools/call`
//! here.

use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, CallToolResult, RawContent};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{Peer, ServiceExt};
use serde_json::{Map, Value};

/// The MCP tool-call error surfaced to callers.
#[derive(Debug, thiserror::Error)]
pub enum McpClientError {
    /// The MCP session couldn't be established (transport / handshake failure).
    #[error("cannot connect to henka at {url}: {source}. Check HENKA_URL, that \
             henka-server is running on the host, and that \
             --allowed-host (HENKA_MCP_ALLOWED_HOST) accepts the container's Host header.")]
    Connect {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The MCP call itself failed (network dropped, protocol error, ...).
    #[error("MCP call `{tool}` failed: {source}")]
    Call {
        tool: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The tool reported an error via `CallToolResult.is_error`.
    #[error("MCP tool `{tool}` returned an error: {message}")]
    ToolError { tool: String, message: String },
    /// The tool returned no content, or content the wrapper doesn't understand.
    #[error("MCP tool `{tool}` returned no text content")]
    EmptyResult { tool: String },
    /// The tool's returned JSON couldn't be parsed.
    #[error("MCP tool `{tool}` returned malformed JSON: {source}")]
    BadJson {
        tool: String,
        #[source]
        source: serde_json::Error,
    },
}

/// A connected MCP client, wrapping the running `rmcp` service.
pub struct McpClient {
    service: Arc<RunningService<RoleClient, ()>>,
}

impl McpClient {
    /// Open an MCP session to `url` (Henka's streamable-http endpoint).
    ///
    /// The connection is opened lazily on first request by rmcp; we still
    /// perform the `initialize` handshake here so a startup misconfiguration
    /// (wrong URL, DNS-rebinding rejection) surfaces immediately.
    pub async fn connect(url: &str) -> Result<Self, McpClientError> {
        let transport = StreamableHttpClientTransport::from_uri(url);
        let service = ()
            .serve(transport)
            .await
            .map_err(|e| McpClientError::Connect {
                url: url.to_string(),
                source: Box::new(e),
            })?;
        Ok(Self {
            service: Arc::new(service),
        })
    }

    /// Peer handle used for issuing tool calls.
    fn peer(&self) -> &Peer<RoleClient> {
        self.service.peer()
    }

    /// Invoke an MCP tool and return the parsed JSON body.
    pub async fn call_tool(
        &self,
        name: &str,
        args: Value,
    ) -> Result<Value, McpClientError> {
        let arguments = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => Some(
                once_pair("_", other)
                    .into_iter()
                    .collect::<Map<String, Value>>(),
            ),
        };
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let result = self
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| McpClientError::Call {
                tool: name.to_string(),
                source: Box::new(e),
            })?;
        parse_result(name, result)
    }

    /// Close the MCP session (used in shutdown).
    pub async fn close(self) {
        // `cancel` consumes the service; a join error here means henka went
        // away, which is fine at shutdown.
        if let Ok(service) = Arc::try_unwrap(self.service) {
            let _ = service.cancel().await;
        }
    }
}

/// Turn a `CallToolResult` into the JSON body Henka encoded into its first
/// text content block.
fn parse_result(tool: &str, result: CallToolResult) -> Result<Value, McpClientError> {
    if let Some(text) = result
        .content
        .iter()
        .find_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
    {
        if result.is_error == Some(true) {
            return Err(McpClientError::ToolError {
                tool: tool.into(),
                message: text,
            });
        }
        return serde_json::from_str(&text).map_err(|source| McpClientError::BadJson {
            tool: tool.into(),
            source,
        });
    }
    if let Some(structured) = result.structured_content {
        return Ok(structured);
    }
    Err(McpClientError::EmptyResult { tool: tool.into() })
}

fn once_pair(k: &str, v: Value) -> Vec<(String, Value)> {
    vec![(k.to_string(), v)]
}
