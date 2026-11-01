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
///
/// The wire shape is OpenAI's, and its `content` has **two** spellings: a plain string, and an
/// array of parts when the message carries something a string cannot — a picture. In Rust that is
/// one field of text beside one field of images, and [`Message`]'s own `Serialize` picks the
/// spelling: text alone serialises exactly as it did before images existed, and text with images
/// becomes the parts array. Reading accepts both, so a response echoing the array parses and a
/// `history.jsonl` written before this distinction still loads.
///
/// Two reasons for keeping the text in `content` rather than folding it into the parts:
///
/// - **Every existing reader keeps working.** The history budget, the tail-fitting rules and the
///   transcript all ask a message for its prose; they should not have to learn the wire shape to
///   get it. A message whose text is behind a `Part` enum makes each of them a place to get wrong.
/// - **A message with no images is byte-identical to what this crate sent before**, which is what
///   lets the change ship without a flag, a version bump or a second endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    pub role: String,
    /// Absent for an assistant message that carries only tool calls.
    pub content: Option<String>,
    /// Pictures this message carries, each a `data:` URL.
    ///
    /// A data URL and not a path: the endpoint is remote — the user's own provider — and a path on
    /// this machine names nothing it can open. It is also why this crate stays free of I/O: what
    /// turns a clipboard into such a URL is the caller's business.
    pub images: Option<Vec<String>>,
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Set on a `tool` message: which call it answers.
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

    /// A user message that carries pictures, with or without words.
    ///
    /// The text is `Option` because a picture is often the whole question: forcing a sentence in
    /// front of it would make the caller invent words the person did not say.
    pub fn with_images(text: Option<String>, images: Vec<String>) -> Self {
        Self {
            role: "user".into(),
            content: text.filter(|text| !text.trim().is_empty()),
            images: if images.is_empty() {
                None
            } else {
                Some(images)
            },
            ..Default::default()
        }
    }

    /// The text this message carries, whatever else it carries.
    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }

    /// How many pictures it carries.
    pub fn image_count(&self) -> usize {
        self.images.as_ref().map_or(0, Vec::len)
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
            images: None,
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
            images: None,
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// The `content` an image-carrying message is sent as: OpenAI's parts array.
///
/// Only the two kinds this crate ever sends. A `text` part is the message's own prose; an
/// `image_url` part is one `data:` URL the model is expected to look at.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: ImageUrl<'a> },
}

#[derive(Debug, Serialize)]
struct ImageUrl<'a> {
    url: &'a str,
}

impl Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let images = self.images.as_deref().unwrap_or(&[]);
        // Two fields and not three when there is no picture, so a message this crate sent before
        // images existed is the same bytes now: no `images: []`, no parts array, nothing for a
        // provider or a golden test to notice.
        let fields = 1
            + usize::from(self.content.is_some() || !images.is_empty())
            + usize::from(self.tool_calls.is_some())
            + usize::from(self.tool_call_id.is_some());
        let mut out = serializer.serialize_struct("Message", fields)?;
        out.serialize_field("role", &self.role)?;
        if images.is_empty() {
            if let Some(text) = &self.content {
                out.serialize_field("content", text)?;
            }
        } else {
            // The parts array. Text first, because a reader of the raw request reads the question
            // before the picture it is about.
            let mut parts: Vec<ContentPart> = Vec::with_capacity(images.len() + 1);
            if let Some(text) = self.content.as_deref().filter(|text| !text.is_empty()) {
                parts.push(ContentPart::Text { text });
            }
            for url in images {
                parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrl { url },
                });
            }
            out.serialize_field("content", &parts)?;
        }
        if let Some(calls) = &self.tool_calls {
            out.serialize_field("tool_calls", calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            out.serialize_field("tool_call_id", id)?;
        }
        out.end()
    }
}

