//! One message in, one turn out: the agent behind the app's dialog.
//!
//! Everything shipped before this file could be exercised without a program — a loop, a sandbox, a
//! skill loader — and the dialog still could not answer a word, because nothing joined them. This
//! is that join, and it is deliberately the one place that knows all of them at once:
//!
//! - **One agent per app, and its state lives in the app's own directory** ([`AgentPaths`]), so
//!   deleting an app deletes its conversation and backing an app up backs the conversation up.
//! - **Every tool call goes through the fence.** The tools are `crates/tools`' six leaves and the
//!   policy is `crates/sandbox`'s for this app: the app directory, the toolchain and temp — the
//!   three roots `writable_roots` already derives, so the file tools and the shell agree because
//!   they are reading the same answer.
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
//! - **The answer arrives whole.** `crate::model::Endpoint` says why: the HTTP adapter this build
//!   ships sends a request and reads a response, and a streaming path would have to be a second
//!   adapter. The turn is the same turn either way; what is missing is watching it being written.
//! - **A writing turn does not rebuild the app.** The reply says `changed`, and building is
//!   `POST /api/apps/<slug>/build` followed by `restart` — two routes that already exist. A build
//!   inside this request would hold one HTTP connection for a minute of `bun install`, which is a
//!   longer request than any browser waits for.
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
    prompt::system_prompt, Agent, AgentError, AgentPaths, Approver, Cancellation, MemoryStore,
    Model, PromptContext, Session, TurnOutcome,
};
use deepnest_apps::{manifest, Slug};
use deepnest_llm::ToolDefinition;
use deepnest_sandbox::{roots::writable_roots, Mode, Policy};
use deepnest_tools::{registry, Tool, ToolContext, ToolResult};
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

/// Read `{"message": "..."}` out of a request body.
///
/// The body and not the query for the same reason `POST /api/config` takes one: what the user types
/// is their own text, and a URL is written into the browser's history and into whatever logs it.
pub fn message_from(body: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the body must be UTF-8 JSON")?;
    if text.trim().is_empty() {
        return Err("the body must be `{\"message\": \"...\"}` — it was empty".to_string());
    }
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("the body is not JSON: {error}"))?;
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| "the body must carry a string field named `message`".to_string())?;
    if message.trim().is_empty() {
        return Err("`message` is empty — there is nothing to answer".to_string());
    }
    Ok(message.to_string())
}

/// Run one message through the app's agent.
///
/// `home` is the deepnest home (`~/.deepnest`) and `app_dir` the app's own directory; `slug` names
/// it in the prompt and in the policy. The app is checked for a manifest first: an agent pointed at
/// a directory that is not an app would edit whatever is there, and "no such app" is a better
/// answer than a turn spent on the wrong directory.
///
/// `approver` is who a call's widening request is put in front of. `None` is a real answer and not a
/// degraded one: an agent with nobody to ask refuses to widen and says so, which is what a test and
/// any caller without a page behind it should get.
pub fn answer(
    model: Arc<dyn Model>,
    home: &Path,
    app_dir: &Path,
    slug: &Slug,
    message: &str,
    approver: Option<Arc<dyn Approver>>,
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

    let system = system_prompt(
        &PromptContext::new(
            slug.as_str(),
            app_dir.display().to_string(),
            Mode::WorkspaceWrite.as_str(),
        )
        .with_writable(writable)
        .with_memory_index(memory)
        .with_skills(skills)
        .with_app_url(format!("/app/{slug}/")),
    );

    // One flag shared with every tool: the loop owns the turn, so what a tool touched is recorded
    // where the tools run and read where the answer is assembled.
    let changed = Arc::new(AtomicBool::new(false));
    let tools: Vec<Box<dyn Tool>> = registry::all()
        .into_iter()
        .map(|tool| {
            Box::new(Recorder {
                changed: Arc::clone(&changed),
                inner: tool,
            }) as Box<dyn Tool>
        })
        .collect();

    let mut agent = Agent::new(
        model,
        tools,
        ToolContext::new(policy, app_dir),
        session,
        system,
    );
    if let Some(approver) = approver {
        agent = agent.with_approver(approver);
    }

    let outcome = match agent.run_turn(message, &Cancellation::new()) {
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
    }

    impl Scripted {
        fn new(replies: Vec<Result<AssistantTurn, LlmError>>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Model for Scripted {
        fn turn(
            &self,
            messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            self.seen.lock().unwrap().push(messages.to_vec());
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                panic!("the model was asked more times than the script has answers");
            }
            replies.remove(0)
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
            "what is on the list?",
            None,
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
            "write the list",
            None,
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
            "hello",
            None,
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
            "something the provider will not answer",
            None,
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
            "hello",
            None,
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
