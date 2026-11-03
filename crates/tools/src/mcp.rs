//! Remote tools: an app's own capabilities, offered to the model as tools.
//!
//! An app is where its own data lives — a todo list in SQLite, a Feishu account behind a CLI — and
//! the agent that works on that app needs hands that reach it. Those hands cannot be written here:
//! this crate must not know what a todo is. So they are **declared by the app** and reached over
//! the wire, which is what makes them different from every other tool in this crate — `bash`,
//! `read`, `write` and the rest run in this process, inside the app's fence.
//!
//! The wire is **MCP** (Model Context Protocol), and it is chosen rather than invented for three
//! reasons. The tool description is already a JSON Schema, which is exactly what
//! [`ToolDefinition`] carries, so `inputSchema` crosses with no translation. The transport the
//! specification calls *Streamable HTTP* sends **each message as its own `POST`** to one endpoint
//! and — since revision 2026-07-28 — keeps no protocol-level session, so it is a request/response
//! exchange rather than a connection this process must hold open. And an app's own tools then
//! arrive by the same road as anybody else's MCP server, so a feature written for one is a feature
//! available to the other.
//!
//! Three properties are the whole of what this module promises:
//!
//! - **A tool this set cannot reach is not a tool the model is told about.** [`McpServer::list`]
//!   asks the app what it has, and the answer is the set; a discovery that fails yields no tools
//!   plus a reason, never a stub that answers "not available" when called.
//! - **The name the model calls is the name the app declared, and only that name.** Names arrive
//!   at run time, which is why [`Tool::name`] returns a borrowed `&str` rather than a
//!   `&'static str` — the trait changed for this module and for nothing else.
//! - **A failure is the model's to read, not this crate's to hide.** A transport error, a JSON-RPC
//!   `error`, and `isError: true` are three different sentences, and each reaches the model as one.
//!
//! What it deliberately does not do: it holds no runtime, opens no socket and knows no URL scheme.
//! The exchange is [`McpTransport`], implemented by whoever owns a socket — the same seam as
//! [`crate::app::AppControl`], for the same reason.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use deepnest_llm::ToolDefinition;
use serde_json::{json, Value};

use crate::{Tool, ToolContext, ToolResult};

/// What came back from one HTTP exchange.
///
/// The content type travels beside the body because the specification lets a server answer a
/// request with either a JSON object or a server-sent event stream scoped to that request, and
/// which one it sent is not guessable from the bytes: an SSE body and a JSON body can both start
/// with a `{`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub content_type: String,
    pub body: String,
}

/// How a message reaches an MCP server, as this crate needs it.
///
/// Implemented outside this crate — `crates/deepnest` is the one that owns sockets — and synchronous
/// for the reason [`Tool::call`] is: the turn thread is already the thing doing the waiting, and a
/// second concurrency model inside a tool is a second place for a turn to hang.
pub trait McpTransport: Send + Sync {
    /// One JSON-RPC message, as one `POST` to `url`.
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Reply, String>;
}

/// One tool an MCP server declared, before it becomes a [`Tool`].
#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    /// What the model calls. The server's spelling, untouched.
    pub name: String,
    /// The server's one-line title, when it gave one.
    pub title: Option<String>,
    pub description: String,
    /// The `inputSchema`, verbatim.
    pub schema: Value,
}

/// One MCP server, as an endpoint and the credential that opens it.
///
/// The credential is not decorated onto each call: it belongs to the server, because the server is
/// the app, and the app was handed its own value when it was started.
pub struct McpServer {
    url: String,
    token: Option<String>,
    transport: Arc<dyn McpTransport>,
    next_id: AtomicU64,
}

impl McpServer {
    pub fn new(url: impl Into<String>, token: Option<String>, transport: Arc<dyn McpTransport>) -> Self {
        Self {
            url: url.into(),
            token,
            transport,
            // A JSON-RPC id is opaque; what matters is that two requests on one server are
            // distinguishable, and a counter in this struct is the cheapest way to say so.
            next_id: AtomicU64::new(1),
        }
    }

