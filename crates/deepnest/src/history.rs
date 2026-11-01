//! The conversation an app's agent has had, read back for the page.
//!
//! The dialog could show only the answer to the message just sent, and that turned out to be the
//! wrong shape twice over. A turn that ends by rewriting the app reloads the page, so the one
//! sentence explaining what happened was destroyed by the thing it was explaining; and a turn that
//! spends its whole budget on tools has no answer at all, which is indistinguishable on screen from
//! a turn that never ran.
//!
//! So the conversation is read from the file the agent already writes ([`crates/agent`'s
//! `history.jsonl`]) rather than held in the page. Nothing new is stored: the record of what
//! happened was always there, and what was missing was a way to ask for it.
//!
//! Three decisions:
//!
//! - **A turn is the unit, not a message.** The file is a flat list — a user line, then dozens of
//!   assistant/tool pairs. Rendered as-is, one question produces a wall of tool chatter; grouped,
//!   it produces "you asked this, it ran 12 tools, here is what it said" and the notes beside it.
//! - **The agent's own notes are part of the transcript.** `the round budget of 24 ran out` is
//!   what the user needed to read and could not; a note is written for a reader of the file, and
//!   the page is that reader.
//! - **A turn is readable while it runs.** The agent flushes a record per round, so a reader who
//!   asks mid-turn sees the turn grow: [`Turn::rounds`] and [`Turn::tools`] climb as the model is
//!   asked again and the calls are made. That is what lets the page say what is happening instead
//!   of only that something is, and it needs nothing beyond this file — no progress channel, no
//!   second copy of the state.
//! - **A turn's tool calls are shown one row each.** "ran 12 tools" is a count, and a count is
//!   the one thing a person cannot check: which files were read, what the shell was told to run,
//!   and whether a call was denied rather than attempted are all in the file already — the model
//!   asked for them by name and arguments, and each answer is a line of its own. So the route
//!   carries them, and the page draws them.
//! - **A missing or unreadable line is an empty conversation, not a failure.** A page that cannot
//!   show what happened is worse than a page showing "nothing yet", and a `500` in a dialog that
//!   has just been opened says the app is broken when it may only be new.

use std::path::Path;

use deepnest_agent::Record;
use serde::Serialize;

/// The route this module answers — the same spelling the router routes on.
pub const ACTION: &str = "history";

/// Where the agent writes the conversation, relative to the app directory.
const HISTORY: &str = ".agent/history.jsonl";

/// One exchange: what the user asked, and everything the agent did about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Turn {
    /// What the person typed.
    pub user: String,
    /// The agent's last prose in this turn, or empty when it never wrote any.
    pub reply: String,
    /// How many tool calls the agent made about it — the number a reader has no other way to see.
    pub tools: usize,
    /// How many pictures were pasted with the question.
    ///
    /// A count and not the pictures themselves, and the reason is the route's own shape: a page
    /// polls this every second while a turn runs, and one screenshot's `data:` URL is about 1.4
    /// million characters, so a transcript that carried them would answer with megabytes a second
    /// to draw a thumbnail. The pictures stay in the conversation file for the model's own replay;
    /// what a reader of the transcript needs is that there was one.
    pub pictures: usize,
    /// How many files the question named, counted the same way and for the same reason.
    ///
    /// The message the model was shown carries the files' contents, because a replay has to hand
    /// the model the same thing twice; the page is showing a person what they said, and what they
    /// said is the sentence with `[file report.csv]` in it. So the contents are cut off at
    /// [`crate::chat::FILE_SECTION`] and counted here instead — one line of chat rather than a csv
    /// poured into it.
    pub files: usize,
    /// How many times the model was asked in this turn.
    ///
    /// One more than a tool call count is not the same thing, and it is the number that says a
    /// turn is working rather than stuck: a round with no calls is the model thinking, and a turn
    /// that has stopped growing is the one worth telling the user about.
    pub rounds: usize,
    /// The agent's own notes, in order: why a turn ended early, what it could not run.
    pub notes: Vec<String>,
    /// What the agent ran about it, in the order it asked.
    pub calls: Vec<Call>,
    /// When the user's message was written, UTC to the second, as the file wrote it.
    pub at: String,
}

