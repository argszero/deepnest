//! One message in, one turn out: the agent behind the app's dialog.
//!
//! Everything shipped before this file could be exercised without a program — a loop, a sandbox, a
//! skill loader — and the dialog still could not answer a word, because nothing joined them. This
//! is that join, and it is deliberately the one place that knows all of them at once:
//!
//! - **One agent per app, and its state lives in the app's own directory** ([`AgentPaths`]), so
//!   deleting an app deletes its conversation and backing an app up backs the conversation up.
//! - **Every tool call goes through the fence.** The tools are `crates/tools`' six leaves plus the
//!   two seams this crate is the only one that can wire — `subagent`, whose child loop belongs to
//!   `crates/agent`, and `install_skill`, whose catalog and publisher are [`crate::install`]'s —
//!   and the policy is `crates/sandbox`'s for this app: the app directory, the toolchain and temp —
//!   the three roots `writable_roots` already derives, so the file tools and the shell agree because
//!   they are reading the same answer.
//! - **The prompt names the tools the turn actually has.** Its tool section is rendered from the
//!   set handed to the loop ([`registry::names`]), so it cannot advertise a tool this turn cannot
//!   dispatch, or leave out one it can. That set is built before the prompt for exactly this
//!   reason.
//! - **The skills are the real loader's.** The prompt's skill index comes from
//!   `deepnest_skills::load`, so a skill the user installed is in front of the agent rather than
//!   in a list this file maintained separately.
//! - **The model is passed in, not built here.** [`answer`] takes any [`Model`], which is what lets
//!   a test run a whole turn — tools included — with no socket and no key. The binary passes
//!   [`crate::model::Endpoint`].
//!
//! What this does **not** do yet, stated where it is implemented so a later reader does not have to
//! infer it from an absence:
//!
//! - **The answer can arrive whole or a piece at a time.** The transport streams and delivers each
//!   event as it arrives. [`answer`] still returns one [`Reply`] and `POST …/chat` still answers
//!   one JSON body, because a caller that only wants the result loses nothing by waiting for it;
//!   [`watched`] is the same turn with an observer, and the route above it uses that when the
//!   request asks for a stream. The turn is the same turn either way — what differs is whether
//!   anything is watching it being written.
//! - **A writing turn starts a rebuild, and does not wait for it.** The reply says `changed` and
//!   the router enqueues the build ([`crate::rebuild`]); this function returns at once, because a
//!   build inside the request would hold one HTTP connection for a minute of `bun install`, which
//!   is a longer request than any browser waits for. What the person watches instead is the status
//!   route the page polls, and what this turn is *told* is the previous build's failure —
//!   [`build_note`] — so a turn after a broken build fixes the source instead of stacking code on
//!   top of a tree that does not compile.
//! - **An escalation request waits for the page that asked the question.** A call may carry
//!   `request_mode` and `justification`; `crates/agent` puts that in front of whoever is attached as
//!   its [`Approver`], and this module hands it [`crate::escalation::Requests::approver`]. So the
//!   turn blocks for as long as a person takes to answer — up to
//!   [`crate::escalation::ANSWER_TIMEOUT`] — and the widened policy belongs to that one call. The
//!   session's own mode is the one the prompt states, before and after, whatever was approved.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use deepnest_agent::{
    prompt::system_prompt, Agent, AgentError, AgentPaths, Approver, Cancellation, LoopRunner,
    MemoryStore, Model, PromptContext, Session, TurnOutcome,
};
use deepnest_apps::{manifest, Slug};
use deepnest_llm::ToolDefinition;
use deepnest_sandbox::{roots::writable_roots, Mode, Policy};
use deepnest_tools::{registry, StdSpawner, Tool, ToolContext, ToolResult};
use serde_json::{json, Value};

/// The verb this module answers, under `POST /api/apps/<slug>/`.
pub const ACTION: &str = "chat";

/// How one message ended, in the shape the route returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

impl Reply {
    fn error(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({ "error": message.into() }),
        }
    }
}

/// The tools whose calls can change the app.
///
/// Named as a list of *changing* tools rather than "everything except the readers" because a tool
/// added tomorrow should default to not marking the app as changed: a page that reloads for nothing
/// is a page that reloads in the middle of the user reading it.
const WRITING_TOOLS: [&str; 3] = ["bash", "write", "edit"];