    /// The endpoint, for a refusal that has to say where it looked.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// What this server offers, or why it could not be asked.
    ///
    /// `tools/list` is paginated by the specification, and this follows `nextCursor` to the end:
    /// an app that declares nine tools and answers with the first five would otherwise be an app
    /// whose other four are invisible for no stated reason.
    pub fn list(&self) -> Result<Vec<Declaration>, String> {
        let mut declared = Vec::new();
        let mut cursor: Option<String> = None;
        // A bound, because a server that answers with a cursor forever is a server this turns into
        // an infinite loop. Ten pages of any plausible app's tools is far past generous.
        for _ in 0..10 {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params)?;
            for tool in result.get("tools").and_then(Value::as_array).into_iter().flatten() {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    // A declaration with no name cannot be called, so it is not a tool. Skipped
                    // rather than refused: one malformed entry must not hide the rest.
                    continue;
                };
                declared.push(Declaration {
                    name: name.to_string(),
                    title: tool
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                });
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(declared),
            }
        }
        Ok(declared)
    }

    /// One `tools/call`, in the tool result shape the model reads.
    ///
    /// The name passed in is a [`ToolResult`]'s label, which is the caller's spelling of the tool
    /// rather than this module's: a result that named the tool differently from the call would be
    /// a transcript row disagreeing with the request beside it.
    pub fn call(&self, name: &str, arguments: &Value) -> ToolResult {
        let params = json!({ "name": name, "arguments": arguments });
        let result = match self.request("tools/call", params) {
            Ok(result) => result,
            Err(reason) => return ToolResult::error(name, reason),
        };
        let text = content_text(&result);
        let text = match structured(&result, &text) {
            Some(both) => both,
            None => text,
        };
        if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            // The server said the call failed, in its own words. `error` rather than `refused`:
            // nothing here was denied by a policy, and telling the model otherwise would send it
            // looking for a permission it already has.
            return ToolResult::error(name, text);
        }
        ToolResult::ok(name, text)
    }

    /// One JSON-RPC exchange, with the envelope's own error handling.
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
            // Every request carries the protocol version and this client's identity, per the
            // specification's `_meta` requirement. Sent rather than omitted: a server that
            // negotiates on it and receives nothing is one this cannot talk to at all.
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientInfo": { "name": "deepnest", "version": env!("CARGO_PKG_VERSION") },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });

        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            // Both, as the specification requires of a client: the server chooses, and a client
            // that named only one would be refused by a server that answers in the other.
            (
                "Accept".to_string(),
                "application/json, text/event-stream".to_string(),
            ),
        ];
        if let Some(token) = &self.token {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }

        let reply = self
            .transport
            .post(&self.url, &headers, &envelope.to_string())?;
        let body = payload(&reply, id)?;

        // A JSON-RPC error is an answer, not a transport failure: the server was reached and said
        // why it would not. Both halves reach the model, because a message without a code and a
        // code without a message are each half a sentence.
        if let Some(error) = body.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            return Err(format!(
                "`{}` answered JSON-RPC error {code}: {message}",
                self.url
            ));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| format!("`{}` answered without a result", self.url))
    }
}

/// The version this client speaks. Named once, because it appears in every request.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// The JSON-RPC body out of an answer that may be JSON or an event stream.
///
/// An event stream carries notifications first and the response last, so the response is the last
/// `data:` line that parses and carries this request's `id`. A stream that ends without one is a
/// failure with a sentence: a server that answered something is not the same as an answer.
fn payload(reply: &Reply, id: u64) -> Result<Value, String> {
    if !reply.content_type.contains("text/event-stream") {
        return serde_json::from_str(&reply.body)
            .map_err(|error| format!("the answer was not JSON: {error}"));
    }
    let mut seen = None;
    for line in reply.body.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(rest.trim()) else {
            continue;
        };
        let matches = value
            .get("id")
            .and_then(Value::as_u64)
            .is_some_and(|theirs| theirs == id);
        if matches {
            seen = Some(value);
        }
    }
    seen.ok_or_else(|| {
        format!(
            "the event stream ended without an answer to request {id}; what arrived was {} bytes",
            reply.body.len()
        )
    })
}

/// The text of a `tools/call` result, in the order the server put it.
///
/// Content blocks that are not text are named rather than dropped: a picture a tool returned is
/// something the model should know exists, even where this route cannot carry it into the
/// conversation.
fn content_text(result: &Value) -> String {
    let Some(blocks) = result.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            Some(other) => parts.push(format!("[{other} content, {} bytes]", block.to_string().len())),
            None => {}
        }
    }
    parts.join("\n")
}

/// The structured half, when the server sent one, appended to the text half.
///
/// Both rather than one: a server that declares an `outputSchema` puts the machine-readable answer
/// in `structuredContent` and may leave the text thin, and a model reading only the text would be
/// reading a summary of an answer it was given in full.
fn structured(result: &Value, text: &str) -> Option<String> {
    let structured = result.get("structuredContent")?;
    if structured.is_null() {
        return None;
    }
    Some(if text.is_empty() {
        structured.to_string()
    } else {
        format!("{text}\n\n{}", structured)
    })
}

/// One declared tool, as the agent's hand.
///
/// Holds the server rather than a copy of its URL and credential: the two tools of one app share
/// one endpoint, and two copies of a credential are two places to revoke it.
pub struct McpTool {
    server: Arc<McpServer>,
    declaration: Declaration,
}

impl McpTool {
    pub fn new(server: Arc<McpServer>, declaration: Declaration) -> Self {
        Self { server, declaration }
    }

