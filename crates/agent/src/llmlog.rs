//! The model log: every request this app's agent sent, and what came back.
//!
//! `history.jsonl` says what the conversation *is* — the messages the model is sent, in order.
//! This file says what each request *cost* and what the endpoint *answered*, which is a different
//! question and one the conversation cannot answer: a token count is not derivable from the text,
//! the same history is charged again on every round it is replayed in, and a `400` that the loop
//! then shrank away leaves no trace in a conversation that recovered.
//!
//! Two records per exchange, in the shape the sibling agent already writes:
//!
//! - **`request`** — the model, the messages, the tools, and whether the answer was streamed. This
//!   is the whole body, because "what did we actually send" is the question a wrong answer sends
//!   you back to, and a file that says only "we asked something" answers nothing.
//! - **`response`** — the text, the tool calls it asked for, why it stopped, the HTTP status, the
//!   usage the provider reported, or the failure when there was no answer at all.
//!
//! **Rotation is part of the design, not housekeeping.** A request record carries the whole context
//! the model was sent, so this file grows at roughly the rate of a conversation multiplied by the
//! rounds in it — hundreds of megabytes on a long-lived app. The same rule the sibling agent uses
//! applies here: past [`MAX_BYTES`] the file becomes `.1`, the old `.1` becomes `.2`, and the old
//! `.2` is dropped. Two backups and no more, because one turn's worth of context is the interesting
//! window and three of them is already more than anybody reads.
//!
//! **A log that cannot be written never fails a turn.** This is a diagnostic, and an app that
//! stopped answering because its log directory was full would be trading the work for the record
//! of it.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::session::now_iso;

/// Past this size the file is rotated. 50 MB, the number the sibling agent settled on.
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// How many rotated files are kept beside the live one.
pub const BACKUPS: usize = 2;

/// One app's model log.
#[derive(Debug, Clone)]
pub struct LlmLog {
    path: PathBuf,
}

/// One request, as it went out.
///
/// Borrowed rather than owned: this is written once, immediately, and copying a context that can be
/// a hundred thousand characters to hand it to a file would double the cost of logging it.
#[derive(Debug, Serialize)]
pub struct Request<'a> {
    pub at: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub model: &'a str,
    pub messages: &'a [deepnest_llm::Message],
    pub tools: &'a [deepnest_llm::ToolDefinition],
}

/// What came back, or why nothing did.
#[derive(Debug, Serialize)]
pub struct Response<'a> {
    pub at: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<&'a [deepnest_llm::ToolCall]>,
    /// The provider's own word — `stop`, `tool_calls`, `length`, `content_filter`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<&'a str>,
    /// The status when the provider answered with one. Absent for a transport fault, which is a
    /// request that never got a status at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// What the provider said the request cost. Zero-shaped when it did not say — see
    /// [`deepnest_llm::Usage::reported`].
    pub usage: deepnest_llm::Usage,
    /// The failure, in the provider's own words, when there was no answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
}

impl Request<'_> {
    /// A request record, stamped now.
    pub fn now<'a>(
        model: &'a str,
        messages: &'a [deepnest_llm::Message],
        tools: &'a [deepnest_llm::ToolDefinition],
    ) -> Request<'a> {
        Request {
            at: now_iso(),
            kind: "request",
            model,
            messages,
            tools,
        }
    }
}

impl<'a> Response<'a> {
    /// A response record that carries an answer.
    pub fn answered(turn: &'a deepnest_llm::AssistantTurn) -> Response<'a> {
        Response {
            at: now_iso(),
            kind: "response",
            content: turn.content.as_deref(),
            tool_calls: if turn.tool_calls.is_empty() {
                None
            } else {
                Some(&turn.tool_calls)
            },
            finish_reason: Some(turn.finish_reason.as_str()),
            http_status: Some(200),
            usage: turn.usage,
            error: None,
        }
    }

    /// A response record that carries a failure instead.
    pub fn failed(error: &'a deepnest_llm::LlmError) -> Response<'a> {
        Response {
            at: now_iso(),
            kind: "response",
            content: None,
            tool_calls: None,
            finish_reason: None,
            http_status: error.status,
            usage: deepnest_llm::Usage::default(),
            error: Some(&error.body),
        }
    }
}

impl LlmLog {
    /// The log at this path. Nothing is created until the first record is written.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write one record, rotating first if the file has grown past [`MAX_BYTES`].
    ///
    /// One `write_all` of one line: a reader tailing the file sees whole records or a torn last
    /// line, which is the same guarantee `history.jsonl` gives and the reason both are line files.
    pub fn append(&self, record: &impl Serialize) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        rotate(&self.path)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.flush()
    }
}

/// Shift `llm.jsonl` → `.1` → `.2` when the live file is at or past [`MAX_BYTES`].
///
/// A failed rename is not worth failing a write over: the log keeps growing in place, which is what
/// it was doing anyway.
fn rotate(path: &Path) -> std::io::Result<()> {
    let size = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        // No file yet is the ordinary first write, not a problem to report.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if size < MAX_BYTES {
        return Ok(());
    }
    // Exactly [`BACKUPS`] files, which is one more than shifts: the oldest is dropped, everything
    // else moves one generation down, and the live file becomes `.1`. Dropping first is what makes
    // the count hold — shifting into a free slot leaves one more file than the constant promises.
    let _ = fs::remove_file(suffixed(path, BACKUPS));
    for index in (1..BACKUPS).rev() {
        let from = suffixed(path, index);
        if from.exists() {
            fs::rename(&from, suffixed(path, index + 1))?;
        }
    }
    fs::rename(path, suffixed(path, 1))
}