/// The most pictures one message may carry.
///
/// Not a guess about the model: it is what the dialog can produce before the person has clearly
/// stopped sending a message and started uploading an album. Every one of them travels in the
/// request body and then into the conversation file.
pub const MAX_IMAGES: usize = 8;

/// The largest the pictures of one message may be together, in characters of `data:` URL.
///
/// A `data:` URL is a third larger than the bytes it carries, so this is a screenshot-sized budget
/// with room for several of them, and a ceiling on what one request can make this process hold.
pub const MAX_IMAGE_CHARS: usize = 8_000_000;

/// One message as the dialog sends it: the words, and any pictures pasted with them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ask {
    pub text: String,
    /// `data:` URLs, in the order they were pasted.
    pub images: Vec<String>,
}

/// Read `{"message": "...", "images": ["data:image/png;base64,..."]}` out of a request body.
///
/// The body and not the query for the same reason `POST /api/config` takes one: what the user types
/// is their own text, and a URL is written into the browser's history and into whatever logs it.
/// The pictures are here for the same reason, more so: a screenshot's base64 is far too long for a
/// query string, and a query string is logged.
///
/// `images` is optional and its absence is the ordinary case — a typed message must parse exactly
/// as it did before pictures existed.
pub fn ask_from(body: &[u8]) -> Result<Ask, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the body must be UTF-8 JSON")?;
    if text.trim().is_empty() {
        return Err("the body must be `{\"message\": \"...\"}` — it was empty".to_string());
    }
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("the body is not JSON: {error}"))?;
    let images = images_from(&value)?;
    let message = value.get("message").and_then(Value::as_str);
    let words = message.map(str::trim).unwrap_or_default();
    if words.is_empty() && images.is_empty() {
        return Err(if message.is_none() {
            "the body must carry a string field named `message`".to_string()
        } else {
            "`message` is empty — there is nothing to answer".to_string()
        });
    }
    Ok(Ask {
        text: message.unwrap_or_default().to_string(),
        images,
    })
}

/// Read the `images` field, if the body has one.
///
/// **A picture must be a `data:` URL and nothing else.** An `https://` URL here would be a request
/// for the model provider to fetch that address on our behalf, which turns a pasted screenshot into
/// a beacon that tells a third party what this machine is looking at — and it would make the
/// conversation file a record of remote links rather than of the pictures themselves, so the turn
/// replayed tomorrow would show whatever that URL serves then.
fn images_from(value: &Value) -> Result<Vec<String>, String> {
    let Some(field) = value.get("images") else {
        return Ok(Vec::new());
    };
    if field.is_null() {
        return Ok(Vec::new());
    }
    let array = field
        .as_array()
        .ok_or_else(|| "`images` must be a list of `data:image/...` URLs".to_string())?;
    if array.len() > MAX_IMAGES {
        return Err(format!(
            "one message carries at most {MAX_IMAGES} pictures; this one has {}",
            array.len()
        ));
    }
    let mut images = Vec::with_capacity(array.len());
    let mut total = 0usize;
    for (index, entry) in array.iter().enumerate() {
        let url = entry
            .as_str()
            .ok_or_else(|| format!("`images[{index}]` must be a string"))?;
        if !url.starts_with("data:image/") {
            return Err(format!(
                "`images[{index}]` must be a `data:image/...` URL — anything else would have the \
                 model provider fetch an address of its own, so a pasted picture is sent as bytes \
                 or not at all"
            ));
        }
        total += url.chars().count();
        images.push(url.to_string());
    }
    if total > MAX_IMAGE_CHARS {
        return Err(format!(
            "the pictures in one message come to at most {MAX_IMAGE_CHARS} characters; this one \
             has {total}"
        ));
    }
    Ok(images)
}

/// Read `{"message": "..."}` out of a request body, words only.
pub fn message_from(body: &[u8]) -> Result<String, String> {
    ask_from(body).map(|ask| ask.text)
}

