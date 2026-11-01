//! The conversation, appended to `history.jsonl`.
//!
//! One JSON record per line, appended and flushed. Three consequences of that choice, all of
//! them the reason for it:
//!
//! - **A crash costs the last record, not the file.** A rewritten file that fails halfway loses
//!   the conversation, and the conversation is the one thing the user cannot regenerate.
//! - **The file stays greppable**, so the agent can search its own past with the `grep` tool.
//! - **Nothing has to be loaded to append.** A long-running app does not hold its history in
//!   memory just to answer one more message.
//!
//! The trim rule is the measured one: **by characters, not by entries.** Four records can be four
//! short sentences or four megabytes of `bash` output, and a rule that counts records will
//! happily send the megabytes. The trim keeps the system message and the tail, and it repairs
//! the two shapes that make a provider reject a request outright: a `tool` record whose
//! `assistant` call was dropped (an answer with no question), and a tail that starts with an
//! assistant or tool record (a conversation that does not begin with a user turn).

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use deepnest_llm::Message;
use serde::{Deserialize, Serialize};

/// A short marker for a record that carried no text, so the file is readable by eye.
const NO_TEXT: &str = "";

/// One line of `history.jsonl`.
///
/// Deliberately the wire shape plus a timestamp: a record that has to be translated to be read
/// is a record with two spellings, and two spellings eventually disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// What this line is. `"message"` for everything the model sees; other kinds are the
    /// agent's own notes and are not sent to the model.
    #[serde(rename = "type", default = "message_kind")]
    pub kind: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Pictures the message carried, each a `data:` URL.
    ///
    /// Stored rather than referenced: the conversation has to be the whole conversation when it is
    /// read back, and a path into a temporary directory is a picture that is gone by the time the
    /// next turn replays it. The cost of that choice is size, and it is paid where it is measured —
    /// in [`Record::chars`], which charges a picture the same fixed price whatever it weighs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<deepnest_llm::ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// How a tool call ended — `"ok"`, `"failed"` or `"refused"` — on the `tool` line that answers
    /// it, and absent everywhere else.
    ///
    /// Written to the file rather than inferred from the text: a refusal and a failure both arrive
    /// as an error whose wording is the tool's own, and a page trying to tell them apart by reading
    /// prose would be guessing. Absent on a line written before this field existed, which a reader
    /// has to treat as "ended, way unknown" rather than as success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// When it was written, UTC, to the second. Ordering is the file's, so this is for the reader
    /// and never for the sort.
    #[serde(default)]
    pub at: String,
}

fn message_kind() -> String {
    "message".to_string()
}

impl Record {
    /// The record a [`Message`] is written as.
    pub fn from_message(message: &Message) -> Self {
        Self {
            kind: message_kind(),
            role: message.role.clone(),
            content: message.content.clone(),
            images: message.images.clone(),
            tool_calls: message.tool_calls.clone(),
            tool_call_id: message.tool_call_id.clone(),
            outcome: None,
            at: now_iso(),
        }
    }

    /// The message this record is, or `None` when it is one of the agent's own notes.
    pub fn as_message(&self) -> Option<Message> {
        if self.kind != "message" {
            return None;
        }
        Some(Message {
            role: self.role.clone(),
            content: self.content.clone(),
            images: self.images.clone(),
            tool_calls: self.tool_calls.clone(),
            tool_call_id: self.tool_call_id.clone(),
        })
    }

    /// An agent note: kept in the file, never sent to the model.
    ///
    /// The cancellation and error notes use this, so a reader of the file sees why a turn ended
    /// while the model is not told a story it did not take part in.
    pub fn note(text: impl Into<String>) -> Self {
        Self {
            kind: "note".to_string(),
            role: "agent".to_string(),
            content: Some(text.into()),
            images: None,
            tool_calls: None,
            tool_call_id: None,
            outcome: None,
            at: now_iso(),
        }
    }

