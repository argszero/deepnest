//! The conversations an app has had, and which one the page is looking at.
//!
//! An app had exactly one conversation until now: `.agent/history.jsonl`, written by whichever turn
//! happened to run. That is one conversation too few — the second thing a person asks about an app
//! is rarely a continuation of the first, and there was no way to start a clean one without
//! deleting the transcript that recorded what the agent had already done.
//!
//! Three decisions, and the first is the one that keeps this from being a migration:
//!
//! - **A session is a file, and the file every app already has keeps its own name.** Session
//!   [`MAIN`] *is* `.agent/history.jsonl`, at the path it has always been at, so nothing is moved
//!   and nothing can be lost by moving it. A conversation the agent has been writing for a day is
//!   not a thing to relocate for the sake of a tidier layout; a named session is the new case and
//!   lives in the new place, `.agent/sessions/<id>/history.jsonl`.
//! - **Which one is current is a file this side owns, not a variable in the page.** Every turn that
//!   changes the app reloads the page, so a drawer that remembered the open conversation in memory
//!   would reopen on the wrong one at exactly the moment the person wanted to read the one they had
//!   just finished. The page asks; the server answers from `.agent/sessions/current`.
//! - **A pointer this side cannot read is [`MAIN`], not an error.** Same reasoning as the history
//!   route's empty conversation: a page that cannot say which conversation it is on is worse than
//!   one that says "the one you have always had". It is also what makes a hand-edited or truncated
//!   pointer harmless — a name that is not a session name is ignored rather than joined onto a path.
//!
//! A session's *name* is its path, so the grammar is deliberately narrow: [`MAIN`], or `s-` and a
//! UTC timestamp. Anything else is not a session, and a pointer holding `../` resolves to nothing
//! rather than out of the app directory.

use std::path::{Path, PathBuf};

use deepnest_agent::AgentPaths;

/// The route this module answers — the same spelling the router routes on.
pub const ACTION: &str = "sessions";

/// The conversation every app already has, at the path it already has.
pub const MAIN: &str = "main";

/// Where the chosen session's name is written, relative to the app directory.
const CURRENT: &str = ".agent/sessions/current";

/// The longest title the drawer shows, in characters.
///
/// A title is the first question, and a question can be a pasted stack trace. The row is one line
/// in a 22rem panel, so what does not fit is dropped here rather than wrapped by the browser.
const TITLE_CHARS: usize = 90;

/// One conversation, as the drawer needs it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Entry {
    /// The name to send back to open it.
    pub id: String,
    /// The first thing the person asked, cut to one row. Empty for a session with no turns.
    pub title: String,
    /// How many exchanges it holds — the number that tells an empty session from a used one.
    pub turns: usize,
    /// When its newest exchange was written, UTC to the second, as the file wrote it.
    pub at: String,
    /// Whether this is the one the page is on.
    pub current: bool,
}

/// The name of the session the page is on.
///
/// Never a failure: a missing pointer, an unreadable one, and one naming something that is not a
/// session name all answer [`MAIN`].
pub fn current(app_dir: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(app_dir.join(CURRENT)) else {
        return MAIN.to_string();
    };
    let name = text.lines().next().unwrap_or("").trim();
    if is_a_name(name) {
        name.to_string()
    } else {
        MAIN.to_string()
    }
}

/// The file a session's records are appended to.
///
/// [`MAIN`] is the legacy path, deliberately: see this module's note on not moving a conversation.
pub fn history_path_for(app_dir: &Path, id: &str) -> PathBuf {
    if id == MAIN {
        return AgentPaths::for_app(app_dir).history();
    }
    sessions_dir(app_dir).join(id).join("history.jsonl")
}

/// Where the named sessions live: `<app>/.agent/sessions`.
fn sessions_dir(app_dir: &Path) -> PathBuf {
    AgentPaths::for_app(app_dir).root().join("sessions")
}

