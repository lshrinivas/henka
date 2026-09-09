//! One LSP session over a single connection.
//!
//! The session reads `Content-Length`-framed JSON-RPC, maps each request to a
//! catalog operation via the shared dispatch core, and writes the result back as
//! the LSP response an editor expects. It is deliberately small: everything that
//! makes an operation correct — project resolution, path mapping, the working-
//! copy overlay, applying edits — lives in the core and is reused unchanged.

use henka_core::{Error as CoreError, Target};
use henka_lsp::framing;
use serde_json::{Value, json};
use tokio::io::{AsyncWrite, BufReader};
use tokio::net::TcpStream;

use crate::lsp::map::{self, Identity};
use crate::mcp::{DispatchResult, HenkaMcp};
use crate::ops;

/// A JSON-RPC failure to report against a request id.
struct Fail {
    code: i64,
    message: String,
}

impl Fail {
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not supported: {method}"),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }
}

/// Map a core error onto the closest JSON-RPC error, mirroring the MCP surface.
fn core_fail(err: CoreError) -> Fail {
    match err {
        CoreError::Backend(_)
        | CoreError::ConfigRead { .. }
        | CoreError::ConfigWrite(_)
        | CoreError::Io(_) => Fail {
            code: -32603,
            message: err.to_string(),
        },
        other => Fail::invalid_params(other.to_string()),
    }
}

/// Run one LSP session to completion over `stream`.
pub async fn run(handler: HenkaMcp, stream: TcpStream) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut session = Session::new(handler);

    while let Some(message) = framing::read_message(&mut reader).await? {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        // A message without an id is a notification: no response, and `exit`
        // ends the session.
        let Some(id) = message.get("id").cloned() else {
            if method == "exit" {
                break;
            }
            session.on_notification(method);
            continue;
        };

        match session.on_request(method, &params).await {
            Ok(result) => reply(&mut write, &id, result).await?,
            Err(fail) => reply_error(&mut write, &id, fail).await?,
        }
    }
    Ok(())
}

/// Write a successful JSON-RPC response.
async fn reply<W: AsyncWrite + Unpin>(write: &mut W, id: &Value, result: Value) -> anyhow::Result<()> {
    framing::write_message(write, &json!({ "jsonrpc": "2.0", "id": id, "result": result })).await?;
    Ok(())
}

/// Write a JSON-RPC error response.
async fn reply_error<W: AsyncWrite + Unpin>(write: &mut W, id: &Value, fail: Fail) -> anyhow::Result<()> {
    framing::write_message(
        write,
        &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": fail.code, "message": fail.message } }),
    )
    .await?;
    Ok(())
}

/// Per-connection session state.
struct Session {
    handler: HenkaMcp,
    identity: Option<Identity>,
}

impl Session {
    fn new(handler: HenkaMcp) -> Self {
        Self {
            handler,
            identity: None,
        }
    }

    /// Handle a notification. Document-sync notifications are accepted and
    /// ignored — Henka reads the working copy from disk, not client buffers.
    fn on_notification(&self, _method: &str) {}