/// The conditions one turn runs under, as the router decides them.
///
/// Grouped for the same reason [`Ask`] is: they arrive together, they are meaningless apart, and a
/// call that took the approver of one turn and the cancellation of another would compile. It also
/// keeps [`answer`] at six parameters, which is where the author of this file already chose to
/// stop after `Ask` was introduced for exactly that reason.
#[derive(Default)]
pub struct Terms {
    /// Who a call's widening request is put in front of.
    ///
    /// `None` is a real answer and not a degraded one: an agent with nobody to ask refuses to widen
    /// and says so, which is what a test and any caller without a page behind it should get.
    pub approver: Option<Arc<dyn Approver>>,
    /// Why the last rebuild failed, when it did: the agent's next turn is told before it writes.
    pub build_note: Option<String>,
    /// What another request cancels this turn through.
    ///
    /// The default — a flag nobody holds — is a turn nobody can stop, which is what a caller with no
    /// page behind it is asking for. The router passes one it registered, so the dialog's stop
    /// button reaches the loop from a different connection.
    pub cancel: Cancellation,
}

/// Run one message through the app's agent.
///
/// `home` is the deepnest home (`~/.deepnest`) and `app_dir` the app's own directory; `slug` names
/// it in the prompt and in the policy. The app is checked for a manifest first: an agent pointed at
/// a directory that is not an app would edit whatever is there, and "no such app" is a better
/// answer than a turn spent on the wrong directory.
///
/// `terms` is the rest of what the caller decides about this turn — see [`Terms`].
pub fn answer(
    model: Arc<dyn Model>,
    home: &Path,
    app_dir: &Path,
    slug: &Slug,
    // The message and whatever was pasted with it. One value rather than two parameters: they
    // arrive together and are invalid apart, and a caller that passed words from one message and
    // pictures from another would be a bug no type ever caught.
    ask: &Ask,
    terms: &Terms,
) -> Reply {
    self::watched(model, home, app_dir, slug, ask, terms, &mut |_| {})
}