/// Whether this app has that session.
///
/// [`MAIN`] always exists, file or no file: it is the conversation every app has by definition, and
/// its file appears the first time somebody asks something. The others are asked for by directory
/// and not by file, because a session that has just been created has a directory and no records yet
/// — a check against `history.jsonl` would decide that the session a page was just told it had
/// started does not exist, and refuse to open the drawer on it.
fn has(app_dir: &Path, id: &str) -> bool {
    if id == MAIN {
        return true;
    }
    sessions_dir(app_dir).join(id).is_dir() || history_path_for(app_dir, id).is_file()
}

/// The file the current session's records are appended to.
///
/// This is what a turn writes to and what the transcript is read from, so the two cannot disagree
/// about which conversation is on screen: both ask this.
pub fn history_path(app_dir: &Path) -> PathBuf {
    history_path_for(app_dir, &current(app_dir))
}

/// Make `id` the current session, if it is one this app has.
///
/// `Ok` with the name it actually opened, because the answer is what the drawer renders: a refusal
/// that leaves the page thinking it switched would be worse than no switching at all.
pub fn open(app_dir: &Path, id: &str) -> Result<String, String> {
    if !is_a_name(id) {
        return Err(format!(
            "`{id}` is not a session name — the names are `{MAIN}` and `s-<timestamp>`",
        ));
    }
    // An existing name must exist: *opening* a session is not a way to create one, or a typo
    // answers with a brand new empty conversation and reads as if the old one were lost.
    if !has(app_dir, id) {
        return Err(format!("there is no session called `{id}` in this app"));
    }
    write_current(app_dir, id)?;
    Ok(id.to_string())
}

/// Start a session and make it current, answering its name.
///
/// The directory is created here rather than left to the first turn, because a session that exists
/// only once somebody has typed into it is a drawer that answers "new session" with nothing.
pub fn create(app_dir: &Path) -> Result<String, String> {
    let id = next_name(app_dir);
    let path = history_path_for(app_dir, &id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("the session directory could not be created: {error}"))?;
    }
    write_current(app_dir, &id)?;
    Ok(id)
}

/// Every session this app has, the most recently used first.
///
/// [`MAIN`] is always one of them, empty or not. The alternative — listing it only once it holds
/// something — reads as the drawer losing the conversation the person was just in the moment they
/// left it, and an empty conversation is still a place they can go and ask something.
pub fn list(app_dir: &Path) -> Vec<Entry> {
    let now = current(app_dir);
    let mut ids: Vec<String> = vec![MAIN.to_string()];
    let dir = sessions_dir(app_dir);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if is_a_name(&name) && name != MAIN && has(app_dir, &name) {
                ids.push(name);
            }
        }
    }
    if !ids.iter().any(|id| id == &now) {
        ids.push(now.clone());
    }

    let mut listed: Vec<Entry> = ids
        .into_iter()
        .map(|id| {
            let turns = crate::history::read_at(&history_path_for(app_dir, &id));
            let title = turns
                .first()
                .map(|turn| cut(&turn.user, TITLE_CHARS))
                .unwrap_or_default();
            // The newest exchange's own clock rather than the file's mtime: the transcript prints
            // these times, and a row dated by a second clock is a row that can disagree with the
            // conversation it opens.
            let at = turns.last().map(|turn| turn.at.clone()).unwrap_or_default();
            Entry {
                id: id.clone(),
                title,
                turns: turns.len(),
                at,
                current: id == now,
            }
        })
        .collect();
    // Newest first, and a session with no turn at all after the ones that have one: the order is
    // what makes "the conversation I was in the middle of" the first row.
    listed.sort_by(|left, right| right.at.cmp(&left.at).then_with(|| right.id.cmp(&left.id)));
    listed
}

/// The body of the `sessions` route: the list, and which one is current.
pub fn body(app_dir: &Path) -> String {
    let sessions = list(app_dir);
    serde_json::json!({ "current": current(app_dir), "sessions": sessions }).to_string()
}

/// Write the pointer.
fn write_current(app_dir: &Path, id: &str) -> Result<(), String> {
    let path = app_dir.join(CURRENT);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("the session directory could not be created: {error}"))?;
    }
    std::fs::write(&path, format!("{id}\n"))
        .map_err(|error| format!("the current session could not be written: {error}"))
}

