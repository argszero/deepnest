//! Wire types for an OpenAI-compatible chat endpoint.
//!
//! Only what deepnest sends and reads, spelled the way the protocol spells it. Nothing here
//! knows about a particular vendor: the endpoint, model name and key are the user's
//! (`host 20:18`), so any divergences have to be handled by behaviour, not by branching.

use serde::{Deserialize, Serialize};

fn default_kind() -> String {
    "function".to_string()
}

/// One message in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Message {
    pub role: String,
    /// Absent for an assistant message that carries only tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Set on a `tool` message: which call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(text.into()),
            ..Default::default()
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(text.into()),
            ..Default::default()
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(text.into()),
            ..Default::default()
        }
    }

    /// The assistant turn the model produced, to be replayed back to it.
    pub fn assistant_with_tools(text: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: text,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        }
    }

    /// The result of a tool call, addressed to the call by id.
    pub fn tool(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// A call the model asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FunctionCall {
    pub name: String,
    /// A JSON object, as the model wrote it. Not parsed here: a malformed argument string is
    /// the tool layer's problem to report, not this crate's to reject.
    pub arguments: String,
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    pub function: ToolSpec,
}

impl ToolDefinition {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: default_kind(),
            function: ToolSpec {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A chat completion request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            stream: None,
            tools: None,
            temperature: None,
        }
    }

    pub fn streaming(mut self) -> Self {
        self.stream = Some(true);
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = Some(tools);
        self
    }
}

/// A non-streamed completion.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub message: Message,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_result_message_serialises_the_call_id() {
        let msg = Message::tool("call_1", "42");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"tool""#), "{json}");
        assert!(json.contains(r#""tool_call_id":"call_1""#), "{json}");
    }

    #[test]
    fn an_assistant_turn_with_only_tool_calls_omits_content() {
        let msg = Message::assistant_with_tools(
            None,
            vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                },
            }],
        );
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains("content"),
            "content must be absent, got {json}"
        );
        assert!(json.contains(r#""name":"read""#), "{json}");
    }

    #[test]
    fn a_response_with_null_content_parses() {
        // Providers send this for a tool-calling turn; a `String` field would fail here.
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function","function":{"name":"grep","arguments":"{}"}}]},
            "finish_reason":"tool_calls"}]}"#;
        let resp: ChatResponse = serde_json::from_str(body).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert!(resp.choices[0].message.content.is_none());
        assert_eq!(
            resp.choices[0].message.tool_calls.as_ref().unwrap()[0]
                .function
                .name,
            "grep"
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }
}
