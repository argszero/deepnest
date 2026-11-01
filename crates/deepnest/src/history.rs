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
    /// How many times the model was asked in this turn.
    ///
    /// One more than a tool call count is not the same thing, and it is the number that says a
    /// turn is working rather than stuck: a round with no calls is the model thinking, and a turn
    /// that has stopped growing is the one worth telling the user about.
    pub rounds: usize,
    /// The agent's own notes, in order: why a turn ended early, what it could not run.
    pub notes: Vec<String>,
    /// When the user's message was written, UTC to the second, as the file wrote it.
    pub at: String,
}

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
    turns
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
            user: record.content.clone().unwrap_or_default(),
            pictures: record.images.as_ref().map_or(0, Vec::len),
            reply: String::new(),
            tools: 0,
            rounds: 0,
            notes: Vec::new(),
            at: record.at.clone(),
        }),
        "assistant" => {
            let Some(turn) = turns.last_mut() else {
                return;
            };
            turn.rounds += 1;
            turn.tools += record.tool_calls.as_ref().map_or(0, Vec::len);
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
        // A tool's result is not counted again: the call was already counted on the assistant
        // line that asked for it, and counting both would report twice the work.
        "tool" => {}
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
}
