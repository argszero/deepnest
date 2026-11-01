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

use base64::Engine as _;
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

/// The most files one message may carry.
///
/// The same reasoning as [`MAX_IMAGES`], and the same number: one message is one question, and a
/// question that arrives with nine files is an upload that has not decided what it is about.
pub const MAX_FILES: usize = 8;

/// The largest the files of one message may be together, in bytes of content.
///
/// A quarter of `deepnest_gateway::http::MAX_BODY`, on purpose: base64 makes the content a third
/// larger again on the wire, and the pictures of the same message may be using most of the rest.
/// [`MAX_FILES`] files of this size would not fit in one request, which is why one message's files
/// share this budget rather than each getting it.
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

/// How much of one file's text is written into the message the model reads.
///
/// A file may be [`MAX_FILE_BYTES`] long, which is far past any model's context. The alternatives
/// to a cut are worse: refusing the message hides why, and sending the lot spends the provider's
/// whole window on one attachment and fails the turn with nothing to point at. The cut says where
/// it is, so the model can ask for the rest rather than answer from half a file it believes whole.
pub const MAX_INLINE_CHARS: usize = 100_000;

/// The line [`with_files`] puts between the person's words and the files those words point at.
///
/// **Public because the transcript cuts here.** The conversation file records the message the model
/// was shown — the words and the contents — because that is what a replay has to hand the model
/// again. The page, though, is showing a person their own sentence, and a csv pasted into that
/// sentence turns one line of chat into a screen of data. So `crate::history` cuts at this line and
/// reports a count instead, which is the same trade `pictures` already makes for a screenshot's
/// megabytes. One constant rather than two spellings of the same literal: a cut that misses is a
/// transcript that silently grows a file dump.
pub const FILE_SECTION: &str =
    "\n\nThe message names these files. Each heading is the marker exactly as it appears in the \
     words above, and the file's contents follow it.\n";

/// One file the message points at: the name the person's own file system spells, and its bytes.
///
/// The bytes rather than a path, because a path is a promise about a disk that outlives the
/// conversation: the file it names can be moved, edited, or never exist. What was pasted is what
/// travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carried {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// One message as the dialog sends it: the words, and any pictures and files pasted with them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ask {
    pub text: String,
    /// `data:` URLs, in the order they were pasted.
    pub images: Vec<String>,
    /// Files the words point at, in the order the words mention them.
    pub files: Vec<Carried>,
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
    let files = files_from(&value)?;
    let message = value.get("message").and_then(Value::as_str);
    let words = message.map(str::trim).unwrap_or_default();
    if words.is_empty() && images.is_empty() && files.is_empty() {
        return Err(if message.is_none() {
            "the body must carry a string field named `message`".to_string()
        } else {
            "`message` is empty — there is nothing to answer".to_string()
        });
    }
    Ok(Ask {
        text: message.unwrap_or_default().to_string(),
        images,
        files,
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

/// Read the `files` field, if the body has one: `[{"name": "report.csv", "data": "<base64>"}]`.
///
/// **Base64 of the bytes, not a path and not a URL.** A path would name a file this process has to
/// open on the strength of a string a page chose, and the page is an app's page — the thing the
/// agent's policy exists to fence off. The bytes the person pasted are the thing they meant; they
/// travel in the request that carries their words, and nothing is read off a disk because of them.
///
/// A name is what the marker in the words holds, so the rules on it are the marker's rules: a `]`
/// would end the marker early, a newline would end the line it sits on, and a path separator names
/// something other than a single file. Everything else is the person's own filename, kept as it is.
fn files_from(value: &Value) -> Result<Vec<Carried>, String> {
    let Some(field) = value.get("files") else {
        return Ok(Vec::new());
    };
    if field.is_null() {
        return Ok(Vec::new());
    }
    let array = field.as_array().ok_or_else(|| {
        "`files` must be a list of `{\"name\": \"...\", \"data\": \"<base64>\"}` objects"
            .to_string()
    })?;
    if array.len() > MAX_FILES {
        return Err(format!(
            "one message carries at most {MAX_FILES} files; this one has {}",
            array.len()
        ));
    }
    let mut files = Vec::with_capacity(array.len());
    let mut total = 0usize;
    for (index, entry) in array.iter().enumerate() {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("`files[{index}].name` must be a string"))?;
        let name = checked_name(index, name)?;
        let data = entry
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("`files[{index}].data` must be a base64 string"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| format!("`files[{index}].data` is not base64: {error}"))?;
        total += bytes.len();
        if total > MAX_FILE_BYTES {
            return Err(format!(
                "the files in one message come to at most {MAX_FILE_BYTES} bytes; this one has \
                 {total}"
            ));
        }
        files.push(Carried { name, bytes });
    }
    Ok(files)
}

/// A file's name as the marker and the model will both carry it, or the reason it may not.
fn checked_name(index: usize, name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "`files[{index}].name` is empty — a file needs a name to be pointed at"
        ));
    }
    if trimmed.chars().count() > 120 {
        return Err(format!(
            "`files[{index}].name` is longer than 120 characters, which no marker in a sentence \
             survives"
        ));
    }
    if let Some(bad) = trimmed
        .chars()
        .find(|character| matches!(character, ']' | '/' | '\\') || character.is_control())
    {
        return Err(format!(
            "`files[{index}].name` contains {bad:?}, which cannot appear in the marker the message \
             names the file by"
        ));
    }
    Ok(trimmed.to_string())
}