/// One tool call: what was asked, of which tool, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Call {
    /// The tool's name, exactly as the model asked for it.
    pub name: String,
    /// The one argument that says what the call was about — the command, the path, the pattern.
    ///
    /// One and not all of them: a row is a line, and the arguments of a call that names a file and
    /// a line range read as a paragraph. [`what_of`] says why this one and not another.
    pub what: String,
    /// How it ended: `ok`, `failed`, `refused`, or `done` when the file records an ending but not
    /// which one. Empty while the call has no answer yet — a call the model has just asked for,
    /// which is what a turn looks like while it runs.
    pub state: String,
    /// What the tool answered, cut to what a page polling once a second can carry.
    pub output: String,
    /// The id that ties this row to the answer it is waiting for. The page has no use for it.
    #[serde(skip)]
    pub id: String,
}

/// How much of one call's output travels to the page.
///
/// The route is polled every second while a turn runs, so this is a rate as much as a size: 1,200
/// characters per call is a glance at what the tool said, and the whole of it is in the file for
/// anyone who wants it. A call that is cut says so, in the cut text itself.
const CALL_OUTPUT_CHARS: usize = 1_200;

/// How much tool output one turn may carry in total.
///
/// A turn can make a hundred calls, and a transcript that answered with all of them every second
/// would be the page slowing the turn it is watching. Past this, a row still says which call it
/// was and how it ended; only its output is left out, and the row says that too.
const TURN_OUTPUT_CHARS: usize = 12_000;

/// How much of the identifying argument a row shows.
const WHAT_CHARS: usize = 80;

/// The argument that best says what a call is about, in the order they are tried.
///
/// Ordered rather than "the first string in the object", because the model does not order its own
/// arguments consistently and `{"line": 12, "path": "src/main.rs"}` would otherwise be a row that
/// says `12`.
const WHAT_KEYS: [&str; 7] = ["command", "path", "pattern", "query", "url", "name", "slug"];

/// Read `app_dir`'s conversation, oldest turn first.
///
/// Never an error: a missing file is a conversation with nothing in it, which is what an app
/// nobody has spoken to has. Records written before the last one are trusted to be complete
/// because the agent appends whole lines; a last line that does not parse is dropped rather than
/// turning the whole transcript into a failure.
pub fn read(app_dir: &Path) -> Vec<Turn> {
    let Ok(text) = std::fs::read_to_string(app_dir.join(HISTORY)) else {
        return Vec::new();
    };
    let mut turns: Vec<Turn> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            // A torn last line is the expected shape of a file that is being written while it is
            // read: the agent appends whole lines, so a partial one can only be the newest.
            continue;
        };
        push(&mut turns, &record);
    }
    cut_outputs(&mut turns);
    turns
}

/// Cut what each call carries down to what a page polled every second can afford, saying where.
///
/// Two cuts, and both name themselves in the text: a long output is cut to
/// [`CALL_OUTPUT_CHARS`], and once a turn has carried [`TURN_OUTPUT_CHARS`] in total the calls
/// after it keep their name, their argument and their ending but lose their output. The file is
/// untouched by either — this is what the page is sent, not what was said.
fn cut_outputs(turns: &mut [Turn]) {
    for turn in turns.iter_mut() {
        let mut spent = 0;
        for call in turn.calls.iter_mut() {
            if call.output.is_empty() {
                continue;
            }
            let total = call.output.chars().count();
            if spent + total.min(CALL_OUTPUT_CHARS) > TURN_OUTPUT_CHARS {
                call.output = CUT_FROM_THE_TURN.to_string();
                continue;
            }
            if total > CALL_OUTPUT_CHARS {
                let kept: String = call.output.chars().take(CALL_OUTPUT_CHARS).collect();
                call.output =
                    format!("{kept}\n[cut: showed {CALL_OUTPUT_CHARS} of {total} characters]");
            }
            spent += call.output.chars().count();
        }
    }
}

/// What a call's output says when the turn around it had already carried its share.
const CUT_FROM_THE_TURN: &str =
    "[cut from the transcript: this turn made more calls than one page carries, and the file has all of it]";

/// The person's own words, with the file contents the model was given cut away.
///
/// The cut is at [`crate::chat::FILE_SECTION`], the line that assembles the two halves in the first
/// place, so the two cannot drift: a message with no files has no section and is returned whole.
fn words_of(content: &str) -> String {
    match content.find(crate::chat::FILE_SECTION) {
        Some(at) => content[..at].to_string(),
        None => content.to_string(),
    }
}