/// The same turn, with the model's text handed to `on_text` as it is written.
///
/// What the observer is for is the route above this: a caller that draws the answer while it is
/// being written passes one, and a caller that only wants the result passes nothing and gets the
/// behaviour it always had. Everything else — the refusals before the turn starts, the conversation
/// written to disk, the reply's shape — is shared, because a watched turn and an unwatched one are
/// the same turn and must not be able to drift.
pub fn watched(
    model: Arc<dyn Model>,
    home: &Path,
    app_dir: &Path,
    slug: &Slug,
    ask: &Ask,
    terms: &Terms,
    on_text: &mut dyn FnMut(&str),
) -> Reply {
    if let Err(error) = manifest::load(app_dir) {
        return Reply::error(
            404,
            format!("`{slug}` is not an app in this deepnest home: {error}"),
        );
    }

    let paths = AgentPaths::for_app(app_dir);
    if let Err(error) = paths.ensure() {
        return Reply::error(
            500,
            format!("the agent's directory could not be created: {error}"),
        );
    }
    let session = match Session::open(paths.history()) {
        Ok(session) => session,
        Err(error) => {
            return Reply::error(
                500,
                format!("the conversation could not be opened: {error}"),
            )
        }
    };

    // `workspace-write` and not a mode from a config file yet: there is one mode an app's agent may
    // run in, and it is the one that keeps the writes inside this app. This value is the *session*
    // default, and the only thing that ever widens is a per-call grant built from it — never a
    // write back to it.
    let policy = Policy::for_app(Mode::WorkspaceWrite, home, slug.as_str(), None);
    let writable: Vec<String> = writable_roots(&policy)
        .iter()
        .map(|root| root.path.display().to_string())
        .collect();

    let memory = MemoryStore::open(paths.memory_dir())
        .ok()
        .and_then(|store| store.index_for_prompt().ok())
        .unwrap_or_default();
    let skills: Vec<(String, String, String)> = deepnest_skills::load(home, Some(app_dir))
        .skills
        .iter()
        .map(|skill| {
            (
                skill.name.clone(),
                skill.description.clone(),
                skill.path.display().to_string(),
            )
        })
        .collect();

    // Everything the prompt claims is gathered before the prompt is rendered, because its tool
    // section is rendered from the set built below. That order is the point: a prompt assembled
    // first is a prompt that can name a tool this turn does not have.
    let context = PromptContext::new(
        slug.as_str(),
        app_dir.display().to_string(),
        Mode::WorkspaceWrite.as_str(),
    )
    .with_writable(writable)
    .with_memory_index(memory)
    .with_skills(skills)
    .with_app_url(format!("/app/{slug}/"));

    // One flag shared with every tool: the loop owns the turn, so what a tool touched is recorded
    // where the tools run and read where the answer is assembled.
    let changed = Arc::new(AtomicBool::new(false));
    // The install an agent may run, wired here because this is where the home is. The publisher is
    // the deadline-bearing one: a turn cannot outlive its own request waiting on somebody else's
    // program, which is the debt `crates/skills` says whoever wires the real publisher owes.
    let installer = Arc::new(crate::install::DialogInstaller::new(
        home.to_path_buf(),
        Arc::new(crate::install::SpawnerPublisher::new(
            Arc::new(StdSpawner),
            app_dir.to_path_buf(),
        )),
    ));
    // The child loop `subagent` delegates to: the same model, the app's own context, and the app's
    // own `AgentPaths`, so each call keeps its conversation under
    // `<app>/.agent/subagents/<NNNN>/history.jsonl` and never appends to the parent's file. The
    // child gets the six leaves and neither seam, so a subagent cannot start a subagent, and it has
    // no approver, so it cannot widen the policy its parent is fenced by.
    let runner = Arc::new(LoopRunner::new(Arc::clone(&model), paths, context.clone()));
    let tools: Vec<Box<dyn Tool>> = registry::with_seams(Some(runner), Some(installer))
        .into_iter()
        .map(|tool| {
            Box::new(Recorder {
                changed: Arc::clone(&changed),
                inner: tool,
            }) as Box<dyn Tool>
        })
        .collect();

    // The tool section names this exact vector — the one the agent below dispatches with — so the
    // prompt and the request's schema cannot come apart.
    let system = system_prompt(&context.with_tools(registry::names(&tools)));
    // Appended rather than folded into the prompt template: what a build said is this app's own
    // state, not a rule of the agent's, and the template is `crates/agent`'s to own.
    let system = match &terms.build_note {
        Some(note) => format!("{system}\n\n{note}"),
        None => system,
    };

    let mut agent = Agent::new(
        model,
        tools,
        ToolContext::new(policy, app_dir),
        session,
        system,
    );
    if let Some(approver) = &terms.approver {
        agent = agent.with_approver(Arc::clone(approver));
    }

    // The flag the router registered for this turn: a stop pressed in the dialog reaches the loop
    // from another connection, and the loop checks it before the next request and before every
    // tool call of a batch.
    let outcome = match agent.run_turn_watched(&ask.text, &ask.images, &terms.cancel, on_text) {
        Ok(outcome) => outcome,
        // A turn that could not run at all is not a turn that failed: the difference is what the
        // user can do about it, so the status carries it.
        Err(error) => return Reply::error(status_for(&error), error.to_string()),
    };

    let rounds = match &outcome {
        TurnOutcome::Answered { rounds, .. } | TurnOutcome::RoundLimit { rounds, .. } => *rounds,
        TurnOutcome::Cancelled { .. } => 0,
    };
    Reply {
        status: 200,
        body: json!({
            "text": outcome.text(),
            "rounds": rounds,
            "cancelled": matches!(outcome, TurnOutcome::Cancelled { .. }),
            "changed": changed.load(Ordering::SeqCst),
        }),
    }
}

/// A provider's refusal is an answer about the content, not this server's failure: the user's next
/// move is to change what they asked, and a `5xx` would suggest retrying the same text would work.
fn status_for(error: &AgentError) -> u16 {
    match error {
        AgentError::Refused { .. } => 422,
        AgentError::Model { .. } => 502,
        AgentError::Session { .. } => 500,
    }
}

/// A tool that notes when a call could have changed the app.
///
/// A wrapper rather than a flag inside each tool: the leaves belong to `crates/tools`, and a
/// counter threaded through six of them would be six places to forget. It records *before* the
/// call, because a tool that wrote and then failed still touched the directory.
struct Recorder {
    inner: Box<dyn Tool>,
    changed: Arc<AtomicBool>,
}