/// The person's words, followed by the contents of the files those words point at.
///
/// **Why the contents are here rather than at a path.** The marker is the position — `[file
/// report.csv]` sits where the sentence mentions the file — so the name the model matches is the
/// name in the words above it, and what it needs next is the file itself. Writing the bytes into
/// the app's directory and naming the path instead would leave a copy that nothing ever deletes:
/// every pasted file would outlive its message, and one deleted from the message would still be
/// sitting in the app it was pasted into.
///
/// A message with no files returns its text unchanged — the exact string this route sent before
/// files existed, which is what keeps the typed-message path byte-identical.
fn with_files(text: &str, files: &[Carried]) -> String {
    if files.is_empty() {
        return text.to_string();
    }
    let mut assembled = String::from(text);
    assembled.push_str(FILE_SECTION);
    for file in files {
        assembled.push_str(&format!(
            "\n[file {}] — {} bytes\n",
            file.name,
            file.bytes.len()
        ));
        let Ok(contents) = std::str::from_utf8(&file.bytes) else {
            // Named and counted rather than decoded lossily: half a zip file is worth less than a
            // line saying what was left out.
            assembled.push_str("(not shown: this file is not text)\n");
            continue;
        };
        let shown: String = contents.chars().take(MAX_INLINE_CHARS).collect();
        let fence = fence_around(&shown);
        assembled.push_str(&fence);
        assembled.push('\n');
        assembled.push_str(&shown);
        if !shown.ends_with('\n') {
            assembled.push('\n');
        }
        assembled.push_str(&fence);
        assembled.push('\n');
        if shown.chars().count() < contents.chars().count() {
            assembled.push_str(&format!(
                "(the rest of this file is not shown: it is longer than {MAX_INLINE_CHARS} \
                 characters)\n"
            ));
        }
    }
    assembled
}

