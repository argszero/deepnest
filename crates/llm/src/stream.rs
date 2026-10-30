//! Server-sent events and the assembly of a streamed turn.
//!
//! Two rules here exist because the alternative is a silent lie to the caller:
//!
//! - **A chunk boundary is not an event boundary.** The wire may split `data: {...}` anywhere,
//!   including between the `\r` and the `\n` of a line ending, so the decoder holds bytes until
//!   a line and then an event are complete, and never parses a half-arrived one.
//! - **`finish_reason` is not a boolean.** `"content_filter"` means the provider refused the
//!   content; read as `"stop"` it ends a turn as if it had succeeded, and the user is told a
//!   refusal was an answer.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::model::{FunctionCall, ToolCall};

/// One dispatched server-sent event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// The `event:` field, when the stream sets one.
    pub event: Option<String>,
    /// The `data:` lines, joined with newlines as the format requires.
    pub data: String,
}

impl SseEvent {
    /// Whether this is the OpenAI-compatible end-of-stream sentinel.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// A line-buffered server-sent-event decoder.
///
/// Feed it whatever arrives; it returns the events that are now complete. Call [`Self::finish`]
/// once the transport has ended to flush an event the stream closed without a blank line.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
    data_lines: Vec<String>,
    event_name: Option<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes received so far, as text. Returns every event this chunk completed.
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        while let Some(line) = self.take_line() {
            if let Some(event) = self.accept_line(&line) {
                events.push(event);
            }
        }
        events
    }

    /// Flush at end of stream: a trailing incomplete line, then a pending event.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        // A lone `\r` at the very end is a terminator, not the start of a `\r\n`.
        while let Some(line) = self.take_line_eof() {
            if let Some(event) = self.accept_line(&line) {
                events.push(event);
            }
        }
        // The stream may also end mid-line, with no terminator at all.
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Some(event) = self.accept_line(&line) {
                events.push(event);
            }
        }
        if let Some(event) = self.dispatch() {
            events.push(event);
        }
        events
    }

    /// Take one complete line, holding a trailing `\r` back: the next chunk may complete a
    /// `\r\n`, and treating the `\r` as a terminator now would emit a spurious empty line —
    /// which the format reads as the end of an event.
    fn take_line(&mut self) -> Option<String> {
        self.take_line_inner(false)
    }

    fn take_line_eof(&mut self) -> Option<String> {
        self.take_line_inner(true)
    }

    fn take_line_inner(&mut self, at_eof: bool) -> Option<String> {
        let bytes = self.buffer.as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            match byte {
                b'\n' => {
                    let line = self.buffer[..index].to_string();
                    self.buffer.drain(..index + 1);
                    return Some(line);
                }
                b'\r' => {
                    if index + 1 < bytes.len() {
                        let width = if bytes[index + 1] == b'\n' { 2 } else { 1 };
                        let line = self.buffer[..index].to_string();
                        self.buffer.drain(..index + width);
                        return Some(line);
                    }
                    if at_eof {
                        let line = self.buffer[..index].to_string();
                        self.buffer.drain(..index + 1);
                        return Some(line);
                    }
                    return None;
                }
                _ => {}
            }
        }
        None
    }

    /// Apply one line to the event under construction.
    fn accept_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            // A comment, sent as a keep-alive. Nothing to carry.
            return None;
        }
        let (field, value) = match line.find(':') {
            Some(colon) => (
                &line[..colon],
                line[colon + 1..]
                    .strip_prefix(' ')
                    .unwrap_or(&line[colon + 1..]),
            ),
            None => (line, ""),
        };
        match field {
            "data" => self.data_lines.push(value.to_string()),
            "event" => self.event_name = Some(value.to_string()),
            // `id` and `retry` have no meaning for a chat completion.
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data_lines.is_empty() && self.event_name.is_none() {
            return None;
        }
        let data = self.data_lines.join("\n");
        let event = SseEvent {
            event: self.event_name.take(),
            data,
        };
        self.data_lines.clear();
        Some(event)
    }
}