impl Tool for Recorder {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        if WRITING_TOOLS.contains(&self.name()) {
            self.changed.store(true, Ordering::SeqCst);
        }
        self.inner.call(args, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_agent::Model;
    use deepnest_llm::{AssistantTurn, FinishReason, LlmError, Message, ToolCall};
    use std::path::PathBuf;

    /// One app in a throwaway home, made by the real template so the manifest is the real one.
    struct Scratch {
        home: PathBuf,
        slug: String,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let home =
                std::env::temp_dir().join(format!("deepnest-chat-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(home.join("apps")).unwrap();
            let slug = "notes".to_string();
            let dir = home.join("apps").join(&slug);
            let parsed = Slug::parse(&slug).unwrap();
            deepnest_apps::create(&parsed, &dir, "Notes").unwrap();
            Self { home, slug }
        }

        fn app_dir(&self) -> PathBuf {
            self.home.join("apps").join(&self.slug)
        }

        fn slug(&self) -> Slug {
            Slug::parse(&self.slug).unwrap()
        }

        /// A slug for a directory the app template did not make.
        fn slug_of(text: &str) -> Slug {
            Slug::parse(text).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    /// A model that answers from a script, so a whole turn runs with no socket.
    struct Scripted {
        replies: std::sync::Mutex<Vec<Result<AssistantTurn, LlmError>>>,
        seen: std::sync::Mutex<Vec<Vec<Message>>>,
        /// What each request was told it could call — the other half of the prompt's tool section.
        offered: std::sync::Mutex<Vec<Vec<ToolDefinition>>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<AssistantTurn, LlmError>>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies),
                seen: std::sync::Mutex::new(Vec::new()),
                offered: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Model for Scripted {
        fn turn(
            &self,
            messages: &[Message],
            tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.offered.lock().unwrap().push(tools.to_vec());
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                panic!("the model was asked more times than the script has answers");
            }
            replies.remove(0)
        }
    }

    /// The tool names the prompt advertises, read back out of the section it prints them in.
    fn advertised(system: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut inside = false;
        for line in system.lines() {
            if line.starts_with("## Your tools") {
                inside = true;
                continue;
            }
            if !inside {
                continue;
            }
            if line.starts_with("## ") {
                break;
            }
            if let Some(rest) = line.strip_prefix("- `") {
                if let Some(end) = rest.find('`') {
                    names.push(rest[..end].to_string());
                }
            }
        }
        names
    }

    /// A typed message: the words, and nothing pasted with them — the shape four of these tests
    /// care about, spelled once.
    fn ask_of(text: &str) -> Ask {
        Ask {
            text: text.to_string(),
            images: Vec::new(),
        }
    }

    #[test]
    fn a_body_must_name_the_message() {
        assert_eq!(
            message_from(br#"{"message":"hi"}"#).unwrap(),
            "hi".to_string()
        );
        assert!(message_from(b"").unwrap_err().contains("empty"));
        assert!(message_from(b"not json").unwrap_err().contains("not JSON"));
        assert!(message_from(br#"{"text":"hi"}"#)
            .unwrap_err()
            .contains("`message`"));
        assert!(message_from(br#"{"message":"  "}"#)
            .unwrap_err()
            .contains("nothing to answer"));
    }

    const PIXEL: &str = "data:image/png;base64,iVBORw0KGgo=";

    #[test]
    fn a_body_may_carry_pictures_beside_its_words() {
        let ask =
            ask_from(format!(r#"{{"message":"what is this?","images":["{PIXEL}"]}}"#).as_bytes())
                .unwrap();
        assert_eq!(ask.text, "what is this?");
        assert_eq!(ask.images.len(), 1);
        // Optional, and absent is the ordinary case: a client that never heard of pictures sends
        // the same body it always did and gets the same message out.
        let typed = ask_from(br#"{"message":"hello"}"#).unwrap();
        assert!(typed.images.is_empty());
        assert_eq!(typed.text, "hello");
    }

    #[test]
    fn a_picture_must_be_bytes_and_not_a_link() {
        // A link here would tell the model provider to fetch it, which is a request this machine
        // never agreed to make — and a conversation that shows whatever that address serves later.
        let error = ask_from(br#"{"message":"look","images":["https://example.com/album/1.png"]}"#)
            .unwrap_err();
        assert!(error.contains("data:image/"), "{error}");
    }

    #[test]
    fn a_screenshot_with_no_words_is_still_something_to_answer() {
        let ask = ask_from(format!(r#"{{"message":"","images":["{PIXEL}"]}}"#).as_bytes()).unwrap();
        assert!(ask.text.is_empty(), "no words were invented for it");
        assert_eq!(ask.images.len(), 1);
        // With neither words nor pictures the old refusal still stands.
        assert!(ask_from(br#"{"message":""}"#)
            .unwrap_err()
            .contains("nothing to answer"));
    }

    #[test]
    fn a_body_is_refused_when_its_pictures_are_too_many_or_too_large() {
        let one = format!(r#""{PIXEL}""#);
        let many = [one.as_str(); MAX_IMAGES + 1].join(",");
        let too_many = format!(r#"{{"message":"look","images":[{many}]}}"#);
        assert!(ask_from(too_many.as_bytes())
            .unwrap_err()
            .contains("at most"));

        let huge = format!(
            r#"{{"message":"look","images":["data:image/png;base64,{}"]}}"#,
            "A".repeat(MAX_IMAGE_CHARS + 1)
        );
        assert!(ask_from(huge.as_bytes())
            .unwrap_err()
            .contains("characters"));

        // Not a list at all, and not a string inside it: both name the field they mean.
        assert!(
            ask_from(format!(r#"{{"message":"look","images":"{PIXEL}"}}"#).as_bytes())
                .unwrap_err()
                .contains("list")
        );
        assert!(ask_from(br#"{"message":"look","images":[42]}"#)
            .unwrap_err()
            .contains("`images[0]`"));
    }

    #[test]
    fn a_picture_reaches_the_model_as_part_of_the_users_own_message() {
        // The whole path, end to end in this process: a body with a pasted screenshot becomes a
        // user message the model is shown, with the words it was sent with.
        let scratch = Scratch::new("picture");
        let model = Arc::new(Scripted::new(vec![Ok(AssistantTurn {
            content: Some("A white pixel.".to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
        })]));
        let ask =
            ask_from(format!(r#"{{"message":"what is this?","images":["{PIXEL}"]}}"#).as_bytes())
                .unwrap();

        let reply = answer(
            Arc::clone(&model) as Arc<dyn Model>,
            &scratch.home,
            &scratch.app_dir(),
            &scratch.slug(),
            &ask,
            &Terms::default(),
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
        let seen = model.seen.lock().unwrap();
        let user = seen[0]
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .unwrap();
        assert_eq!(user.text(), "what is this?");
        assert_eq!(user.image_count(), 1);

        // And it is in the conversation afterwards, so the next turn still has the screenshot.
        let history = AgentPaths::for_app(scratch.app_dir()).history();
        let text = std::fs::read_to_string(&history).unwrap();
        assert!(text.contains("what is this?"), "{text}");
    }

    #[test]
    fn a_message_is_answered_by_the_model_and_the_conversation_is_kept() {
        let scratch = Scratch::new("answered");
        let model = Scripted::new(vec![Ok(AssistantTurn {
            content: Some("Bread, milk.".to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
        })]);

        let reply = answer(
            Arc::new(model),
            &scratch.home,
            &scratch.app_dir(),
            &scratch.slug(),
            &ask_of("what is on the list?"),
            &Terms::default(),
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(reply.body["text"], "Bread, milk.");
        assert_eq!(reply.body["changed"], false);
        assert_eq!(reply.body["rounds"], 1);

        // The conversation is on disk under the app, which is what makes a second message a
        // continuation rather than a fresh start.
        let history = AgentPaths::for_app(scratch.app_dir()).history();
        let text = std::fs::read_to_string(&history).unwrap();
        assert!(text.contains("what is on the list?"), "{text}");
        assert!(text.contains("Bread, milk."), "{text}");
    }

    #[test]
    fn a_tool_call_that_writes_marked_the_app_as_changed() {
        let scratch = Scratch::new("changed");
        let target = scratch.app_dir().join("list.txt");
        let model = Scripted::new(vec![
            Ok(AssistantTurn {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    kind: "function".to_string(),
                    function: deepnest_llm::FunctionCall {
                        name: "write".to_string(),
                        arguments: json!({
                            "file_path": target.display().to_string(),
                            "content": "bread\n",
                        })
                        .to_string(),
                    },
                }],
                finish_reason: FinishReason::ToolCalls,
            }),
            Ok(AssistantTurn {
                content: Some("Saved it.".to_string()),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
            }),
        ]);

        let reply = answer(
            Arc::new(model),
            &scratch.home,
            &scratch.app_dir(),
            &scratch.slug(),
            &ask_of("write the list"),
            &Terms::default(),
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(reply.body["changed"], true, "{}", reply.body);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "bread\n",
            "the tool ran inside the fence and wrote where it was told"
        );
    }

    #[test]
    fn a_directory_that_is_not_an_app_is_refused_before_a_turn_starts() {
        let scratch = Scratch::new("not-an-app");
        let stray = scratch.home.join("apps").join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        let model = Scripted::new(Vec::new());

        let reply = answer(
            Arc::new(model),
            &scratch.home,
            &stray,
            &Scratch::slug_of("stray"),
            &ask_of("hello"),
            &Terms::default(),
        );
        assert_eq!(reply.status, 404, "{}", reply.body);
        assert!(
            reply.body["error"].as_str().unwrap().contains("stray"),
            "{}",
            reply.body
        );
    }

    #[test]
    fn a_provider_refusal_is_the_users_request_and_not_a_server_fault() {
        let scratch = Scratch::new("refused");
        // Built as a refusal rather than classified into one: a 400 is read as an over-long
        // request and retried with a smaller history, which is a different path entirely.
        let model = Scripted::new(vec![Err(LlmError::content_risk("blocked by the provider"))]);

        let reply = answer(
            Arc::new(model),
            &scratch.home,
            &scratch.app_dir(),
            &scratch.slug(),
            &ask_of("something the provider will not answer"),
            &Terms::default(),
        );
        // The loop reads a refusal as an answer about the content and stops, and the route reports
        // it as the user's request to change — not as this server failing.
        assert_eq!(reply.status, 422, "{}", reply.body);
        assert!(
            reply.body["error"]
                .as_str()
                .is_some_and(|text| text.contains("blocked by the provider")),
            "{}",
            reply.body
        );
    }

    #[test]
    fn the_prompt_advertises_exactly_the_tools_the_request_offers() {
        // The whole cut in one assertion. Before it, the prompt named `subagent` — a tool this
        // dialog never wired — and left out `install_skill`, which it did wire. The model that
        // believed the sentence got "there is no tool called subagent" back.
        let scratch = Scratch::new("tool-truth");
        let model = Arc::new(Scripted::new(vec![Ok(AssistantTurn {
            content: Some("ok".to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
        })]));

        let reply = answer(
            Arc::clone(&model) as Arc<dyn Model>,
            &scratch.home,
            &scratch.app_dir(),
            &scratch.slug(),
            &ask_of("hello"),
            &Terms::default(),
        );
        assert_eq!(reply.status, 200, "{}", reply.body);

        let seen = model.seen.lock().unwrap();
        let system = seen[0][0].content.clone().unwrap_or_default();
        let offered: Vec<String> = model.offered.lock().unwrap()[0]
            .iter()
            .map(|definition| definition.function.name.clone())
            .collect();

        assert_eq!(
            advertised(&system),
            offered,
            "the sentence in front of the model and the schema beside it must be one list"
        );
        // Both seams are wired in a dialog turn, which is what made this dialog's tool set wrong
        // in the first place: it could only ever pick one of them.
        for expected in ["subagent", "install_skill"] {
            assert!(
                offered.iter().any(|name| name == expected),
                "{expected} is missing from {offered:?}"
            );
        }
        assert_eq!(offered.len(), 8, "{offered:?}");
    }

    #[test]
    fn the_system_prompt_carries_the_fence_and_the_app_url() {
        // The prompt is assembled here rather than in `crates/agent`, so a wiring mistake — a
        // missing mode, no writable roots — would leave the agent guessing. This runs a turn and
        // reads what the model was told.
        let scratch = Scratch::new("prompt");
        let model = Arc::new(Scripted::new(vec![Ok(AssistantTurn {
            content: Some("ok".to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
        })]));

        answer(
            Arc::clone(&model) as Arc<dyn Model>,
            &scratch.home,
            &scratch.app_dir(),
            &scratch.slug(),
            &ask_of("hello"),
            &Terms::default(),
        );

        let seen = model.seen.lock().unwrap();
        let system = seen
            .first()
            .and_then(|messages| messages.first())
            .and_then(|message| message.content.clone())
            .unwrap_or_default();
        assert!(system.contains("workspace-write"), "{system}");
        assert!(system.contains("/app/notes/"), "{system}");
    }
}