    /// The declaration this tool was built from, for a caller assembling a report.
    pub fn declaration(&self) -> &Declaration {
        &self.declaration
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.declaration.name
    }

    fn definition(&self) -> ToolDefinition {
        // The description is the server's, with the one sentence this crate adds: a model reading
        // a remote tool exactly like a local one would reach for it expecting `bash`'s reach, and
        // what it can touch is whatever the app it names can.
        let description = if self.declaration.description.is_empty() {
            format!(
                "A tool provided by the app this conversation builds. It runs inside that app, not \
                 in this process, so what it can reach is whatever `{}` can.",
                self.server.url()
            )
        } else {
            format!(
                "{}\n\nProvided by the app this conversation builds, and run inside it rather \
                 than in this process.",
                self.declaration.description
            )
        };
        ToolDefinition::function(self.name().to_string(), description, self.declaration.schema.clone())
    }

    fn call(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        // The fence does not apply and is not consulted: this call leaves the sandbox entirely, by
        // design, and a policy check here would be a boundary this module does not have. What
        // bounds it is the app — one endpoint, one credential, that app's own capabilities.
        self.server.call(&self.declaration.name, args)
    }

    /// Not here: this call is the app's work, and the turn hands it back rather than making it.
    ///
    /// The reason is not this module's but the turn's — see `crates/agent`'s `TurnOutcome::Paused`.
    /// In one sentence: the app that declares these tools is also the app that made the request the
    /// turn is answering, and a program answering one request at a time cannot answer a callback
    /// from inside it. Handing the batch back ends the request instead of nesting inside it.
    fn runs_here(&self) -> bool {
        false
    }
}