/// How many distinct files the words name, read out of the markers exactly as the dialog reads them.
///
/// A marker repeated is still one file, and a marker the person cut out is a file that was not sent
/// — both because that is what the request carried.
fn files_named(content: &str) -> usize {
    let words = words_of(content);
    let mut names: Vec<&str> = Vec::new();
    let mut rest = words.as_str();
    while let Some(start) = rest.find("[file ") {
        let after = &rest[start + "[file ".len()..];
        let Some(end) = after.find(']') else {
            break;
        };
        let name = &after[..end];
        if !names.contains(&name) {
            names.push(name);
        }
        rest = &after[end + 1..];
    }
    names.len()
}

/// The one argument that says what a call was about.
///
/// A call's arguments are a JSON object written by the model, so this reads them rather than
/// quoting them: a row is a line, and `{"path":"a.rs","line":12,"limit":40}` on one line is a
/// sentence nobody parses twice. Arguments that are not an object — the model sometimes sends a
/// bare string — are shown as they arrived.
fn what_of(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return cut(arguments, WHAT_CHARS);
    };
    for key in WHAT_KEYS {
        if let Some(text) = value.get(key).and_then(|value| value.as_str()) {
            return cut(text, WHAT_CHARS);
        }
    }
    if let Some(map) = value.as_object() {
        for value in map.values() {
            if let Some(text) = value.as_str() {
                return cut(text, WHAT_CHARS);
            }
        }
    }
    cut(arguments, WHAT_CHARS)
}

/// `text`, cut to `budget` characters, saying so when it was.
fn cut(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let kept: String = text.chars().take(budget).collect();
    format!("{kept}…")
}

/// Where one record belongs in the transcript.
fn push(turns: &mut Vec<Turn>, record: &Record) {
    match record.role.as_str() {
        // A question starts a turn. Text before any question — a system message, which is the
        // agent's instructions and not the conversation — is not part of what the page shows.
        "user" => turns.push(Turn {
            // A message that was only pictures has no content at all — `Message::with_images`
            // filters blank text to `None` — so this is empty rather than missing, and `pictures`
            // below is what says the message was not nothing.
            user: words_of(&record.content.clone().unwrap_or_default()),
            pictures: record.images.as_ref().map_or(0, Vec::len),
            files: files_named(&record.content.clone().unwrap_or_default()),
            reply: String::new(),
            tools: 0,
            rounds: 0,
            notes: Vec::new(),
            calls: Vec::new(),
            at: record.at.clone(),
        }),
        "assistant" => {
            let Some(turn) = turns.last_mut() else {
                return;
            };
            turn.rounds += 1;
            turn.tools += record.tool_calls.as_ref().map_or(0, Vec::len);
            // The calls this round asked for, in the order it asked: a row each, waiting for the
            // answer that will give it its ending.
            for asked in record.tool_calls.iter().flatten() {
                turn.calls.push(Call {
                    name: asked.function.name.clone(),
                    what: what_of(&asked.function.arguments),
                    state: String::new(),
                    output: String::new(),
                    id: asked.id.clone(),
                });
            }
            // Only the newest prose is the answer; an agent that writes a line before each tool
            // call would otherwise have a transcript of its own narration.
            if let Some(text) = record
                .content
                .as_ref()
                .filter(|text| !text.trim().is_empty())
            {
                turn.reply = text.clone();
            }
        }
        // A tool's answer names the call it belongs to, so it is a field on that row rather than
        // a row of its own — a call and its result are one thing that happened.
        "tool" => {
            let Some(turn) = turns.last_mut() else {
                return;
            };
            let Some(id) = record.tool_call_id.as_ref() else {
                return;
            };
            let Some(call) = turn
                .calls
                .iter_mut()
                .find(|call| &call.id == id && call.state.is_empty())
            else {
                return;
            };
            // `done` and not `ok` when the file predates the outcome field: the call ended and
            // the file does not say how, and a page that called that a success would be
            // inventing the one thing this field exists to state.
            call.state = record.outcome.clone().unwrap_or_else(|| "done".to_string());
            call.output = record.content.clone().unwrap_or_default();
        }
        "agent" => {
            if let (Some(turn), Some(note)) = (turns.last_mut(), record.content.as_ref()) {
                turn.notes.push(note.clone());
            }
        }
        _ => {}
    }
}