    /// What one picture is charged against the history budget.
    ///
    /// Not its length. A 1 MB PNG is about 1.4 million base64 characters, and the whole budget is
    /// [`crate::turn::Limits::max_history_chars`] — twenty times smaller — so charging the bytes
    /// would evict every word of the conversation the picture was sent with, and the next turn
    /// would arrive with a photograph and no history. What a picture really costs a provider is
    /// tokens, by area; about a thousand of them for a screen-sized image, which in this budget's
    /// currency is roughly this. An approximation, stated as one.
    pub const IMAGE_CHARS: usize = 4_000;

    /// The characters this record costs once it is sent.
    pub fn chars(&self) -> usize {
        let content = self.content.as_deref().unwrap_or(NO_TEXT).chars().count()
            + self.images.as_ref().map_or(0, Vec::len) * Self::IMAGE_CHARS;
        let calls: usize = self
            .tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .map(|call| {
                        call.function.name.chars().count() + call.function.arguments.chars().count()
                    })
                    .sum()
            })
            .unwrap_or(0);
        content + calls
    }
}

/// One app's conversation file.
#[derive(Debug, Clone)]
pub struct Session {
    path: PathBuf,
}

impl Session {
    /// Open (creating if needed) a conversation at this path.
    ///
    /// A file whose last write was torn — the process died mid-line — is repaired here by
    /// terminating the torn line. Without that, the next append lands on the end of it and one
    /// unreadable line becomes two records lost: the torn one and the good one after it.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let session = Self { path };
        session.terminate_torn_line()?;
        Ok(session)
    }

    /// Make sure the file ends at a line boundary.
    fn terminate_torn_line(&self) -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(());
        }
        file.seek(SeekFrom::Start(len - 1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
            file.flush()?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record and flush it to the disk.
    ///
    /// The flush is not an optimisation to skip: a record that sits in a buffer when the process
    /// dies is a record the user watched the agent write and then lost.
    pub fn append(&self, record: &Record) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.flush()
    }

    /// Append a message, returning the record that was written.
    pub fn append_message(&self, message: &Message) -> std::io::Result<Record> {
        let record = Record::from_message(message);
        self.append(&record)?;
        Ok(record)
    }

    /// Append a tool result, recording how the call ended beside it.
    ///
    /// The ending is not part of what the model is sent — a refusal and a failure are the same
    /// answer to it — so it lives on the record rather than on [`Message`]: the page shows what
    /// the file keeps, and the model is not handed a second story about its own call.
    pub fn append_result(&self, message: &Message, outcome: &str) -> std::io::Result<Record> {
        let mut record = Record::from_message(message);
        record.outcome = Some(outcome.to_string());
        self.append(&record)?;
        Ok(record)
    }

    /// Every record in the file.
    ///
    /// A line that does not parse is skipped rather than fatal: a file being appended to by a
    /// running agent can be read mid-line, and losing the whole conversation to one torn line is
    /// the opposite of what this store is for.
    pub fn records(&self) -> std::io::Result<Vec<Record>> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut records = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<Record>(&line) {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Every message in the file, in order — what the model is shown.
    pub fn messages(&self) -> std::io::Result<Vec<Message>> {
        Ok(self
            .records()?
            .iter()
            .filter_map(Record::as_message)
            .collect())
    }

    /// How many messages the file holds.
    pub fn message_count(&self) -> std::io::Result<usize> {
        Ok(self.messages()?.len())
    }
}

/// Fit a conversation into `budget` characters, keeping the tail.
///
/// The rules, in order, and each one answers a rejection a provider gives verbatim:
///
/// 1. the **system** message is always kept — it is the agent's instructions, not history;
/// 2. records are taken from the end backwards until the budget is spent, so what survives is
///    the recent conversation rather than the beginning of a long-forgotten one;
/// 3. the kept tail must **start with a user message**, because a conversation beginning with a
///    tool result is a request a provider refuses;
/// 4. a `tool` message whose `assistant` call was dropped is dropped too, because a tool result
///    with no call is an answer to a question that is not in the request.
///
/// `dropped` is returned so the caller can record that it happened: a silently shortened
/// conversation reads to the model as one that never had a beginning.
pub fn fit_tail(messages: &[Message], budget: usize) -> (Vec<Message>, usize) {
    let split = messages.iter().position(|message| message.role != "system");
    let (system, rest) = match split {
        Some(index) => (&messages[..index], &messages[index..]),
        None => (messages, &messages[..0]),
    };
    let system_chars: usize = system
        .iter()
        .map(|message| {
            message
                .content
                .as_deref()
                .unwrap_or(NO_TEXT)
                .chars()
                .count()
        })
        .sum();
    let mut remaining = budget.saturating_sub(system_chars);

    // Walk backwards, then repair.
    let mut start = rest.len();
    while start > 0 {
        let cost = message_chars(&rest[start - 1]);
        if cost > remaining {
            break;
        }
        remaining -= cost;
        start -= 1;
    }

    // Rule 3: the tail begins at a user turn.
    while start < rest.len() && rest[start].role != "user" {
        start += 1;
    }
    // Rule 4: no tool result without its call.
    let mut kept: Vec<Message> = Vec::with_capacity(rest.len() - start);
    let mut answered: Vec<String> = Vec::new();
    for message in &rest[start..] {
        if message.role == "assistant" {
            answered = message
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().map(|call| call.id.clone()).collect())
                .unwrap_or_default();
            kept.push(message.clone());
            continue;
        }
        if message.role == "tool" {
            let id = message.tool_call_id.clone().unwrap_or_default();
            if !answered.contains(&id) {
                // An answer with no question in the request: dropped, and the call it answered
                // stays as a call whose result the model never sees — which is legal, unlike the
                // reverse.
                continue;
            }
            kept.push(message.clone());
            continue;
        }
        kept.push(message.clone());
    }

    let mut out: Vec<Message> = system.to_vec();
    out.extend(kept);
    let kept_count = out.len();
    (out, messages.len().saturating_sub(kept_count))
}