    /// Handle a request, producing its LSP result or a failure.
    async fn on_request(&mut self, method: &str, params: &Value) -> Result<Value, Fail> {
        match method {
            "initialize" => {
                self.identity = map::identity(params);
                // Pick up working copies auto-registered under a workspaces
                // mount, so a freshly opened project resolves without a manual
                // registration step.
                self.handler.warm_registry().await;
                Ok(self.capabilities())
            }
            // A no-op ack; the connection stays open for requests.
            "initialized" => Ok(Value::Null),
            "shutdown" => Ok(Value::Null),

            "textDocument/references" => {
                let target = map::position_target(params).map_err(Fail::invalid_params)?;
                let query = self.query("find-usages", target, json!({})).await?;
                Ok(map::locations(&query, "usages", self.folder()))
            }
            "textDocument/definition" => {
                let target = map::position_target(params).map_err(Fail::invalid_params)?;
                let query = self.query("go-to-definition", target, json!({})).await?;
                Ok(map::locations(&query, "definitions", self.folder()))
            }
            "textDocument/implementation" => {
                let target = map::position_target(params).map_err(Fail::invalid_params)?;
                let query = self.query("find-implementations", target, json!({})).await?;
                Ok(map::locations(&query, "implementations", self.folder()))
            }
            "textDocument/hover" => {
                let target = map::position_target(params).map_err(Fail::invalid_params)?;
                let query = self.query("describe-symbol", target, json!({})).await?;
                Ok(map::hover(&query))
            }
            "textDocument/documentSymbol" => {
                let target = map::file_target(params).map_err(Fail::invalid_params)?;
                let query = self.query("file-outline", target, json!({})).await?;
                Ok(map::outline(&query))
            }
            "textDocument/prepareCallHierarchy" => {
                let target = map::position_target(params).map_err(Fail::invalid_params)?;
                let query = self.query("prepare-call-hierarchy", target, json!({})).await?;
                Ok(map::call_hierarchy_items(&query))
            }
            "callHierarchy/incomingCalls" => {
                let query = self
                    .query("incoming-calls", Target::Project, self.item_params(params)?)
                    .await?;
                Ok(map::calls(&query, "from"))
            }
            "callHierarchy/outgoingCalls" => {
                let query = self
                    .query("outgoing-calls", Target::Project, self.item_params(params)?)
                    .await?;
                Ok(map::calls(&query, "to"))
            }

            // A rename resolves and applies the edit server-side, matching the
            // MCP surface, then reports what changed (rather than handing back an
            // edit for the client to apply).
            "textDocument/rename" => {
                let target = map::position_target(params).map_err(Fail::invalid_params)?;
                let new_name = params
                    .get("newName")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Fail::invalid_params("missing `newName`"))?;
                self.dispatch("rename", target, json!({ "new_name": new_name }), false)
                    .await
                    .map(into_value)
            }
            // The identifier's own range is left to the client to compute.
            "textDocument/prepareRename" => Ok(Value::Null),

            "workspace/executeCommand" => self.execute_command(params).await,

            // No code actions are offered; operations are reached via
            // executeCommand.
            "textDocument/codeAction" => Ok(Value::Array(Vec::new())),

            other => Err(Fail::method_not_found(other)),
        }
    }

    /// The advertised server capabilities.
    fn capabilities(&self) -> Value {
        json!({
            "capabilities": {
                "positionEncoding": "utf-16",
                "referencesProvider": true,
                "definitionProvider": true,
                "implementationProvider": true,
                "hoverProvider": true,
                "documentSymbolProvider": true,
                "callHierarchyProvider": true,
                "renameProvider": { "prepareProvider": true },
                "executeCommandProvider": { "commands": self.handler.command_ids() },
            },
            "serverInfo": { "name": "henka", "version": env!("CARGO_PKG_VERSION") },
        })
    }

    /// Run a query operation and return its raw structured result.
    async fn query(&self, op_id: &str, target: Target, params: Value) -> Result<Value, Fail> {
        match self.dispatch(op_id, target, params, true).await? {
            DispatchResult::Query(value) => Ok(value),
            DispatchResult::Edit(value) => Ok(value),
        }
    }

    /// Run a catalog operation for this session's project through the shared
    /// core, targeting the working copy the client opened. Fans out across
    /// every language that registers `op_id`, exactly as the MCP surface does.
    async fn dispatch(
        &self,
        op_id: &str,
        target: Target,
        params: Value,
        dry_run: bool,
    ) -> Result<DispatchResult, Fail> {
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| Fail::invalid_params("no workspace: send `initialize` first"))?;
        let project = self
            .handler
            .project_snapshot(&identity.project_id)
            .await
            .map_err(core_fail)?;
        self.handler
            .run(
                &project,
                op_id,
                target,
                params,
                identity.workspace.clone(),
                None,
                dry_run,
            )
            .await
            .map_err(core_fail)
    }

    /// Run an arbitrary catalog operation named by an `executeCommand`, parsing
    /// its target and parameters from the command's single argument object the
    /// same way the MCP surface parses a tool call.
    async fn execute_command(&self, params: &Value) -> Result<Value, Fail> {
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| Fail::invalid_params("missing `command`"))?;
        let args = params
            .get("arguments")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| Fail::invalid_params("no workspace: send `initialize` first"))?;
        let project = self
            .handler
            .project_snapshot(&identity.project_id)
            .await
            .map_err(core_fail)?;
        let descriptor = self
            .handler
            .descriptor_for(command, &project.languages)
            .map_err(core_fail)?;

        let target = ops::parse_target(&args, descriptor.target)
            .map_err(|e| Fail::invalid_params(e.message.to_string()))?;
        let op_params = ops::operation_params(&args);
        let dry_run = ops::dry_run(&args);

        let result = self
            .handler
            .run(
                &project,
                command,
                target,
                op_params,
                identity.workspace.clone(),
                ops::expect(&args),
                dry_run,
            )
            .await
            .map_err(core_fail)?;
        Ok(into_value(result))
    }

    /// The `{ item }` params a call-hierarchy direction requires, taken from the
    /// request's `item` field.
    fn item_params(&self, params: &Value) -> Result<Value, Fail> {
        let item = params
            .get("item")
            .filter(|i| i.is_object())
            .ok_or_else(|| Fail::invalid_params("missing call hierarchy `item`"))?;
        Ok(json!({ "item": item }))
    }

    /// The working copy the client opened, for building result URIs.
    fn folder(&self) -> &std::path::Path {
        self.identity
            .as_ref()
            .map(|i| i.folder.as_path())
            .unwrap_or_else(|| std::path::Path::new(""))
    }
}