/// The body of the `history` route: the conversation, oldest first.
pub fn body(app_dir: &Path) -> String {
    let turns = read(app_dir);
    serde_json::json!({ "turns": turns }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-history-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(base.join(".agent")).unwrap();
            Self { base }
        }

        fn write(&self, lines: &[&str]) {
            fs::write(self.base.join(HISTORY), lines.join("\n") + "\n").unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    const SYSTEM: &str = r#"{"type":"message","role":"system","content":"You are the agent","at":"2026-11-01T01:00:00Z"}"#;
    const ASK: &str = r#"{"type":"message","role":"user","content":"build a todo list","at":"2026-11-01T01:00:01Z"}"#;
    const TOOLS: &str = r#"{"type":"message","role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}},{"id":"c2","type":"function","function":{"name":"read","arguments":"{}"}}],"at":"2026-11-01T01:00:02Z"}"#;
    const RESULT: &str = r#"{"type":"message","role":"tool","content":"ok","tool_call_id":"c1","at":"2026-11-01T01:00:03Z"}"#;
    const REPLY: &str = r#"{"type":"message","role":"assistant","content":"Done: three files.","at":"2026-11-01T01:00:04Z"}"#;
    const NOTE: &str = r#"{"type":"note","role":"agent","content":"the round budget of 24 ran out with the model still asking for work","at":"2026-11-01T01:00:05Z"}"#;

    #[test]
    fn an_app_nobody_has_spoken_to_has_an_empty_conversation() {
        // The state every newly created app is in, and the one a `500` would misreport as broken.
        let scratch = Scratch::new("empty");
        let _ = fs::remove_file(scratch.base.join(HISTORY));
        assert!(read(&scratch.base).is_empty());
        assert_eq!(body(&scratch.base), r#"{"turns":[]}"#);
    }

    #[test]
    fn a_question_that_named_files_is_read_back_as_words_and_a_count() {
        // The record holds what the model was given — the words and the files' contents — because a
        // replay has to hand it the same thing twice. The page is showing a person their own
        // sentence, so the contents are cut at the line `chat::with_files` put them behind, and
        // what is left is the count.
        let scratch = Scratch::new("files");
        let words = "the total in [file report.csv] looks wrong, see [file totals.csv] too";
        // Spelled here rather than by calling `chat::with_files`: this file reads what is on disk,
        // and a test that built its input with the function under test would agree with itself.
        let content = format!(
            "{words}{}",
            crate::chat::FILE_SECTION.to_string()
                + "\n[file report.csv] — 12 bytes\n```\nid,total\n1,2\n```\n"
        );
        let line = serde_json::json!({
            "type": "message",
            "role": "user",
            "content": content,
            "at": "2026-11-01T02:00:00Z",
        })
        .to_string();
        scratch.write(&[&line]);

        let turns = read(&scratch.base);
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].user, words,
            "the file's contents are in the chat line"
        );
        assert_eq!(turns[0].files, 2, "both markers count, and one each");
        assert!(turns[0].pictures == 0);

        // A repeated marker is one file, because one file pointed at twice is still one file.
        assert_eq!(files_named("[file a.csv] and again [file a.csv]"), 1);
        // And a message with no files is returned whole, section or no section.
        assert_eq!(words_of("just words"), "just words");
    }

    #[test]
    fn a_question_and_its_answer_are_one_turn() {
        let scratch = Scratch::new("one");
        scratch.write(&[SYSTEM, ASK, TOOLS, RESULT, REPLY]);
        let turns = read(&scratch.base);
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert_eq!(turns[0].user, "build a todo list");
        assert_eq!(turns[0].reply, "Done: three files.");
        // Two calls on the assistant line, and the tool results are not counted again: the
        // number the page shows is how many things the agent did, not how many lines the file has.
        assert_eq!(turns[0].tools, 2);
        // Two assistant lines: the one that asked for the calls, and the one that answered.
        assert_eq!(turns[0].rounds, 2);
        assert_eq!(turns[0].at, "2026-11-01T01:00:01Z");
    }

    const ONE_PICTURE: &str = r#"{"type":"message","role":"user","images":["data:image/png;base64,iVBORw0KGgo="],"at":"2026-11-01T03:00:00Z"}"#;
    const A_PICTURE_REPLY: &str = r#"{"type":"message","role":"assistant","content":"It is a white square.","at":"2026-11-01T03:00:05Z"}"#;

    #[test]
    fn a_picture_sent_on_its_own_is_a_turn_with_a_question() {
        // The message was a screenshot and nothing else, so the record has no text at all. Read
        // back without its pictures this was a turn with an empty question, and the page drew a
        // blank row with the answer hanging under it — the one case the dialog was built for.
        let scratch = Scratch::new("picture-only");
        scratch.write(&[ONE_PICTURE, A_PICTURE_REPLY]);

        let turns = read(&scratch.base);
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert_eq!(turns[0].user, "", "the file holds no words for it");
        assert_eq!(turns[0].pictures, 1, "the picture did not survive the read");
        assert_eq!(turns[0].reply, "It is a white square.");

        // And the route says so, which is what the page actually reads.
        let wire: serde_json::Value = serde_json::from_str(&body(&scratch.base)).unwrap();
        assert_eq!(wire["turns"][0]["pictures"], 1, "{wire}");
        assert_eq!(wire["turns"][0]["user"], "", "{wire}");
    }

    #[test]
    fn a_question_with_words_and_pictures_says_both() {
        let scratch = Scratch::new("words-and-pictures");
        scratch.write(&[
            r#"{"type":"message","role":"user","content":"what is wrong here?","images":["data:image/png;base64,AA==","data:image/png;base64,BB=="],"at":"2026-11-01T03:00:00Z"}"#,
            A_PICTURE_REPLY,
        ]);
        let turns = read(&scratch.base);
        assert_eq!(turns[0].user, "what is wrong here?");
        assert_eq!(
            turns[0].pictures, 2,
            "pictures are counted, not just noticed"
        );
    }

    #[test]
    fn a_conversation_written_before_pictures_existed_still_counts_zero() {
        // The other half of the compatibility claim: a line with no `images` key is a message with
        // no pictures, not an error and not a missing count.
        let scratch = Scratch::new("no-pictures");
        scratch.write(&[SYSTEM, ASK, TOOLS, RESULT, REPLY]);
        let turns = read(&scratch.base);
        assert_eq!(turns[0].pictures, 0, "{turns:?}");
        let wire: serde_json::Value = serde_json::from_str(&body(&scratch.base)).unwrap();
        assert_eq!(wire["turns"][0]["pictures"], 0, "{wire}");
    }

    #[test]
    fn the_picture_count_is_a_count_and_not_the_pictures() {
        // The route is polled every second while a turn runs, and one screenshot is about 1.4
        // million characters. A transcript that carried them would answer with megabytes a second
        // to draw a thumbnail, so what a turn carries is how many there were.
        let scratch = Scratch::new("pictures-are-not-carried");
        scratch.write(&[
            r#"{"type":"message","role":"user","content":"look","images":["data:image/png;base64,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"],"at":"2026-11-01T03:00:00Z"}"#,
            A_PICTURE_REPLY,
        ]);
        let wire = body(&scratch.base);
        assert!(wire.contains(r#""pictures":1"#), "{wire}");
        assert!(!wire.contains("data:image/"), "the picture itself: {wire}");
        assert!(
            wire.len() < 600,
            "a transcript of one picture should be small: {} bytes",
            wire.len()
        );
    }

    #[test]
    fn a_turn_is_readable_while_it_is_still_running() {
        // The state the page polls in: a question asked, a few rounds spent, no answer yet. Reading
        // it has to say how far the turn has got rather than failing on the missing reply, because
        // "still working" and "never started" are the two readings the page exists to separate.
        let scratch = Scratch::new("mid-turn");
        scratch.write(&[ASK, TOOLS, RESULT]);
        let turns = read(&scratch.base);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user, "build a todo list");
        assert_eq!(turns[0].reply, "");
        assert_eq!(turns[0].rounds, 1, "one round has happened: {turns:?}");
        assert_eq!(turns[0].tools, 2);
        // And it grows: the next read of the same turn sees the round that followed.
        scratch.write(&[ASK, TOOLS, RESULT, TOOLS, RESULT, REPLY]);
        let later = read(&scratch.base);
        // Two more rounds of tool calls and then the answer, on top of the one already there.
        assert_eq!(later[0].rounds, 3, "{later:?}");
        assert_eq!(later[0].tools, 4);
        assert_eq!(later[0].reply, "Done: three files.");
    }

    #[test]
    fn a_turn_that_never_wrote_an_answer_still_has_a_turn() {
        // The measured case: a turn that spends its whole budget on tools returns no prose, and
        // it has to be distinguishable on the page from a turn that never ran.
        let scratch = Scratch::new("no-answer");
        scratch.write(&[ASK, TOOLS, RESULT, NOTE]);
        let turns = read(&scratch.base);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].reply, "");
        assert_eq!(turns[0].tools, 2);
        assert_eq!(turns[0].notes.len(), 1);
        assert!(
            turns[0].notes[0].contains("round budget of 24"),
            "{:?}",
            turns[0].notes
        );
    }

    #[test]
    fn the_agent_s_instructions_are_not_part_of_the_conversation() {
        let scratch = Scratch::new("system");
        scratch.write(&[SYSTEM]);
        assert!(read(&scratch.base).is_empty());
    }

    #[test]
    fn two_questions_are_two_turns_in_the_order_they_were_asked() {
        let scratch = Scratch::new("order");
        let second = r#"{"type":"message","role":"user","content":"now add dates","at":"2026-11-01T02:00:00Z"}"#;
        let answer = r#"{"type":"message","role":"assistant","content":"Added.","at":"2026-11-01T02:00:01Z"}"#;
        scratch.write(&[ASK, REPLY, second, answer]);
        let turns = read(&scratch.base);
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0].user, "build a todo list");
        assert_eq!(turns[1].user, "now add dates");
        assert_eq!(turns[1].reply, "Added.");
    }

    #[test]
    fn a_torn_last_line_costs_one_record_and_not_the_conversation() {
        // The file is appended a line at a time while this reads it, so a partial newest line is
        // expected rather than exceptional.
        let scratch = Scratch::new("torn");
        scratch.write(&[ASK, REPLY, r#"{"type":"message","role":"assistant","cont"#]);
        let turns = read(&scratch.base);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].reply, "Done: three files.");
    }

    /// One assistant line that asked for these calls, spelled the way the agent writes it.
    fn asking(calls: &[(String, &str, &str)]) -> String {
        let asked: Vec<serde_json::Value> = calls
            .iter()
            .map(|(id, name, arguments)| {
                serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                })
            })
            .collect();
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "tool_calls": asked,
            "at": "2026-11-01T01:00:02Z",
        })
        .to_string()
    }

    /// One tool line answering `id`, with the ending when the file would have recorded one.
    fn answering(id: &str, content: &str, outcome: Option<&str>) -> String {
        let mut line = serde_json::json!({
            "type": "message",
            "role": "tool",
            "content": content,
            "tool_call_id": id,
            "at": "2026-11-01T01:00:03Z",
        });
        if let Some(outcome) = outcome {
            line["outcome"] = serde_json::json!(outcome);
        }
        line.to_string()
    }

    #[test]
    fn every_call_the_agent_made_is_a_row_of_its_own() {
        // What replaced a count. "ran 2 tools" was the one thing a person could not check: which
        // file was read, what the shell was told to run, and whether a call was denied rather than
        // attempted. All of it was already in the file, on two kinds of line.
        let scratch = Scratch::new("calls");
        let asked = asking(&[
            ("c1".to_string(), "bash", r#"{"command":"cargo build"}"#),
            (
                "c2".to_string(),
                "read",
                r#"{"line":12,"path":"src/main.rs"}"#,
            ),
        ]);
        let built = answering("c1", "Compiling deepnest", Some("ok"));
        let denied = answering("c2", "outside the writable set", Some("refused"));
        scratch.write(&[SYSTEM, ASK, &asked, &built, &denied, REPLY]);

        let turns = read(&scratch.base);
        assert_eq!(turns.len(), 1);
        let calls = &turns[0].calls;
        assert_eq!(calls.len(), 2, "one row per call: {calls:?}");
        assert_eq!(calls[0].name, "bash");
        // The identifying argument, not the whole argument object: a row is a line.
        assert_eq!(calls[0].what, "cargo build");
        assert_eq!(calls[0].state, "ok");
        assert_eq!(calls[0].output, "Compiling deepnest");
        // `path` and not the `line` that came first in the object, which is why the keys are
        // tried in an order rather than taking whatever string turns up first.
        assert_eq!(calls[1].name, "read");
        assert_eq!(calls[1].what, "src/main.rs");
        assert_eq!(calls[1].state, "refused");
        assert_eq!(calls[1].output, "outside the writable set");
        // The count is still there, because the progress line counts work in flight, and a row is
        // only drawn once there is something to draw.
        assert_eq!(turns[0].tools, 2);
    }

    #[test]
    fn a_call_the_model_has_just_asked_for_says_nothing_about_how_it_ended() {
        // What a running turn looks like between the ask and the answer: the row exists so the
        // page can say what is happening now, and it must not claim an ending it has not seen.
        let scratch = Scratch::new("pending-call");
        let asked = asking(&[("c1".to_string(), "bash", r#"{"command":"cargo test"}"#)]);
        scratch.write(&[SYSTEM, ASK, &asked]);

        let calls = &read(&scratch.base)[0].calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].what, "cargo test");
        assert!(calls[0].state.is_empty(), "{:?}", calls[0].state);
        assert!(calls[0].output.is_empty());
    }

    #[test]
    fn a_line_written_before_the_outcome_was_recorded_says_it_ended_without_saying_how() {
        // Every conversation on this machine predates the field. Reading "it ended" as "it
        // succeeded" would invent the one thing the field exists to state.
        let scratch = Scratch::new("old-file");
        let asked = asking(&[("c1".to_string(), "bash", r#"{"command":"ls"}"#)]);
        let result = answering("c1", "a b c", None);
        scratch.write(&[SYSTEM, ASK, &asked, &result]);

        let calls = &read(&scratch.base)[0].calls;
        assert_eq!(calls[0].state, "done");
        assert_eq!(calls[0].output, "a b c");
    }

    #[test]
    fn a_long_output_is_cut_for_the_page_and_says_where_it_was_cut() {
        let scratch = Scratch::new("long-output");
        let asked = asking(&[("c1".to_string(), "bash", r#"{"command":"cat big.log"}"#)]);
        let long = "x".repeat(CALL_OUTPUT_CHARS + 800);
        let result = answering("c1", &long, Some("ok"));
        scratch.write(&[SYSTEM, ASK, &asked, &result]);

        let output = &read(&scratch.base)[0].calls[0].output;
        assert!(
            output.contains(&format!(
                "[cut: showed {CALL_OUTPUT_CHARS} of {} characters]",
                CALL_OUTPUT_CHARS + 800
            )),
            "a cut that does not say it was cut reads as the whole output"
        );
        // A rate, not only a size: this route is polled once a second while a turn runs.
        assert!(output.chars().count() < CALL_OUTPUT_CHARS + 80);
    }

    #[test]
    fn a_turn_that_has_carried_its_share_stops_carrying_output_but_keeps_its_rows() {
        // A hundred calls in one turn would otherwise be a transcript answering with all of it
        // every second — the page slowing the turn it is watching. The row survives; the text
        // does not, and the row says so.
        let scratch = Scratch::new("many-calls");
        let mut lines: Vec<String> = vec![SYSTEM.to_string(), ASK.to_string()];
        let asked: Vec<(String, &str, &str)> = (0..20)
            .map(|n| (format!("c{n}"), "bash", r#"{"command":"cat build.log"}"#))
            .collect();
        lines.push(asking(&asked));
        for (id, _, _) in &asked {
            lines.push(answering(id, &"y".repeat(1_000), Some("ok")));
        }
        let refs: Vec<&str> = lines.iter().map(|line| line.as_str()).collect();
        scratch.write(&refs);

        let calls = &read(&scratch.base)[0].calls;
        assert_eq!(calls.len(), 20);
        assert!(!calls[0].output.contains(CUT_FROM_THE_TURN));
        assert_eq!(calls[19].output, CUT_FROM_THE_TURN);
        // Still a row with everything a row is for.
        assert_eq!(calls[19].what, "cat build.log");
        assert_eq!(calls[19].state, "ok");
    }

    #[test]
    fn arguments_that_do_not_parse_are_shown_as_they_arrived() {
        // The model sometimes sends something that is not an object; a row that said nothing
        // about such a call would hide the call.
        assert_eq!(what_of("not json at all"), "not json at all");
        assert_eq!(what_of(r#"{"command":"npm test"}"#), "npm test");
        assert_eq!(what_of(r#"{"count":3}"#), r#"{"count":3}"#);
        let long = what_of(&format!(r#"{{"command":"{}"}}"#, "z".repeat(200)));
        assert!(long.chars().count() <= WHAT_CHARS + 1, "{long}");
    }
}