fn message_chars(message: &Message) -> usize {
    let content = message
        .content
        .as_deref()
        .unwrap_or(NO_TEXT)
        .chars()
        .count();
    let calls: usize = message
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls
                .iter()
                .map(|call| {
                    call.function.name.chars().count() + call.function.arguments.chars().count()
                })
                .sum()
        })
        .unwrap_or(0);
    content + calls
}

/// The current UTC instant as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Hand-rolled rather than pulling a date crate in for one format: the civil-date conversion is
/// twenty lines and pinned by a test, and a dependency that exists only to print a timestamp is a
/// dependency to audit at every upgrade.
pub fn now_iso() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    iso_from_unix(seconds)
}

/// The same formatting for a given instant, so the format is testable without waiting.
pub fn iso_from_unix(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let in_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = in_day / 3_600;
    let minute = (in_day % 3_600) / 60;
    let second = in_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's days-to-civil conversion: exact for every day the calendar has.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "deepnest-agent-session-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn session(&self) -> Session {
            Session::open(self.base.join("history.jsonl")).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_message_survives_a_round_trip_through_the_file() {
        let scratch = Scratch::new("roundtrip");
        let session = scratch.session();
        session
            .append_message(&Message::user("hello there"))
            .unwrap();
        let messages = session.messages().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content.as_deref(), Some("hello there"));
    }

    #[test]
    fn append_keeps_every_earlier_record() {
        // The failure this prevents: a rewritten file that fails halfway loses the conversation.
        let scratch = Scratch::new("append");
        let session = scratch.session();
        for index in 0..5 {
            session
                .append_message(&Message::user(format!("turn {index}")))
                .unwrap();
        }
        let messages = session.messages().unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].content.as_deref(), Some("turn 0"));
        assert_eq!(messages[4].content.as_deref(), Some("turn 4"));
    }

    #[test]
    fn a_tool_call_and_its_result_both_survive() {
        let scratch = Scratch::new("tool-call");
        let session = scratch.session();
        let call = deepnest_llm::ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: deepnest_llm::FunctionCall {
                name: "read".into(),
                arguments: "{\"file_path\":\"a.txt\"}".into(),
            },
        };
        session
            .append_message(&Message::assistant_with_tools(None, vec![call]))
            .unwrap();
        session
            .append_message(&Message::tool("call_1", "the file"))
            .unwrap();
        let messages = session.messages().unwrap();
        assert_eq!(messages[0].tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn an_agent_note_is_written_but_is_not_a_message() {
        let scratch = Scratch::new("note");
        let session = scratch.session();
        session
            .append(&Record::note("turn cancelled by the host"))
            .unwrap();
        let records = session.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, "note");
        assert!(
            session.messages().unwrap().is_empty(),
            "a note must not reach the model"
        );
    }

    #[test]
    fn a_torn_line_is_skipped_rather_than_losing_the_whole_file() {
        let scratch = Scratch::new("torn");
        let session = scratch.session();
        session.append_message(&Message::user("before")).unwrap();
        // A process killed mid-write leaves a line with no terminator. The next start repairs
        // it, so the record after it is not lost with it.
        let mut file = OpenOptions::new()
            .append(true)
            .open(session.path())
            .unwrap();
        file.write_all(b"{\"type\":\"message\",\"role\":\"us")
            .unwrap();
        let session = Session::open(session.path()).unwrap();
        session.append_message(&Message::user("after")).unwrap();
        let messages = session.messages().unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[1].content.as_deref(), Some("after"));
    }

    #[test]
    fn an_empty_file_reads_as_no_messages() {
        let scratch = Scratch::new("empty");
        let session = session_without_writing(&scratch);
        assert!(session.messages().unwrap().is_empty());
    }

    fn session_without_writing(scratch: &Scratch) -> Session {
        Session {
            path: scratch.base.join("absent/history.jsonl"),
        }
    }

    #[test]
    fn the_tail_is_kept_and_the_beginning_is_what_goes() {
        let messages: Vec<Message> = (0..10)
            .map(|index| Message::user(format!("message {index} {}", "x".repeat(90))))
            .collect();
        let (kept, dropped) = fit_tail(&messages, 300);
        assert!(dropped > 0, "something had to go");
        assert_eq!(
            kept.last().unwrap().content.as_deref(),
            messages.last().unwrap().content.as_deref(),
            "the newest message survives"
        );
        assert!(kept.len() < messages.len());
    }

    #[test]
    fn the_budget_counts_characters_and_not_entries() {
        // Four entries, four megabytes: an entry-counting rule sends all of it.
        let big = "y".repeat(10_000);
        let messages = vec![
            Message::system("instructions"),
            Message::user("hi"),
            Message::assistant(big.clone()),
            Message::user(big),
        ];
        let (kept, dropped) = fit_tail(&messages, 15_000);
        assert!(
            dropped > 0,
            "the fourth message alone is over budget with the first"
        );
        let chars: usize = kept
            .iter()
            .map(|message| message.content.as_deref().unwrap_or("").chars().count())
            .sum();
        assert!(chars <= 15_000, "{chars} is over budget");
    }

    #[test]
    fn the_system_message_is_never_dropped() {
        let messages = vec![
            Message::system("you are the app agent"),
            Message::user("x".repeat(5_000)),
        ];
        let (kept, _) = fit_tail(&messages, 10);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].role, "system");
    }

    #[test]
    fn a_tail_that_would_start_with_a_tool_result_starts_at_the_next_user_turn() {
        // A request whose first non-system message is a tool result is refused outright.
        let call = deepnest_llm::ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: deepnest_llm::FunctionCall {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        };
        let messages = vec![
            Message::user("old turn"),
            Message::assistant_with_tools(None, vec![call.clone()]),
            Message::tool("c1", "result"),
            Message::user("new turn"),
        ];
        // Enough for the tool result and its call, not enough to reach the earlier user turn:
        // the natural tail would begin with the tool result, which a provider refuses.
        let (kept, _) = fit_tail(&messages, 20);
        assert_eq!(kept[0].role, "user");
        assert_eq!(kept[0].content.as_deref(), Some("new turn"));
    }

    #[test]
    fn a_tool_result_whose_call_was_dropped_is_dropped_too() {
        let call = deepnest_llm::ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: deepnest_llm::FunctionCall {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        };
        // The assistant call is over budget, the result is not: kept alone it would be an answer
        // with no question.
        let messages = vec![
            Message::user("go"),
            Message::assistant_with_tools(Some("z".repeat(5_000)), vec![call]),
            Message::tool("c1", "ok"),
        ];
        let (kept, _) = fit_tail(&messages, 100);
        assert!(
            !kept.iter().any(|message| message.role == "tool"),
            "{kept:?}"
        );
    }

    #[test]
    fn the_timestamp_is_a_utc_instant() {
        // 2026-10-31T00:00:00Z is what the epoch arithmetic has to produce, not a plausible date.
        assert_eq!(iso_from_unix(1_793_404_800), "2026-10-31T00:00:00Z");
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_unix(1_000_000_000), "2001-09-09T01:46:40Z");
        assert!(now_iso().ends_with('Z'));
    }

    const PIXEL: &str = "data:image/png;base64,iVBORw0KGgo=";

    #[test]
    fn a_picture_survives_a_round_trip_through_the_file() {
        // A screenshot the agent cannot read back is a screenshot that vanished after one turn:
        // the model sees it once, and every later request replays a conversation with a hole in it.
        let scratch = Scratch::new("picture-roundtrip");
        let session = scratch.session();
        session
            .append_message(&Message::with_images(
                Some("what is this?".to_string()),
                vec![PIXEL.to_string()],
            ))
            .unwrap();
        let messages = session.messages().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content.as_deref(), Some("what is this?"));
        assert_eq!(
            messages[0].images.as_deref(),
            Some([PIXEL.to_string()].as_slice())
        );
    }

    #[test]
    fn a_file_written_before_pictures_existed_still_reads() {
        // The claim that makes this shippable: `images` is an added key with a default, so every
        // conversation already on a user's disk opens with its words intact and its pictures, of
        // which it had none, absent rather than an error.
        let scratch = Scratch::new("old-format");
        let path = scratch.base.join("history.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"message","role":"user","content":"hello"}"#,
                "\n",
                r#"{"type":"message","role":"assistant","content":"hi"}"#,
                "\n",
            ),
        )
        .unwrap();
        let session = Session::open(path).unwrap();
        let messages = session.messages().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content.as_deref(), Some("hello"));
        assert!(messages[0].images.is_none());
    }

    #[test]
    fn a_picture_is_charged_a_price_rather_than_its_length() {
        // One photographed page is about 1.4 million base64 characters, and the whole history
        // budget is 120 000: charging the bytes would evict every word the picture was sent with,
        // and the next turn would arrive with a photograph and no conversation. So a picture costs
        // a constant, and the constant is what this test pins.
        let photo = format!("data:image/png;base64,{}", "A".repeat(1_400_000));
        let record =
            Record::from_message(&Message::with_images(Some("look".to_string()), vec![photo]));
        assert_eq!(record.chars(), 4 + Record::IMAGE_CHARS);
        assert!(Record::IMAGE_CHARS * 20 < crate::turn::Limits::default().max_history_chars);
    }

    #[test]
    fn a_record_counts_the_characters_it_will_cost() {
        let record = Record::from_message(&Message::assistant("12345"));
        assert_eq!(record.chars(), 5);
        let call = deepnest_llm::ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: deepnest_llm::FunctionCall {
                name: "read".into(),
                arguments: "ab".into(),
            },
        };
        let with_call = Record::from_message(&Message::assistant_with_tools(None, vec![call]));
        assert_eq!(with_call.chars(), 6, "name plus arguments");
    }
}