/// The response value carried by either dispatch outcome.
fn into_value(result: DispatchResult) -> Value {
    match result {
        DispatchResult::Query(value) | DispatchResult::Edit(value) => value,
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::path::Path;
    use std::sync::Arc;

    use async_trait::async_trait;
    use henka_core::operation::{
        Operation, OperationCtx, OperationDescriptor, OperationKind, OperationOutcome,
        OperationRequest, TargetKind,
    };
    use henka_core::{
        Language, LanguageProvider, LanguageSession, Project, ProjectRegistry, ProviderRegistry,
        Result as CoreResult,
    };
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::pathmap::PathMap;

    /// A stand-in find-usages that answers with one enriched usage, so the test
    /// exercises the request→operation→LSP-response path without a real backend.
    struct UsagesOp;

    #[async_trait]
    impl Operation for UsagesOp {
        fn descriptor(&self) -> OperationDescriptor {
            OperationDescriptor {
                id: "find-usages".into(),
                title: "Find usages".into(),
                description: "test".into(),
                kind: OperationKind::Query,
                languages: vec![Language::Java],
                target: TargetKind::Position,
                params_schema: json!({ "type": "object", "properties": {} }),
            }
        }

        async fn run(
            &self,
            _ctx: &OperationCtx<'_>,
            _req: &OperationRequest,
        ) -> CoreResult<OperationOutcome> {
            Ok(OperationOutcome::Query(json!({
                "count": 1,
                "usages": [{
                    "file": "Main.java",
                    "start_line": 0, "start_character": 0,
                    "end_line": 0, "end_character": 5,
                    "text": "hello"
                }]
            })))
        }
    }

    struct MockSession;
    impl LanguageSession for MockSession {
        fn language(&self) -> Language {
            Language::Java
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct MockProvider;
    #[async_trait]
    impl LanguageProvider for MockProvider {
        fn language(&self) -> Language {
            Language::Java
        }
        fn operations(&self) -> Vec<Arc<dyn Operation>> {
            vec![Arc::new(UsagesOp)]
        }
        async fn session(&self, _project: &Project) -> CoreResult<Arc<dyn LanguageSession>> {
            Ok(Arc::new(MockSession))
        }
    }

    /// A handler over a Java project named `proj` (so its derived id matches the
    /// workspace-folder basename the client sends).
    fn handler(dir: &Path) -> (HenkaMcp, std::path::PathBuf) {
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pom.xml"), "<project/>").unwrap();
        std::fs::write(root.join("Main.java"), "hello\n").unwrap();

        let mut registry = ProjectRegistry::load(dir.join("projects.toml")).unwrap();
        registry.register(None, &root).unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(MockProvider));
        (HenkaMcp::with_path_map(registry, providers, PathMap::default()), root)
    }

    #[tokio::test]
    async fn serves_a_reference_query_over_tcp() {
        let dir = tempfile::tempdir().unwrap();
        let (handler, root) = handler(dir.path());

        // A real socket, driven end to end: the client speaks framed JSON-RPC,
        // exactly what socat forwards from an editor's stdio.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            run(handler, stream).await.unwrap();
        });

        let client = TcpStream::connect(addr).await.unwrap();
        let (client_read, mut client_write) = client.into_split();
        let mut client_read = BufReader::new(client_read);
        let folder = format!("file://{}", root.display());

        // initialize → capabilities.
        framing::write_message(
            &mut client_write,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "workspaceFolders": [{ "uri": folder, "name": "proj" }] }
            }),
        )
        .await
        .unwrap();
        let init = framing::read_message(&mut client_read).await.unwrap().unwrap();
        assert_eq!(init["result"]["capabilities"]["positionEncoding"], json!("utf-16"));
        assert_eq!(init["result"]["capabilities"]["referencesProvider"], json!(true));

        // references → an LSP Location carrying the source line as an extra field.
        framing::write_message(
            &mut client_write,
            &json!({
                "jsonrpc": "2.0", "id": 2, "method": "textDocument/references",
                "params": {
                    "textDocument": { "uri": format!("{folder}/Main.java") },
                    "position": { "line": 0, "character": 0 },
                    "context": { "includeDeclaration": true }
                }
            }),
        )
        .await
        .unwrap();
        let refs = framing::read_message(&mut client_read).await.unwrap().unwrap();
        let location = &refs["result"][0];
        assert_eq!(location["uri"], json!(format!("{folder}/Main.java")));
        assert_eq!(location["range"]["end"]["character"], json!(5));
        assert_eq!(location["text"], json!("hello"));

        // shutdown → null, then exit ends the session.
        framing::write_message(
            &mut client_write,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
        )
        .await
        .unwrap();
        let shutdown = framing::read_message(&mut client_read).await.unwrap().unwrap();
        assert_eq!(shutdown["result"], Value::Null);
        framing::write_message(&mut client_write, &json!({ "jsonrpc": "2.0", "method": "exit" }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_unknown_method_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let (handler, root) = handler(dir.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            run(handler, stream).await.unwrap();
        });

        let client = TcpStream::connect(addr).await.unwrap();
        let (client_read, mut client_write) = client.into_split();
        let mut client_read = BufReader::new(client_read);
        let folder = format!("file://{}", root.display());

        framing::write_message(
            &mut client_write,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "workspaceFolders": [{ "uri": folder }] }
            }),
        )
        .await
        .unwrap();
        let _ = framing::read_message(&mut client_read).await.unwrap().unwrap();

        framing::write_message(
            &mut client_write,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/formatting", "params": {} }),
        )
        .await
        .unwrap();
        let reply = framing::read_message(&mut client_read).await.unwrap().unwrap();
        assert_eq!(reply["error"]["code"], json!(-32601));

        // The session survives the unknown method: a follow-up still answers.
        framing::write_message(
            &mut client_write,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
        )
        .await
        .unwrap();
        let shutdown = framing::read_message(&mut client_read).await.unwrap().unwrap();
        assert_eq!(shutdown["result"], Value::Null);
    }
}