/// A fence no line of `contents` can end early.
///
/// A file of markdown or source is exactly the kind of file a person pastes, and it may contain
/// three backticks of its own. The fence is then one longer than the longest run inside.
fn fence_around(contents: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for byte in contents.bytes() {
        if byte == b'`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
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
    // The session the page is on, not the file every app used to have only one of: a turn writes
    // where the drawer says the page is looking, and `sessions` is the one place that decides.
    let session = match Session::open(crate::sessions::history_path(app_dir)) {
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
    // What the model is shown: the person's words, and the contents of every file those words
    // point at, under a heading that repeats the marker. Built here rather than stored in `Ask` so
    // that a message with no files produces exactly the string this route sent before files
    // existed — the typed-message path stays byte-identical.
    let words = with_files(&ask.text, &ask.files);
    let outcome = match agent.run_turn_watched(&words, &ask.images, &terms.cancel, on_text) {
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
            files: Vec::new(),
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

    /// One file entry as the dialog sends it: a name, and the base64 of its bytes.
    fn file_json(name: &str, contents: &str) -> String {
        format!(
            r#"{{"name":"{name}","data":"{}"}}"#,
            base64::engine::general_purpose::STANDARD.encode(contents)
        )
    }

    #[test]
    fn a_body_may_carry_files_beside_its_words() {
        let ask = ask_from(
            format!(
                r#"{{"message":"what is wrong with [file report.csv]?","files":[{}]}}"#,
                file_json("report.csv", "id,total\n1,2\n")
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(ask.files.len(), 1);
        assert_eq!(ask.files[0].name, "report.csv");
        assert_eq!(ask.files[0].bytes, b"id,total\n1,2\n");
        // Absent is the ordinary case: a client that never heard of files sends the body it always
        // sent and gets the same message out.
        let typed = ask_from(br#"{"message":"hello"}"#).unwrap();
        assert!(typed.files.is_empty());
        // And a message that is nothing but a file is still something to answer, the same way a
        // message that is nothing but a picture is.
        let only_file = ask_from(
            format!(r#"{{"message":"","files":[{}]}}"#, file_json("a.txt", "x")).as_bytes(),
        )
        .unwrap();
        assert!(only_file.text.is_empty());
        assert_eq!(only_file.files.len(), 1);
    }

    #[test]
    fn a_file_arrives_where_the_words_name_it() {
        // The shape of the whole feature: the marker is the position, and the contents follow it.
        // The model is given one string, so what it reads in order is what the person wrote in
        // order — a file listed apart from its sentence would be a file the question never reaches.
        let words = "the total in [file report.csv] looks wrong";
        let ask = ask_from(
            format!(
                r#"{{"message":"{words}","files":[{}]}}"#,
                file_json("report.csv", "id,total\n1,2\n")
            )
            .as_bytes(),
        )
        .unwrap();
        let text = with_files(&ask.text, &ask.files);
        assert!(text.starts_with(words), "{text}");
        // The heading repeats the marker verbatim, so the name the model matches is the name in
        // the words above it.
        assert!(text.contains("[file report.csv]"), "{text}");
        assert!(text.contains("id,total\n1,2"), "{text}");
        // A message with no files is the exact string this route sent before files existed.
        assert_eq!(with_files("plain words", &[]), "plain words");
    }

    #[test]
    fn a_file_that_is_not_text_is_named_and_not_mangled() {
        // A zip, a picture pasted as a file, a database: naming it and saying so is worth more
        // than a lossy conversion that the model would read as the file's content.
        let text = with_files(
            "what is in [file archive.zip]?",
            &[Carried {
                name: "archive.zip".to_string(),
                bytes: vec![0x50, 0x4b, 0x03, 0x04, 0xff, 0xfe],
            }],
        );
        assert!(text.contains("[file archive.zip] — 6 bytes"), "{text}");
        assert!(text.contains("not text"), "{text}");
    }

    #[test]
    fn a_fence_grows_past_what_the_file_itself_contains() {
        // A pasted file is exactly as likely to be markdown or source as it is to be a csv, and a
        // fence the contents can close early is a fence the model reads as the end of the file.
        let text = with_files(
            "look at [file readme.md]",
            &[Carried {
                name: "readme.md".to_string(),
                bytes: b"# title\n\n```\ncode\n```\n".to_vec(),
            }],
        );
        assert!(text.contains("````\n# title"), "{text}");
    }

    #[test]
    fn a_file_longer_than_a_context_window_is_cut_and_the_cut_is_stated() {
        let long = "a".repeat(MAX_INLINE_CHARS + 10);
        let text = with_files(
            "summarise [file long.txt]",
            &[Carried {
                name: "long.txt".to_string(),
                bytes: long.into_bytes(),
            }],
        );
        assert!(
            text.contains("the rest of this file is not shown"),
            "{text}"
        );
        // The whole file was not sent: the point of the cut is that it is a cut.
        assert!(
            text.chars().count() < MAX_INLINE_CHARS + 500,
            "{}",
            text.chars().count()
        );
    }

    #[test]
    fn a_file_must_have_a_name_a_marker_can_hold() {
        // The name is carried into the words the model reads and into the marker the person's own
        // text holds, so the two characters that would break either are refused by name.
        for (name, expected) in [
            ("", "empty"),
            ("a]b.csv", "cannot appear"),
            ("../etc/passwd", "cannot appear"),
            ("dir/report.csv", "cannot appear"),
        ] {
            let body =
                format!(r#"{{"message":"look","files":[{{"name":"{name}","data":"eA=="}}]}}"#);
            let error = ask_from(body.as_bytes()).unwrap_err();
            assert!(error.contains(expected), "{name}: {error}");
        }
        // The data has to be base64, and the field has to be an object: each refusal names the
        // entry it is about rather than the list.
        assert!(
            ask_from(br#"{"message":"look","files":[{"name":"a.txt","data":"!!!"}]}"#)
                .unwrap_err()
                .contains("not base64")
        );
        assert!(ask_from(br#"{"message":"look","files":["a.txt"]}"#)
            .unwrap_err()
            .contains("`files[0].name`"));
        assert!(ask_from(br#"{"message":"look","files":"a.txt"}"#)
            .unwrap_err()
            .contains("list"));
    }

    #[test]
    fn a_bodys_files_are_refused_when_there_are_too_many_or_they_are_too_large() {
        let one = file_json("a.txt", "x");
        let many = [one.as_str(); MAX_FILES + 1].join(",");
        let too_many = format!(r#"{{"message":"look","files":[{many}]}}"#);
        assert!(ask_from(too_many.as_bytes())
            .unwrap_err()
            .contains("at most"));

        // One file past the budget, and two that are each fine but together are not: the quota is
        // the message's, not each file's.
        let huge = format!(
            r#"{{"message":"look","files":[{{"name":"big.bin","data":"{}"}}]}}"#,
            base64::engine::general_purpose::STANDARD.encode(vec![b'x'; MAX_FILE_BYTES + 1])
        );
        assert!(ask_from(huge.as_bytes()).unwrap_err().contains("bytes"));
        let half = MAX_FILE_BYTES / 2 + 1;
        let part = format!(
            r#"{{"name":"part.bin","data":"{}"}}"#,
            base64::engine::general_purpose::STANDARD.encode(vec![b'x'; half])
        );
        let together = format!(r#"{{"message":"look","files":[{part},{part}]}}"#);
        assert!(ask_from(together.as_bytes())
            .unwrap_err()
            .contains("the files in one message"));
    }

    #[test]
    fn a_file_reaches_the_model_with_the_words_that_point_at_it() {
        // The whole path, in this process: the body becomes one user message whose text holds the
        // person's words and the file's contents, in that order.
        let scratch = Scratch::new("file");
        let model = Arc::new(Scripted::new(vec![Ok(AssistantTurn {
            content: Some("The totals column is empty.".to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
        })]));
        let ask = ask_from(
            format!(
                r#"{{"message":"what is wrong with [file report.csv]?","files":[{}]}}"#,
                file_json("report.csv", "id,total\n1,2\n")
            )
            .as_bytes(),
        )
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
        let text = user.text();
        assert!(
            text.starts_with("what is wrong with [file report.csv]?"),
            "{text}"
        );
        assert!(text.contains("id,total\n1,2"), "{text}");
        assert_eq!(user.image_count(), 0, "a file is not a picture");
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

    #[test]
    fn the_body_bound_admits_the_pictures_this_route_accepts() {
        // Two numbers in two crates that have to agree, and did not. Neither is wrong on its own —
        // a screenshot-sized budget for pictures, a bound on what one request can make the process
        // hold — and only the pair was, which is why the pair is what is asserted here. Nothing did,
        // and the disagreement surfaced as a `413` on a pasted screenshot: this route advertising
        // 8,000,000 characters of pictures that the HTTP layer underneath would not read.
        let envelope = deepnest_gateway::http::MAX_BODY;
        assert!(
            envelope > MAX_IMAGE_CHARS,
            "the body bound ({envelope}) cannot carry a message of nothing but pictures \
             ({MAX_IMAGE_CHARS} characters), so the route refuses what it says it takes"
        );
        // Not merely by a byte, either: the words and the JSON around the pictures travel in the
        // same body, and a bound that fits the pictures alone still refuses a real message.
        assert!(
            envelope - MAX_IMAGE_CHARS >= 1024 * 1024,
            "the body bound ({envelope}) leaves {} bytes for the words and the JSON around the \
             pictures",
            envelope - MAX_IMAGE_CHARS
        );

        // And the same pair again, for the other thing one message may carry: files go in as
        // base64, so a full budget of them costs a third more on the wire than on disk. The worst
        // case is one message with every picture and every file the quotas allow, which is what
        // this adds up — a bound below it would refuse the message the route says it takes.
        let files_on_the_wire = MAX_FILE_BYTES.div_ceil(3) * 4;
        assert!(
            envelope > MAX_IMAGE_CHARS + files_on_the_wire,
            "the body bound ({envelope}) cannot carry the pictures ({MAX_IMAGE_CHARS} characters) \
             and the files ({MAX_FILE_BYTES} bytes, {files_on_the_wire} as base64) of one message"
        );
        assert!(
            envelope - MAX_IMAGE_CHARS - files_on_the_wire >= 1024 * 1024,
            "the body bound ({envelope}) leaves {} bytes for the words around a message's \
             pictures and files",
            envelope - MAX_IMAGE_CHARS - files_on_the_wire
        );
    }
}
