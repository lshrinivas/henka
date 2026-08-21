//! Thin async wrapper around Henka's MCP HTTP endpoint.
//!
//! The proxy speaks MCP over streamable HTTP to a single long-lived
//! `henka-server` (typically on the host, reached via `host.docker.internal`
//! from inside the dev container). Each LSP request becomes one `tools/call`
//! here.

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
    service: RunningService<RoleClient, ()>,
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
        Ok(Self { service })
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

    /// Cancel the MCP session, closing the transport.
    ///
    /// Takes `&self`: the client is owned by the shared session for the life of
    /// the LSP connection and can't be moved out of it at shutdown, so this
    /// goes through the service's cancellation token rather than
    /// `RunningService::cancel`, which consumes the service.
    pub fn cancel(&self) {
        self.service.cancellation_token().cancel();
    }
}

/// Turn a `CallToolResult` into the JSON body Henka encoded into its first
/// text content block.
fn parse_result(tool: &str, result: CallToolResult) -> Result<Value, McpClientError> {
    let text = result.content.iter().find_map(|c| match &c.raw {
        RawContent::Text(t) => Some(t.text.clone()),
        _ => None,
    });

    // An error result is an error whatever it carries — checking this only on
    // the text-content path would let a structured-only failure through as a
    // successful response body.
    if result.is_error == Some(true) {
        return Err(McpClientError::ToolError {
            tool: tool.into(),
            message: text.or_else(|| result.structured_content.map(|v| v.to_string()))
                .unwrap_or_else(|| "no message".into()),
        });
    }

    if let Some(text) = text {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;
    use serde_json::json;

    #[test]
    fn text_content_is_parsed_as_json() {
        let result = CallToolResult::success(vec![Content::text(r#"{"count":2}"#)]);
        let value = parse_result("find-usages", result).unwrap();
        assert_eq!(value, json!({ "count": 2 }));
    }

    #[test]
    fn text_error_result_carries_the_message() {
        let result = CallToolResult::error(vec![Content::text("project `p` is not registered")]);
        let err = parse_result("rename", result).unwrap_err();
        assert!(
            matches!(err, McpClientError::ToolError { .. }),
            "expected a tool error, got {err:?}"
        );
        assert!(err.to_string().contains("not registered"), "got: {err}");
    }

    #[test]
    fn structured_only_error_result_is_still_an_error() {
        // A failure that carries no text block must not fall through to the
        // structured-content path and come back as a successful response.
        let mut result = CallToolResult::structured(json!({ "message": "no such project" }));
        result.is_error = Some(true);
        let err = parse_result("rename", result).unwrap_err();
        assert!(
            matches!(err, McpClientError::ToolError { .. }),
            "expected a tool error, got {err:?}"
        );
        assert!(err.to_string().contains("no such project"), "got: {err}");
    }

    #[test]
    fn structured_success_is_returned_as_is() {
        let result = CallToolResult::structured(json!({ "count": 0 }));
        assert_eq!(
            parse_result("find-usages", result).unwrap(),
            json!({ "count": 0 })
        );
    }

    #[test]
    fn malformed_text_json_is_reported() {
        let result = CallToolResult::success(vec![Content::text("not json")]);
        let err = parse_result("rename", result).unwrap_err();
        assert!(matches!(err, McpClientError::BadJson { .. }), "got {err:?}");
    }

    #[test]
    fn empty_result_is_reported() {
        let err = parse_result("rename", CallToolResult::success(vec![])).unwrap_err();
        assert!(matches!(err, McpClientError::EmptyResult { .. }), "got {err:?}");
    }
}