/// What the model said this turn ended with.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FinishReason {
    /// The model finished speaking.
    Stop,
    /// The token budget ran out mid-sentence.
    Length,
    /// The model wants a tool run before it continues.
    ToolCalls,
    /// The provider refused the content. **Never** to be treated as `Stop`.
    ContentFilter,
    /// A reason this version does not know, carried through rather than guessed at.
    Other(String),
    /// Absent from the wire; only the end-of-stream sentinel ended the turn.
    #[default]
    Absent,
}

impl FinishReason {
    pub fn from_wire(reason: Option<&str>) -> Self {
        match reason {
            None => Self::Absent,
            Some("stop") => Self::Stop,
            Some("length") => Self::Length,
            Some("tool_calls") | Some("function_call") => Self::ToolCalls,
            Some("content_filter") => Self::ContentFilter,
            Some(other) => Self::Other(other.to_string()),
        }
    }

    /// The provider refused the content, so this turn is a failure, not an answer.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::ContentFilter)
    }

    /// The turn was cut off while still incomplete.
    pub fn is_truncated(&self) -> bool {
        matches!(self, Self::Length)
    }

    /// The model asked for tools, so the caller owes it a result.
    pub fn is_tool_call(&self) -> bool {
        matches!(self, Self::ToolCalls)
    }
}

/// One delta of one tool call, as the wire spells it. Every field is optional: `id` and `name`
/// arrive once, `arguments` arrives in fragments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct StreamDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct StreamChoice {
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub delta: StreamDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// One `data:` payload of a streamed chat completion.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
}

/// The finished turn: what the model said, what it asked for, and why it stopped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssistantTurn {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
}

/// Assembles a streamed turn from its deltas.
///
/// The only non-obvious rule: fragments are grouped by the delta's `index`, because a model
/// asking for two tools interleaves their fragments. An absent index falls back to the call
/// currently being built, which is what every provider that omits it means.
#[derive(Debug, Default)]
pub struct TurnAccumulator {
    content: String,
    calls: BTreeMap<u32, PartialCall>,
    finish_reason: FinishReason,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl TurnAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `data:` payload. Chunks with no choices (a keep-alive) change nothing.
    pub fn apply(&mut self, chunk: &StreamChunk) {
        // deepnest never asks for `n > 1`, so only the first choice carries the turn.
        let Some(choice) = chunk.choices.first() else {
            return;
        };
        if let Some(text) = &choice.delta.content {
            self.content.push_str(text);
        }
        if let Some(deltas) = &choice.delta.tool_calls {
            for delta in deltas {
                let slot = match delta.index {
                    Some(index) => index,
                    None => self.calls.keys().next_back().copied().unwrap_or(0),
                };
                let call = self.calls.entry(slot).or_default();
                if let Some(id) = &delta.id {
                    if !id.is_empty() {
                        call.id = id.clone();
                    }
                }
                if let Some(function) = &delta.function {
                    if let Some(name) = &function.name {
                        if !name.is_empty() {
                            call.name = name.clone();
                        }
                    }
                    if let Some(arguments) = &function.arguments {
                        call.arguments.push_str(arguments);
                    }
                }
            }
        }
        if let Some(reason) = &choice.finish_reason {
            self.finish_reason = FinishReason::from_wire(Some(reason.as_str()));
        }
    }

    /// Read the finish reason seen so far, without consuming the turn.
    pub fn finish_reason(&self) -> &FinishReason {
        &self.finish_reason
    }