/// `llm.jsonl` and `llm.jsonl.1` — the suffix goes after the whole name, not before the extension,
/// so `ls` groups the family together.
fn suffixed(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    use deepnest_llm::{FinishReason, Usage};

    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "deepnest-llmlog-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn turn(content: &str) -> deepnest_llm::AssistantTurn {
        deepnest_llm::AssistantTurn {
            content: Some(content.to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 1_200,
                completion_tokens: 40,
                total_tokens: 1_240,
                cached_tokens: Some(1_024),
            },
        }
    }

    /// The file is one record per line, and a record says what it cost — the whole reason this
    /// exists is that a token count cannot be worked out from the conversation afterwards.
    #[test]
    fn an_exchange_is_two_lines_and_the_answer_says_what_it_cost() {
        let scratch = Scratch::new("exchange");
        let log = LlmLog::new(scratch.root.join("llm.jsonl"));

        let messages = vec![deepnest_llm::Message::user("hello")];
        log.append(&Request::now("m", &messages, &[])).unwrap();
        let answer = turn("hi");
        log.append(&Response::answered(&answer)).unwrap();

        let text = fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");

        let request: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(request["type"], "request");
        assert_eq!(request["model"], "m");
        assert_eq!(request["messages"][0]["content"], "hello");
        assert!(
            request["at"].as_str().is_some_and(|at| at.ends_with('Z')),
            "a record is stamped in UTC: {text}"
        );

        let response: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(response["type"], "response");
        assert_eq!(response["content"], "hi");
        assert_eq!(response["finish_reason"], "stop");
        assert_eq!(response["http_status"], 200);
        assert_eq!(response["usage"]["prompt_tokens"], 1_200);
        assert_eq!(response["usage"]["completion_tokens"], 40);
        assert_eq!(
            response["usage"]["cached_tokens"], 1_024,
            "the cache count is what says whether a replayed history is being paid for twice"
        );
    }

    /// A failure is a record too: the loop shrinks an over-long request and carries on, so a
    /// conversation that recovered says nothing about the 400 that made it.
    #[test]
    fn a_failure_is_written_with_the_status_and_the_providers_own_words() {
        let scratch = Scratch::new("failure");
        let log = LlmLog::new(scratch.root.join("llm.jsonl"));
        let error = deepnest_llm::LlmError::classify(Some(400), "context length exceeded");
        log.append(&Response::failed(&error)).unwrap();

        let text = fs::read_to_string(log.path()).unwrap();
        let record: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(record["type"], "response");
        assert_eq!(record["http_status"], 400);
        assert_eq!(record["error"], "context length exceeded");
        assert!(
            record.get("content").is_none(),
            "a failure answers nothing and must not claim it did: {text}"
        );
        assert_eq!(
            record["usage"]["prompt_tokens"], 0,
            "a request that failed has no cost to report"
        );
    }

    /// Two backups and no more: the live file moves to `.1`, the old `.1` to `.2`, and the old `.2`
    /// is dropped rather than becoming a third.
    #[test]
    fn the_log_rotates_into_two_backups_and_drops_the_third() {
        let scratch = Scratch::new("rotate");
        let log = LlmLog::new(scratch.root.join("llm.jsonl"));

        // Three generations of content, each made large enough to be over the limit on its own.
        for generation in 1..=3 {
            let filler = "x".repeat(MAX_BYTES as usize + 1);
            fs::write(log.path(), format!("{generation}:{filler}")).unwrap();
            log.append(&Response::failed(&deepnest_llm::LlmError::classify(
                None, "fault",
            )))
            .unwrap();
        }

        assert!(log.path().exists(), "the live file is back");
        assert!(
            suffixed(log.path(), 1).exists(),
            ".1 holds the generation before"
        );
        assert!(
            suffixed(log.path(), 2).exists(),
            ".2 holds the one before that"
        );
        assert!(
            !suffixed(log.path(), 3).exists(),
            "a third backup is not kept"
        );

        let newest = fs::read_to_string(suffixed(log.path(), 1)).unwrap();
        assert!(
            newest.starts_with("3:"),
            "the file that was live before this write is what .1 holds: {}",
            &newest[..2]
        );
    }
}