/// A name no session is using yet.
///
/// The timestamp is the visible part and the serial is the tie-break: two sessions started in the
/// same second are two sessions, and the second is not made to wait for the first to end.
fn next_name(app_dir: &Path) -> String {
    let stamp: String = deepnest_agent::session::now_iso()
        .chars()
        .filter(|character| character.is_ascii_digit())
        .take(14)
        .collect();
    let base = format!("s-{stamp}");
    if !has(app_dir, &base) {
        return base;
    }
    for serial in 2..1000 {
        let candidate = format!("{base}-{serial}");
        if !has(app_dir, &candidate) {
            return candidate;
        }
    }
    // A thousand sessions in one second is not a case worth a smarter allocator; the caller gets a
    // name that will exist rather than a panic, and the second session of that second reuses it.
    base
}

/// Whether `name` is a session name.
///
/// The grammar is the path: lowercase letters, digits and `-`, starting with a letter, at most 40
/// characters. That excludes `.`, `/` and everything a filesystem would interpret, so a pointer
/// read from disk cannot address anything outside the app's own `.agent/sessions` directory.
fn is_a_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 40 {
        return false;
    }
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    name.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    })
}

/// `text` cut to `chars` characters, marked where it was cut.
fn cut(text: &str, chars: usize) -> String {
    let mut kept = String::new();
    for character in text.chars().take(chars) {
        // A question is typed with line breaks in it; a row is one line.
        kept.push(if character == '\n' || character == '\r' {
            ' '
        } else {
            character
        });
    }
    if text.chars().count() > chars {
        kept.push('…');
    }
    kept.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        /// An app with a conversation under the legacy name, which is what every existing app has.
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-sessions-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(base.join(".agent")).unwrap();
            Self { base }
        }

        fn ask(&self, path: &Path, words: &str, at: &str) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let line = serde_json::json!({
                "type": "message",
                "role": "user",
                "content": words,
                "at": at,
            })
            .to_string();
            let mut text = fs::read_to_string(path).unwrap_or_default();
            text.push_str(&line);
            text.push('\n');
            fs::write(path, text).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn an_app_nobody_has_spoken_to_is_on_the_conversation_it_has_always_had() {
        // The pointer is missing, and that is the normal state of a fresh app rather than a fault:
        // the answer is the legacy conversation, and it is where the first turn will write.
        let scratch = Scratch::new("fresh");
        assert_eq!(current(&scratch.base), MAIN);
        assert_eq!(
            history_path(&scratch.base),
            scratch.base.join(".agent/history.jsonl"),
            "the legacy path is not somewhere a conversation is moved out of"
        );
    }

    #[test]
    fn a_new_session_gets_its_own_file_and_becomes_the_one_the_page_is_on() {
        let scratch = Scratch::new("new");
        let id = create(&scratch.base).unwrap();
        assert!(is_a_name(&id), "{id}");
        assert_eq!(current(&scratch.base), id);
        assert_eq!(
            history_path(&scratch.base),
            scratch
                .base
                .join(".agent/sessions")
                .join(&id)
                .join("history.jsonl")
        );
        assert!(
            history_path(&scratch.base).parent().unwrap().is_dir(),
            "the directory is created with the session, so the drawer can show it straight away"
        );
    }

    #[test]
    fn a_second_session_in_the_same_second_is_a_second_session() {
        // The timestamp is the visible part of the name and the serial is the tie-break; without
        // one, two sessions started together would be one session with two people's questions.
        let scratch = Scratch::new("two");
        let first = create(&scratch.base).unwrap();
        let second = create(&scratch.base).unwrap();
        assert_ne!(first, second);
        assert_eq!(current(&scratch.base), second);
    }

    #[test]
    fn opening_a_session_this_app_does_not_have_is_refused_rather_than_created() {
        // A created-on-open would answer a typo with an empty conversation, which reads as the
        // conversation that was there a second ago having been lost.
        let scratch = Scratch::new("typo");
        assert!(open(&scratch.base, "s-20260101-000000").is_err());
        assert_eq!(current(&scratch.base), MAIN);
        // And a name that is not a session name is refused before it can become a path.
        for name in ["../secrets", "/etc/passwd", "Main", "s-1/../2", ""] {
            assert!(open(&scratch.base, name).is_err(), "{name} was accepted");
        }
    }

    #[test]
    fn a_pointer_that_is_not_a_session_name_is_ignored() {
        // A hand-edited or truncated pointer must not become a path, and it must not become an
        // error either: the page carries on in the conversation it has always had.
        let scratch = Scratch::new("pointer");
        fs::create_dir_all(scratch.base.join(".agent/sessions")).unwrap();
        fs::write(scratch.base.join(CURRENT), "../../etc/passwd\n").unwrap();
        assert_eq!(current(&scratch.base), MAIN);
        assert_eq!(
            history_path(&scratch.base),
            scratch.base.join(".agent/history.jsonl")
        );
    }

    #[test]
    fn the_drawer_lists_the_newest_conversation_first_and_says_which_one_is_open() {
        let scratch = Scratch::new("list");
        scratch.ask(
            &scratch.base.join(".agent/history.jsonl"),
            "build a todo list",
            "2026-11-01T01:00:00Z",
        );
        let id = create(&scratch.base).unwrap();
        scratch.ask(
            &history_path_for(&scratch.base, &id),
            "why is the total wrong",
            "2026-11-01T02:00:00Z",
        );

        let listed = list(&scratch.base);
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0].id, id, "the newest is the first row");
        assert_eq!(listed[0].title, "why is the total wrong");
        assert_eq!(listed[0].turns, 1);
        assert!(listed[0].current, "and it is the one the page is on");
        assert_eq!(listed[1].id, MAIN);
        assert_eq!(listed[1].title, "build a todo list");
        assert!(!listed[1].current);
    }

    #[test]
    fn a_session_nobody_has_typed_into_is_still_in_the_list() {
        // The state "New session" leaves behind, and the one a list built only from files that
        // hold something would answer with nothing — an empty drawer that just created a session.
        let scratch = Scratch::new("blank");
        let id = create(&scratch.base).unwrap();
        let listed = list(&scratch.base);
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].turns, 0);
        assert_eq!(listed[0].title, "");
        assert!(listed[0].current);
        // And the conversation the app has always had is still there to go back to.
        assert_eq!(listed[1].id, MAIN);
        assert_eq!(listed[1].turns, 0);
    }

    #[test]
    fn a_title_is_one_row_however_the_question_was_typed() {
        // A pasted stack trace is a question too, and a row is a line: what does not fit is cut,
        // and the line breaks in it do not become rows.
        let scratch = Scratch::new("title");
        let long = format!("the build fails\n\n{}", "x".repeat(200));
        scratch.ask(
            &scratch.base.join(".agent/history.jsonl"),
            &long,
            "2026-11-01T01:00:00Z",
        );
        let title = list(&scratch.base)[0].title.clone();
        assert!(!title.contains('\n'), "{title}");
        assert!(title.chars().count() <= TITLE_CHARS + 1, "{title}");
        assert!(title.ends_with('…'), "{title}");
    }

    #[test]
    fn the_route_answers_with_the_list_and_the_one_that_is_open() {
        let scratch = Scratch::new("body");
        scratch.ask(
            &scratch.base.join(".agent/history.jsonl"),
            "build a todo list",
            "2026-11-01T01:00:00Z",
        );
        let body = body(&scratch.base);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["current"], MAIN);
        assert_eq!(value["sessions"][0]["id"], MAIN);
        assert_eq!(value["sessions"][0]["title"], "build a todo list");
    }

    #[test]
    fn a_name_is_its_path_so_the_grammar_is_the_guard() {
        for good in ["main", "s-20261101-171500", "s-20261101-171500-2", "a"] {
            assert!(is_a_name(good), "{good}");
        }
        for bad in [
            "",
            "Main",
            "s_1",
            "../x",
            "a/b",
            ".hidden",
            "a b",
            &"a".repeat(41),
        ] {
            assert!(!is_a_name(bad), "{bad}");
        }
    }
}