    pub fn into_turn(self) -> AssistantTurn {
        let mut tool_calls: Vec<ToolCall> = self
            .calls
            .into_values()
            .map(|call| ToolCall {
                id: call.id,
                kind: "function".to_string(),
                function: FunctionCall {
                    name: call.name,
                    arguments: call.arguments,
                },
            })
            .collect();
        // A call the model started but never named is not callable; drop it rather than let a
        // nameless entry reach the tool dispatcher.
        tool_calls.retain(|call| !call.function.name.is_empty());
        AssistantTurn {
            content: if self.content.is_empty() {
                None
            } else {
                Some(self.content)
            },
            tool_calls,
            finish_reason: self.finish_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_split_across_chunks_is_parsed_once_whole() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("data: {\"a\"").is_empty());
        assert!(decoder.push(":1}\n").is_empty());
        let events = decoder.push("\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn a_crlf_split_between_cr_and_lf_does_not_invent_an_event_boundary() {
        // The failure this guard prevents: the trailing `\r` read as a terminator makes the
        // following `\n` an empty line, which ends the event early — two events where the wire
        // carried one, and a half-parsed first.
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("data: one\r").is_empty());
        let events = decoder.push("\ndata: two\r\n\r\n");
        assert_eq!(events.len(), 1, "one event, not two: {events:?}");
        assert_eq!(events[0].data, "one\ntwo");
    }

    #[test]
    fn comments_and_unknown_fields_carry_nothing() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(": keep-alive\nid: 7\nretry: 100\nevent: message\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn the_done_sentinel_is_recognised() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push("data: [DONE]\n\n");
        assert_eq!(events.len(), 1);
        assert!(events[0].is_done());
    }

    #[test]
    fn a_stream_that_closes_without_a_blank_line_still_yields_its_last_event() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push("data: tail").is_empty());
        let events = decoder.finish();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].data, "tail");
    }

    #[test]
    fn content_filter_is_not_stop() {
        assert_eq!(
            FinishReason::from_wire(Some("content_filter")),
            FinishReason::ContentFilter
        );
        assert!(FinishReason::from_wire(Some("content_filter")).is_refusal());
        assert!(!FinishReason::from_wire(Some("stop")).is_refusal());
        assert_eq!(FinishReason::from_wire(None), FinishReason::Absent);
        assert_eq!(
            FinishReason::from_wire(Some("function_call")),
            FinishReason::ToolCalls
        );
        assert_eq!(
            FinishReason::from_wire(Some("something_new")),
            FinishReason::Other("something_new".into())
        );
    }

    fn chunk(json: &str) -> StreamChunk {
        serde_json::from_str(json).expect("chunk parses")
    }

    #[test]
    fn tool_call_fragments_join_by_index_and_arguments_concatenate() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&chunk(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"call_a","function":{"name":"read_file","arguments":""}}]}}]}"#,
        ));
        acc.apply(&chunk(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
        ));
        // A second call interleaved, then both finished — the interleaving is what the index
        // grouping exists for.
        acc.apply(&chunk(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":1,"id":"call_b","function":{"name":"grep","arguments":"{}"}},
                {"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        ));
        acc.apply(&chunk(
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ));
        let turn = acc.into_turn();
        assert_eq!(turn.finish_reason, FinishReason::ToolCalls);
        assert!(turn.content.is_none());
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].id, "call_a");
        assert_eq!(turn.tool_calls[0].function.name, "read_file");
        assert_eq!(turn.tool_calls[0].function.arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(turn.tool_calls[1].function.name, "grep");
    }

    #[test]
    fn text_fragments_concatenate_in_order() {
        let mut acc = TurnAccumulator::new();
        for piece in ["Hel", "lo, ", "world"] {
            acc.apply(&chunk(&format!(
                r#"{{"choices":[{{"index":0,"delta":{{"content":"{piece}"}}}}]}}"#
            )));
        }
        let turn = acc.into_turn();
        assert_eq!(turn.content.as_deref(), Some("Hello, world"));
        assert_eq!(turn.finish_reason, FinishReason::Absent);
    }

    #[test]
    fn a_keepalive_chunk_changes_nothing() {
        let mut acc = TurnAccumulator::new();
        acc.apply(&chunk(r#"{"choices":[]}"#));
        let turn = acc.into_turn();
        assert!(turn.content.is_none());
        assert!(turn.tool_calls.is_empty());
    }
}