/// Every tool `server` declares, as tools a turn can dispatch.
///
/// An empty vector plus a reason when discovery failed, rather than a single tool that reports the
/// failure: the caller decides how loudly to say it, and a model offered a tool that cannot work
/// will spend a round learning that.
pub fn tools(server: Arc<McpServer>) -> (Vec<Box<dyn Tool>>, Option<String>) {
    match server.list() {
        Ok(declared) => {
            let tools: Vec<Box<dyn Tool>> = declared
                .into_iter()
                .map(|declaration| {
                    Box::new(McpTool::new(Arc::clone(&server), declaration)) as Box<dyn Tool>
                })
                .collect();
            (tools, None)
        }
        Err(reason) => (Vec::new(), Some(reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A transport that answers from a script and records what it was sent.
    struct Scripted {
        replies: Mutex<Vec<Result<Reply, String>>>,
        sent: Mutex<Vec<(String, Vec<(String, String)>, String)>>,
    }

    impl Scripted {
        fn new(replies: Vec<Reply>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into_iter().map(Ok).collect()),
                sent: Mutex::new(Vec::new()),
            })
        }

        fn sent(&self) -> Vec<(String, Vec<(String, String)>, String)> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl McpTransport for Scripted {
        fn post(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &str,
        ) -> Result<Reply, String> {
            self.sent
                .lock()
                .unwrap()
                .push((url.to_string(), headers.to_vec(), body.to_string()));
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Err("no scripted reply left".to_string());
            }
            replies.remove(0)
        }
    }

    fn json_reply(body: impl Into<String>) -> Reply {
        Reply {
            content_type: "application/json".to_string(),
            body: body.into(),
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(
            deepnest_sandbox::Policy::for_app(
                deepnest_sandbox::Mode::WorkspaceWrite,
                "/home/u/.deepnest",
                "notes",
                None,
            ),
            "/home/u/.deepnest/apps/notes",
        )
    }

    #[test]
    fn a_declared_tool_is_offered_with_its_own_schema_and_name() {
        let transport = Scripted::new(vec![json_reply(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[
                {"name":"todos_list","description":"List the open todos.",
                 "inputSchema":{"type":"object","properties":{"state":{"type":"string"}}}}]}}"#,
        )]);
        let server = Arc::new(McpServer::new(
            "http://127.0.0.1:9000/mcp",
            Some("ticket".to_string()),
            transport.clone(),
        ));
        let (tools, reason) = tools(Arc::clone(&server));
        assert!(reason.is_none(), "{reason:?}");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "todos_list");
        let definition = tools[0].definition();
        // The schema crosses unchanged: translating it would be this crate deciding what an app's
        // parameters mean.
        assert_eq!(definition.function.parameters["properties"]["state"]["type"], "string");
        assert!(definition.function.description.contains("List the open todos"));
        // And the one sentence this crate adds, so a remote tool is not read as a local one.
        assert!(definition.function.description.contains("inside it rather than in this process"));
    }

    #[test]
    fn the_request_carries_the_token_and_the_version_and_the_call_carries_the_arguments() {
        let transport = Scripted::new(vec![
            json_reply(
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"done","inputSchema":{"type":"object"}}]}}"#,
            ),
            json_reply(
                r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"marked 3 done"}],"isError":false}}"#,
            ),
        ]);
        let server = Arc::new(McpServer::new(
            "http://127.0.0.1:9000/mcp",
            Some("ticket".to_string()),
            transport.clone(),
        ));
        let (tools, _) = tools(Arc::clone(&server));
        let result = tools[0].call(&json!({"id": 3}), &ctx());
        assert_eq!(result.content, "marked 3 done");
        assert!(!result.is_error);

        let sent = transport.sent();
        assert_eq!(sent.len(), 2);
        for (url, headers, _) in &sent {
            assert_eq!(url, "http://127.0.0.1:9000/mcp");
            assert!(headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer ticket"));
            assert!(headers
                .iter()
                .any(|(name, value)| name == "Accept" && value.contains("text/event-stream")));
        }
        let call: Value = serde_json::from_str(&sent[1].2).unwrap();
        assert_eq!(call["method"], "tools/call");
        assert_eq!(call["params"]["name"], "done");
        assert_eq!(call["params"]["arguments"]["id"], 3);
        assert_eq!(
            call["_meta"]["io.modelcontextprotocol/protocolVersion"],
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn a_server_that_answers_in_an_event_stream_is_read_to_its_last_matching_event() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n\
                    event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"one\"}]}}\n\n";
        let transport = Scripted::new(vec![Reply {
            content_type: "text/event-stream".to_string(),
            body: body.to_string(),
        }]);
        let server = Arc::new(McpServer::new("http://127.0.0.1:9000/mcp", None, transport));
        let declared = server.list().unwrap();
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].name, "one");
    }

    #[test]
    fn a_stream_that_ends_without_an_answer_is_a_failure_with_a_sentence() {
        let transport = Scripted::new(vec![Reply {
            content_type: "text/event-stream".to_string(),
            body: "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notes\"}\n\n".to_string(),
        }]);
        let server = McpServer::new("http://127.0.0.1:9000/mcp", None, transport);
        let reason = server.list().unwrap_err();
        assert!(reason.contains("without an answer"), "{reason}");
    }

    #[test]
    fn a_json_rpc_error_reaches_the_model_with_its_code() {
        let transport = Scripted::new(vec![json_reply(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unknown tool `nope`"}}"#,
        )]);
        let server = McpServer::new("http://127.0.0.1:9000/mcp", None, transport);
        let reason = server.list().unwrap_err();
        assert!(reason.contains("-32602"), "{reason}");
        assert!(reason.contains("unknown tool"), "{reason}");
    }

    #[test]
    fn is_error_is_the_servers_own_word_and_is_not_a_refusal() {
        let transport = Scripted::new(vec![json_reply(
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"no such todo"}],"isError":true}}"#,
        )]);
        let server = McpServer::new("http://127.0.0.1:9000/mcp", None, transport);
        let result = server.call("done", &json!({"id": 9}));
        assert!(result.is_error);
        // Not refused: no policy denied this, and a model told "refused" would look for a
        // permission rather than a different argument.
        assert!(!result.refused);
        assert_eq!(result.content, "no such todo");
    }

    #[test]
    fn discovery_that_fails_offers_no_tools_and_says_why() {
        let transport = Arc::new(Scripted {
            replies: Mutex::new(vec![Err("connection refused".to_string())]),
            sent: Mutex::new(Vec::new()),
        });
        let server = Arc::new(McpServer::new("http://127.0.0.1:9/mcp", None, transport));
        let (tools, reason) = tools(server);
        assert!(tools.is_empty(), "a tool that cannot work is not offered");
        assert!(reason.unwrap().contains("connection refused"));
    }

    #[test]
    fn pagination_is_followed_to_the_end() {
        let transport = Scripted::new(vec![
            json_reply(
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"one"}],"nextCursor":"page2"}}"#,
            ),
            json_reply(
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"two"}]}}"#,
            ),
        ]);
        let server = Arc::new(McpServer::new("http://127.0.0.1:9000/mcp", None, transport.clone()));
        let declared = server.list().unwrap();
        assert_eq!(declared.len(), 2, "{declared:?}");
        // The second request carried the cursor the first answer named.
        let sent = transport.sent();
        let second: Value = serde_json::from_str(&sent[1].2).unwrap();
        assert_eq!(second["params"]["cursor"], "page2");
    }

    #[test]
    fn structured_content_is_added_to_the_text_and_neither_is_dropped() {
        let transport = Scripted::new(vec![json_reply(
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"3 open"}],
                "structuredContent":{"count":3},"isError":false}}"#,
        )]);
        let server = McpServer::new("http://127.0.0.1:9000/mcp", None, transport);
        let result = server.call("count", &json!({}));
        assert!(result.content.contains("3 open"), "{}", result.content);
        assert!(result.content.contains("\"count\":3"), "{}", result.content);
    }
}
