//! The model the binary uses: the user's own endpoint, one whole answer at a time.
//!
//! `crates/agent` reaches the model through its own `Model` trait so the loop is testable with no
//! network; this is the one line of wiring that connects it to [`deepnest_llm::Client`], and it
//! exists rather than the crate's streaming impl because the HTTP adapter in
//! `crates/gateway::transport` sends a whole request and reads a whole answer. When a TLS adapter
//! lands, this type is what changes — the loop above it is already indifferent.
//!
//! The prompt is passed on every request because the loop owns it; nothing here decides what the
//! agent is told.

use std::sync::Arc;

use deepnest_agent::Model;
use deepnest_llm::{
    AssistantTurn, ChatRequest, Client, LlmError, Message, ToolDefinition, Transport,
};

/// A model client bound to one endpoint, key and model name.
pub struct Endpoint {
    client: Client,
}

impl Endpoint {
    /// One endpoint from the config file's `[llm]` table.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            client: Client::new(base_url, api_key, model, transport),
        }
    }

    /// The endpoint this was built for, for a message that has to say where it sent the request.
    pub fn endpoint(&self) -> &str {
        self.client.endpoint()
    }
}

impl Model for Endpoint {
    fn turn(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<AssistantTurn, LlmError> {
        // No `.streaming()`: this path asks for one response, and the loop still gets an
        // `AssistantTurn`. The trade is arrival, not content.
        let request = ChatRequest::new(self.client.model().to_string(), messages.to_vec())
            .with_tools(tools.to_vec());
        self.client.turn(&request)
    }
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key lives inside the client and must not reach a log line.
        f.debug_struct("Endpoint")
            .field("endpoint", &self.client.endpoint())
            .field("model", &self.client.model())
            .finish()
    }
}