impl<'de> Deserialize<'de> for Message {
    /// Reads either spelling of `content`, which is what makes a response that echoes the parts
    /// array parse: this crate's own deserializer must accept what its serializer writes.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            role: String,
            #[serde(default)]
            content: Option<serde_json::Value>,
            #[serde(default)]
            tool_calls: Option<Vec<ToolCall>>,
            #[serde(default)]
            tool_call_id: Option<String>,
        }

        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum RawPart {
            Text { text: String },
            ImageUrl { image_url: RawImage },
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawImage {
            Url { url: String },
            Bare(String),
        }

        let raw = Raw::deserialize(deserializer)?;
        let mut content = None;
        let mut images: Vec<String> = Vec::new();
        match raw.content {
            // Absent or `null`: a tool-calling assistant turn, and the shape most responses have.
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::String(text)) => content = Some(text),
            Some(serde_json::Value::Array(parts)) => {
                let mut texts: Vec<String> = Vec::new();
                for part in parts {
                    match serde_json::from_value::<RawPart>(part).map_err(D::Error::custom)? {
                        RawPart::Text { text } => texts.push(text),
                        RawPart::ImageUrl { image_url } => match image_url {
                            RawImage::Url { url } | RawImage::Bare(url) => images.push(url),
                        },
                    }
                }
                if !texts.is_empty() {
                    content = Some(texts.join(""));
                }
            }
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "`content` is neither text nor a list of parts: {other}"
                )))
            }
        }
        Ok(Message {
            role: raw.role,
            content,
            images: if images.is_empty() {
                None
            } else {
                Some(images)
            },
            tool_calls: raw.tool_calls,
            tool_call_id: raw.tool_call_id,
        })
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

    const PIXEL: &str = "data:image/png;base64,iVBORw0KGgo=";

    #[test]
    fn a_message_with_no_picture_serialises_as_it_always_did() {
        // The whole reason the text stayed in `content`: a turn that carries no image must be the
        // same bytes it was before images existed, so nothing downstream has to learn a new shape
        // to keep working.
        let json = serde_json::to_string(&Message::user("hello")).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"hello"}"#);
    }

    #[test]
    fn a_message_with_a_picture_serialises_as_content_parts() {
        // OpenAI's spelling, verbatim: the question first, then the picture it is about.
        let msg = Message::with_images(Some("what is this?".into()), vec![PIXEL.into()]);
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"role":"user","content":[{{"type":"text","text":"what is this?"}},{{"type":"image_url","image_url":{{"url":"{PIXEL}"}}}}]}}"#
            )
        );
    }

    #[test]
    fn a_picture_on_its_own_needs_no_words_and_no_invented_ones() {
        let msg = Message::with_images(None, vec![PIXEL.into()]);
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"role":"user","content":[{{"type":"image_url","image_url":{{"url":"{PIXEL}"}}}}]}}"#
            )
        );
        // Blank text is no text: a message of spaces is not a question.
        let blank = Message::with_images(Some("   ".into()), vec![PIXEL.into()]);
        assert_eq!(blank.content, None);
    }

    #[test]
    fn the_parts_array_reads_back_as_the_text_and_the_picture_it_was() {
        // This crate's deserializer has to accept what its serializer writes, or a conversation
        // read back from `history.jsonl` loses the pictures the moment it is reloaded.
        let written = serde_json::to_string(&Message::with_images(
            Some("what is this?".into()),
            vec![PIXEL.into(), PIXEL.into()],
        ))
        .unwrap();
        let read: Message = serde_json::from_str(&written).unwrap();
        assert_eq!(read.role, "user");
        assert_eq!(read.text(), "what is this?");
        assert_eq!(read.image_count(), 2);
        assert_eq!(read.images.as_deref().unwrap()[0], PIXEL);
        // And it writes back the same bytes.
        assert_eq!(serde_json::to_string(&read).unwrap(), written);
    }

    #[test]
    fn a_plain_string_still_reads_as_it_did() {
        let read: Message = serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        assert_eq!(read.text(), "hello");
        assert_eq!(read.image_count(), 0);
        assert!(read.images.is_none());
    }

    #[test]
    fn a_content_that_is_neither_text_nor_parts_is_refused_by_name() {
        // Silence here would be a turn that ran with the picture quietly missing.
        let error = serde_json::from_str::<Message>(r#"{"role":"user","content":42}"#).unwrap_err();
        assert!(
            error.to_string().contains("neither text nor a list"),
            "{error}"
        );
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
