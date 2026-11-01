//! The turn loop: ask the model, run what it asked for, feed the answers back, stop.
//!
//! Every stop condition is deliberate, because a loop with one exit is a loop that hangs when
//! that exit does not come:
//!
//! | the turn ends when | because |
//! |---|---|
//! | the model answers without asking for a tool | the request is met |
//! | the caller cancels | a cancelled turn that keeps looping holds the round it was cancelled to end |
//! | the round budget runs out | two tools that each ask for another tool is a loop, not progress |
//! | the provider refuses the content | a refusal is an answer, and resending cannot change it |
//! | the model cannot be reached | there is nothing to dispatch, and a fabricated answer is worse than none |
//!
//! The **cancellation checkpoint is not advisory**: it is checked before every request and before
//! every tool call in a batch, so a turn cancelled mid-batch runs no further command. And a
//! **refusal is never read as an over-long request**: a provider's content filter arrives as a
//! 400, and treating it as a length problem shrinks the history and resends the refused text —
//! forever.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use deepnest_llm::{AssistantTurn, ChatRequest, LlmError, Message, ToolDefinition};
use deepnest_sandbox::Grant;
use deepnest_tools::{Tool, ToolContext, ToolResult};
use serde_json::Value;

use crate::approve::{self, Approver};
use crate::session::{fit_tail, Record, Session};

/// The model, as this loop needs it.
///
/// A trait rather than `deepnest_llm::Client` for two reasons: the loop is then testable with no
/// network and no HTTP adapter, and the adapter itself (which lives in `crates/gateway`) can be
/// swapped without touching the loop.
pub trait Model: Send + Sync {
    /// One turn: the messages in, the reply out.
    fn turn(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<AssistantTurn, LlmError>;
}

impl Model for deepnest_llm::Client {
    fn turn(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<AssistantTurn, LlmError> {
        let request = ChatRequest::new(self.model().to_string(), messages.to_vec())
            .streaming()
            .with_tools(tools.to_vec());
        self.chat_stream(&request)
    }
}

/// A shared cancellation flag.
///
/// Shared rather than polled from a channel: cancelling must be possible from a request handler
/// that holds nothing but a clone of this, and it must be observable from inside a tool batch.
#[derive(Debug, Clone, Default)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel, from anywhere. Idempotent: a second cancel is a no-op, not an error.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// How far one turn may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Model calls in one turn, including tool round-trips.
    pub max_rounds: usize,
    /// Characters of conversation sent per request, system prompt included.
    pub max_history_chars: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rounds: 24,
            max_history_chars: 120_000,
        }
    }
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The model answered and asked for nothing further.
    Answered { text: String, rounds: usize },
    /// The caller cancelled. No further tool ran.
    Cancelled { note: String },
    /// The round budget ran out with the model still asking for work.
    RoundLimit { text: String, rounds: usize },
}

impl TurnOutcome {
    /// What the user sees.
    pub fn text(&self) -> &str {
        match self {
            TurnOutcome::Answered { text, .. } => text,
            TurnOutcome::Cancelled { note } => note,
            TurnOutcome::RoundLimit { text, .. } => text,
        }
    }
}

/// A turn that could not be completed at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// The provider refused the content. **Never** retried by shrinking: the text is the problem,
    /// not its size.
    Refused { reason: String },
    /// The model could not be reached, or answered with something unusable.
    Model { reason: String },
    /// The conversation file could not be read or written.
    Session { reason: String },
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Refused { reason } => write!(
                f,
                "the model provider refused this conversation: {reason}. Rewording it will not \
                 change the outcome; the content that triggered the filter has to change"
            ),
            AgentError::Model { reason } => write!(f, "the model call failed: {reason}"),
            AgentError::Session { reason } => {
                write!(f, "the conversation could not be saved: {reason}")
            }
        }
    }
}

impl std::error::Error for AgentError {}

/// The agent for one app.
pub struct Agent {
    model: Arc<dyn Model>,
    tools: Vec<Box<dyn Tool>>,
    ctx: ToolContext,
    session: Session,
    system: String,
    limits: Limits,
    /// Who is asked when a call wants more room than the session gives. `None` means nobody, and
    /// a request then comes back as a refusal saying so — which is the honest answer for an agent
    /// with no page in front of it, and is what every test without a person attached gets.
    approver: Option<Arc<dyn Approver>>,
}

impl Agent {
    /// A full agent: a model, the tools it may run, the context those tools read, and the
    /// conversation file.
    pub fn new(
        model: Arc<dyn Model>,
        tools: Vec<Box<dyn Tool>>,
        ctx: ToolContext,
        session: Session,
        system: impl Into<String>,
    ) -> Self {
        Self {
            model,
            tools,
            ctx,
            session,
            system: system.into(),
            limits: Limits::default(),
            approver: None,
        }
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Attach the person a widening request is put in front of.
    ///
    /// Separate from [`Agent::new`] rather than another argument, because an agent with nobody to
    /// ask is a legitimate agent — it refuses to widen, and says why — and because a constructor
    /// that took an `Option` would make every test pass `None` to say what it already means.
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = Some(approver);
        self
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The tool definitions, in the order the tools were given.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    /// Run one turn for this user message.
    pub fn run_turn(
        &self,
        user_text: &str,
        cancel: &Cancellation,
    ) -> Result<TurnOutcome, AgentError> {
        self.run_turn_with(user_text, &[], cancel)
    }

    /// Run one turn for a message that carries pictures as well as words.
    ///
    /// `images` are `data:` URLs, already validated by whoever read the request — the loop does not
    /// care where a picture came from, only that the model is looking at the one the user sent. They
    /// join the conversation as part of the user's own message, so a later turn replays them from
    /// the file exactly as this turn sent them, and a conversation read back tomorrow still has its
    /// screenshots.
    ///
    /// A message with no pictures takes the old path through [`Message::user`] rather than an
    /// equivalent constructor, so a text-only turn produces the same record it always did.
    pub fn run_turn_with(
        &self,
        user_text: &str,
        images: &[String],
        cancel: &Cancellation,
    ) -> Result<TurnOutcome, AgentError> {
        let opening = if images.is_empty() {
            Message::user(user_text)
        } else {
            Message::with_images(Some(user_text.to_string()), images.to_vec())
        };
        self.append(&opening)?;
        let definitions = self.definitions();
        let mut budget = self.limits.max_history_chars;
        let mut shrinks = 0usize;
        let mut last_text = String::new();

        for round in 0..self.limits.max_rounds {
            if cancel.is_cancelled() {
                return Ok(self.cancel_note(round));
            }
            let messages = self.request_messages(budget)?;
            let turn = match self.model.turn(&messages, &definitions) {
                Ok(turn) => turn,
                Err(error) => {
                    // A refusal is an answer about the content; a length complaint is an answer
                    // about the request. Only the second is worth acting on, and only twice.
                    if error.kind == deepnest_llm::FailureKind::ContentRisk {
                        let reason = error.body.clone();
                        self.note(format!("turn refused by the provider: {reason}"))?;
                        return Err(AgentError::Refused { reason });
                    }
                    let overlong = error.kind == deepnest_llm::FailureKind::ContextTooLong;
                    if overlong && shrinks < 2 && budget > 2_000 {
                        shrinks += 1;
                        budget /= 2;
                        self.note(format!(
                            "the request did not fit; the conversation is sent in at most {budget} \
                             characters from here, and the file keeps every record"
                        ))?;
                        continue;
                    }
                    self.note(format!("the model call failed: {}", error.body))?;
                    return Err(AgentError::Model { reason: error.body });
                }
            };

            if turn.finish_reason.is_refusal() {
                let reason = "the provider's finish reason was content_filter".to_string();
                self.note(format!("turn refused: {reason}"))?;
                return Err(AgentError::Refused { reason });
            }

            if let Some(text) = &turn.content {
                last_text = text.clone();
            }
            self.append(&Message::assistant_with_tools(
                turn.content.clone(),
                turn.tool_calls.clone(),
            ))?;

            if turn.tool_calls.is_empty() {
                return Ok(TurnOutcome::Answered {
                    text: last_text,
                    rounds: round + 1,
                });
            }

            for call in &turn.tool_calls {
                if cancel.is_cancelled() {
                    // The calls that did not run are named in the file, so a reader can tell a
                    // cancelled batch from a model that asked for less.
                    self.note(format!(
                        "turn cancelled before running {:?}; the remaining {} call(s) of this \
                         round did not run",
                        call.function.name,
                        turn.tool_calls.len()
                    ))?;
                    return Ok(TurnOutcome::Cancelled {
                        note: "the turn was cancelled; no further tool was run".to_string(),
                    });
                }
                let result = self.dispatch(call);
                self.append(&Message::tool(call.id.clone(), result.content))?;
            }
        }

        self.note(format!(
            "the round budget of {} ran out with the model still asking for work",
            self.limits.max_rounds
        ))?;
        Ok(TurnOutcome::RoundLimit {
            text: last_text,
            rounds: self.limits.max_rounds,
        })
    }

    /// Run one tool call, turning every failure into a result the model can read.
    ///
    /// Nothing here panics and nothing is silently dropped: a call whose arguments do not parse,
    /// or that names a tool this agent does not have, comes back as a refusal naming what is
    /// wrong and what exists. A model told only "error" will make the same call again.
    pub fn dispatch(&self, call: &deepnest_llm::ToolCall) -> ToolResult {
        let name = call.function.name.as_str();
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == name) else {
            let available: Vec<&str> = self.tools.iter().map(|tool| tool.name()).collect();
            return ToolResult::error(
                name.to_string(),
                format!(
                    "there is no tool called {name:?} in this session. The tools available are: {}. \
                     Nothing was run.",
                    available.join(", ")
                ),
            );
        };
        let arguments = if call.function.arguments.trim().is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str::<Value>(&call.function.arguments) {
                Ok(value) => value,
                Err(err) => {
                    let shown: String = call.function.arguments.chars().take(400).collect();
                    return ToolResult::error(
                        name.to_string(),
                        format!(
                            "the arguments for {name} are not valid JSON ({err}), so this call \
                             cannot be run. They were: {shown}"
                        ),
                    );
                }
            }
        };
        let (arguments, wanted) = match approve::take_request(&arguments, self.ctx.policy.mode()) {
            Ok(pair) => pair,
            Err(reason) => return ToolResult::refused(name.to_string(), reason),
        };
        // The context for *this* call. Only a widening produces a second one: otherwise the agent's
        // own is borrowed, so the path every call takes allocates nothing and "no approval, no new
        // policy" is visible in the code that runs.
        let widened;
        let ctx = match &wanted {
            None => &self.ctx,
            Some(request) => {
                // The jump table first, and before anybody is asked. A host shown "widen
                // workspace-write to read-only?" is being asked to approve a narrowing; a host
                // asked enough questions like that stops reading them.
                if let Err(refusal) = request.validate() {
                    return ToolResult::refused(
                        name.to_string(),
                        format!("{refusal}. Nothing was run."),
                    );
                }
                let Some(approver) = self.approver.as_ref() else {
                    return ToolResult::refused(
                        name.to_string(),
                        format!(
                            "this call asked to widen {} to {}, and there is nobody to ask: this \
                             agent has no page in front of it. Nothing was run. Do the work the \
                             current mode allows, or tell the user what needs the wider mode.",
                            request.from(),
                            request.to()
                        ),
                    );
                };
                match approver.ask(request) {
                    approve::Answer::Approved => {}
                    approve::Answer::Denied { reason } => {
                        let said = match reason.trim() {
                            "" => String::new(),
                            words => format!(" They said: {words}"),
                        };
                        return ToolResult::refused(
                            name.to_string(),
                            format!(
                                "the host read the request to widen {} to {} and refused it.{said} \
                                 Nothing was run. Change the approach rather than asking again for \
                                 the same widening.",
                                request.from(),
                                request.to()
                            ),
                        );
                    }
                    approve::Answer::Unanswered { reason } => {
                        return ToolResult::refused(
                            name.to_string(),
                            format!(
                                "nobody answered the request to widen {} to {} — {reason} — so the \
                                 call did not run. Tell the user what you needed and why; do not \
                                 repeat the call.",
                                request.from(),
                                request.to()
                            ),
                        );
                    }
                }
                // Approved. The grant is built from the session's own policy, which is *borrowed*
                // and never written to, and it is moved into this call's context and dropped with
                // it: the next call starts from the session's mode again, whatever this one got.
                match Grant::approve(request.clone(), &self.ctx.policy) {
                    Ok(grant) => {
                        widened =
                            ToolContext::new(grant.policy().clone(), self.ctx.workdir.clone())
                                .with_scan(self.ctx.scan.clone());
                        &widened
                    }
                    Err(refusal) => {
                        return ToolResult::refused(
                            name.to_string(),
                            format!("{refusal}. Nothing was run."),
                        )
                    }
                }
            }
        };

        let result = tool.call(&arguments, ctx);
        match &wanted {
            // The model is told both halves: that this call ran wider, and that the session did
            // not move. An agent told only the first would report a session that is not the one it
            // is running in, and the next call would surprise it.
            Some(request) => ToolResult {
                content: format!(
                    "[ran at {} — the host approved this widening for this call only; the session \
                     is still {}]\n{}",
                    request.to(),
                    self.ctx.policy.mode(),
                    result.content
                ),
                ..result
            },
            None => result,
        }
    }

    /// The messages for one request: the system prompt, then as much conversation as fits.
    fn request_messages(&self, budget: usize) -> Result<Vec<Message>, AgentError> {
        let mut history = self.session.messages().map_err(|err| AgentError::Session {
            reason: err.to_string(),
        })?;
        history.insert(0, Message::system(self.system.clone()));
        let (mut messages, _dropped) = fit_tail(&history, budget);
        // `fit_tail` keeps a leading system message that was in the input; with several it keeps
        // them all, and one prompt is what this loop sends.
        if messages.first().map(|message| message.role.as_str()) != Some("system") {
            messages.insert(0, Message::system(self.system.clone()));
        }
        Ok(messages)
    }

    fn append(&self, message: &Message) -> Result<(), AgentError> {
        self.session
            .append_message(message)
            .map(|_| ())
            .map_err(|err| AgentError::Session {
                reason: err.to_string(),
            })
    }

    fn note(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.session
            .append(&Record::note(text))
            .map_err(|err| AgentError::Session {
                reason: err.to_string(),
            })
    }

    fn cancel_note(&self, round: usize) -> TurnOutcome {
        let note = "the turn was cancelled before it finished; nothing further was run";
        let _ = self.note(format!("{note} (round {round})"));
        TurnOutcome::Cancelled {
            note: note.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_llm::{FailureKind, FinishReason, FunctionCall, ToolCall};
    use deepnest_sandbox::{Mode, Policy};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// A model whose replies are queued by the test, and which records what it was shown.
    struct ScriptedModel {
        replies: Mutex<VecDeque<Result<AssistantTurn, LlmError>>>,
        seen: Mutex<Vec<Vec<Message>>>,
    }

    impl ScriptedModel {
        fn new(replies: Vec<Result<AssistantTurn, LlmError>>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl Model for ScriptedModel {
        fn turn(
            &self,
            messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(answer("out of script")))
        }
    }

    fn answer(text: &str) -> AssistantTurn {
        AssistantTurn {
            content: Some(text.to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
        }
    }

    fn asks(calls: Vec<ToolCall>) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: calls,
            finish_reason: FinishReason::ToolCalls,
        }
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: format!("call_{name}"),
            kind: "function".into(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-agent-turn-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn agent(&self, model: Arc<dyn Model>) -> Agent {
            self.agent_with_tools(model, deepnest_tools::registry::all())
        }

        fn agent_with_tools(&self, model: Arc<dyn Model>, tools: Vec<Box<dyn Tool>>) -> Agent {
            let ctx = ToolContext::new(
                Policy::from_roots(
                    Mode::WorkspaceWrite,
                    self.base.clone(),
                    self.base.clone(),
                    self.base.clone(),
                ),
                self.base.clone(),
            );
            let session = Session::open(self.base.join("history.jsonl")).unwrap();
            Agent::new(model, tools, ctx, session, "you are the app agent")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_picture_reaches_the_model_with_the_words_that_came_with_it() {
        let scratch = Scratch::new("picture");
        let model = ScriptedModel::new(vec![Ok(answer("it is a cat"))]);
        let agent = scratch.agent(model.clone());
        let outcome = agent
            .run_turn_with(
                "what is this?",
                &["data:image/png;base64,iVBORw0KGgo=".to_string()],
                &Cancellation::new(),
            )
            .unwrap();
        assert_eq!(outcome.text(), "it is a cat");
        let seen = model.seen.lock().unwrap();
        let request = &seen[0];
        let user = request
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .unwrap();
        assert_eq!(user.text(), "what is this?");
        assert_eq!(user.image_count(), 1, "the model was shown the picture");
        // A picture does not displace the rules: the system prompt is still the first message.
        assert_eq!(request[0].role, "system");
    }

    #[test]
    fn a_turn_with_no_picture_is_the_turn_it_always_was() {
        let scratch = Scratch::new("no-picture");
        let model = ScriptedModel::new(vec![Ok(answer("ok"))]);
        let agent = scratch.agent(model.clone());
        agent.run_turn("hello", &Cancellation::new()).unwrap();
        let seen = model.seen.lock().unwrap();
        let user = seen[0]
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .unwrap();
        assert!(user.images.is_none());
        assert_eq!(user.content.as_deref(), Some("hello"));
    }

    #[test]
    fn a_turn_with_no_tool_call_answers_and_is_written_down() {
        let scratch = Scratch::new("answer");
        let model = ScriptedModel::new(vec![Ok(answer("here is your app"))]);
        let agent = scratch.agent(model.clone());
        let outcome = agent
            .run_turn("build me a note app", &Cancellation::new())
            .unwrap();
        assert_eq!(
            outcome,
            TurnOutcome::Answered {
                text: "here is your app".into(),
                rounds: 1
            }
        );
        let messages = agent.session().messages().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content.as_deref(), Some("build me a note app"));
        assert_eq!(messages[1].content.as_deref(), Some("here is your app"));
    }

    #[test]
    fn the_system_prompt_is_the_first_message_of_every_request() {
        let scratch = Scratch::new("system-first");
        let model = ScriptedModel::new(vec![Ok(answer("ok"))]);
        let agent = scratch.agent(model.clone());
        agent.run_turn("hello", &Cancellation::new()).unwrap();
        let seen = model.seen.lock().unwrap();
        let first = &seen[0];
        assert_eq!(first[0].role, "system");
        assert_eq!(first[0].content.as_deref(), Some("you are the app agent"));
        assert_eq!(first[1].role, "user");
    }

    #[test]
    fn a_tool_call_is_dispatched_and_its_result_fed_back() {
        let scratch = Scratch::new("dispatch");
        std::fs::write(scratch.base.join("a.txt"), "the file").unwrap();
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call("read", r#"{"file_path":"a.txt"}"#)])),
            Ok(answer("I read it")),
        ]);
        let agent = scratch.agent(model.clone());
        let outcome = agent.run_turn("read a.txt", &Cancellation::new()).unwrap();
        assert!(
            matches!(outcome, TurnOutcome::Answered { rounds: 2, .. }),
            "{outcome:?}"
        );
        assert_eq!(model.calls(), 2);
        let messages = agent.session().messages().unwrap();
        let tool_message = messages
            .iter()
            .find(|message| message.role == "tool")
            .expect("the tool result is in the conversation");
        assert!(tool_message
            .content
            .as_deref()
            .unwrap()
            .contains("the file"));
        assert_eq!(tool_message.tool_call_id.as_deref(), Some("call_read"));
    }

    #[test]
    fn a_tool_this_agent_does_not_have_is_a_result_the_model_can_read() {
        let scratch = Scratch::new("unknown-tool");
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call("browser", "{}")])),
            Ok(answer("sorry")),
        ]);
        let agent = scratch.agent(model);
        agent.run_turn("open a page", &Cancellation::new()).unwrap();
        let messages = agent.session().messages().unwrap();
        let result = messages
            .iter()
            .find(|message| message.role == "tool")
            .unwrap();
        let text = result.content.as_deref().unwrap();
        assert!(text.contains("no tool called"), "{text}");
        assert!(
            text.contains("bash"),
            "the available tools are named: {text}"
        );
    }

    #[test]
    fn malformed_arguments_are_refused_rather_than_guessed_at() {
        let scratch = Scratch::new("bad-args");
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call("read", "{not json")])),
            Ok(answer("ok")),
        ]);
        let agent = scratch.agent(model);
        agent.run_turn("read it", &Cancellation::new()).unwrap();
        let messages = agent.session().messages().unwrap();
        let text = messages
            .iter()
            .find(|message| message.role == "tool")
            .and_then(|message| message.content.clone())
            .unwrap();
        assert!(text.contains("not valid JSON"), "{text}");
        assert!(
            text.contains("{not json"),
            "the offending text is shown: {text}"
        );
    }

    #[test]
    fn a_cancelled_turn_makes_no_request() {
        let scratch = Scratch::new("cancel-first");
        let model = ScriptedModel::new(vec![Ok(answer("should not run"))]);
        let agent = scratch.agent(model.clone());
        let cancel = Cancellation::new();
        cancel.cancel();
        let outcome = agent.run_turn("build something", &cancel).unwrap();
        assert!(
            matches!(outcome, TurnOutcome::Cancelled { .. }),
            "{outcome:?}"
        );
        assert_eq!(model.calls(), 0, "a cancelled turn must not call the model");
        // The user's message is still on disk: they said it, and losing it is worse.
        let records = agent.session().records().unwrap();
        assert!(records.iter().any(|record| record.role == "user"));
    }

    #[test]
    fn cancelling_inside_a_tool_batch_stops_the_remaining_calls() {
        // The measured failure this prevents: a cancelled turn whose queued commands all ran.
        struct CancellingTool {
            cancel: Cancellation,
        }
        impl Tool for CancellingTool {
            fn name(&self) -> &'static str {
                "read"
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::function("read", "reads", serde_json::json!({"type": "object"}))
            }
            fn call(&self, _args: &Value, _ctx: &ToolContext) -> ToolResult {
                self.cancel.cancel();
                ToolResult::ok("read", "read once")
            }
        }
        let scratch = Scratch::new("cancel-batch");
        let cancel = Cancellation::new();
        let model = ScriptedModel::new(vec![Ok(asks(vec![
            call("read", "{}"),
            call("read", "{}"),
            call("read", "{}"),
        ]))]);
        let agent = scratch.agent_with_tools(
            model.clone(),
            vec![Box::new(CancellingTool {
                cancel: cancel.clone(),
            })],
        );
        let outcome = agent.run_turn("read three things", &cancel).unwrap();
        assert!(
            matches!(outcome, TurnOutcome::Cancelled { .. }),
            "{outcome:?}"
        );
        let responses = agent
            .session()
            .messages()
            .unwrap()
            .into_iter()
            .filter(|message| message.role == "tool")
            .count();
        assert_eq!(responses, 1, "only the call that ran has a result");
        assert_eq!(model.calls(), 1, "no second request after the cancel");
    }

    #[test]
    fn an_over_long_request_shrinks_and_tries_again() {
        let scratch = Scratch::new("shrink");
        let model = ScriptedModel::new(vec![
            Err(LlmError {
                status: Some(400),
                body: "maximum context length exceeded".into(),
                kind: FailureKind::ContextTooLong,
            }),
            Ok(answer("second time lucky")),
        ]);
        let agent = scratch.agent(model.clone());
        let outcome = agent
            .run_turn("a very long request", &Cancellation::new())
            .unwrap();
        assert_eq!(
            outcome,
            TurnOutcome::Answered {
                text: "second time lucky".into(),
                rounds: 2
            }
        );
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(
            seen[1].len() <= seen[0].len(),
            "the second request is not larger than the first"
        );
    }

    #[test]
    fn a_refusal_is_never_treated_as_an_over_long_request() {
        // The trap: a content filter also arrives as a 400, so shrinking and resending repeats
        // the refusal forever while looking like progress.
        let scratch = Scratch::new("refusal");
        let model = ScriptedModel::new(vec![
            Err(LlmError {
                status: Some(400),
                body: "Content Exists Risk".into(),
                kind: FailureKind::ContentRisk,
            }),
            Ok(answer("this must not be reached")),
        ]);
        let agent = scratch.agent(model.clone());
        let error = agent
            .run_turn("something the provider refuses", &Cancellation::new())
            .unwrap_err();
        assert!(matches!(error, AgentError::Refused { .. }), "{error:?}");
        assert_eq!(model.calls(), 1, "a refusal is not retried");
        // And nothing was removed from the file to make room.
        assert!(agent
            .session()
            .messages()
            .unwrap()
            .iter()
            .any(|message| message.content.as_deref() == Some("something the provider refuses")));
    }

    #[test]
    fn a_content_filter_finish_reason_is_a_refusal() {
        let scratch = Scratch::new("filter-finish");
        let model = ScriptedModel::new(vec![Ok(AssistantTurn {
            content: None,
            tool_calls: Vec::new(),
            finish_reason: FinishReason::ContentFilter,
        })]);
        let agent = scratch.agent(model);
        let error = agent
            .run_turn("anything", &Cancellation::new())
            .unwrap_err();
        assert!(matches!(error, AgentError::Refused { .. }), "{error:?}");
    }

    #[test]
    fn a_transport_failure_ends_the_turn_without_inventing_an_answer() {
        let scratch = Scratch::new("transport");
        let model = ScriptedModel::new(vec![Err(LlmError {
            status: None,
            body: "connection refused".into(),
            kind: FailureKind::Other,
        })]);
        let agent = scratch.agent(model);
        let error = agent.run_turn("hello", &Cancellation::new()).unwrap_err();
        assert!(matches!(error, AgentError::Model { .. }), "{error:?}");
    }

    #[test]
    fn the_round_budget_stops_a_model_that_keeps_asking_for_tools() {
        let scratch = Scratch::new("rounds");
        let replies: Vec<_> = (0..10)
            .map(|_| Ok(asks(vec![call("glob", r#"{"pattern":"*.none"}"#)])))
            .collect();
        let model = ScriptedModel::new(replies);
        let agent = scratch.agent(model.clone()).with_limits(Limits {
            max_rounds: 3,
            ..Limits::default()
        });
        let outcome = agent
            .run_turn("look forever", &Cancellation::new())
            .unwrap();
        assert!(
            matches!(outcome, TurnOutcome::RoundLimit { rounds: 3, .. }),
            "{outcome:?}"
        );
        assert_eq!(model.calls(), 3);
        let notes: Vec<String> = agent
            .session()
            .records()
            .unwrap()
            .into_iter()
            .filter(|record| record.kind == "note")
            .filter_map(|record| record.content)
            .collect();
        assert!(
            notes.iter().any(|note| note.contains("round budget")),
            "{notes:?}"
        );
    }

    #[test]
    fn two_turns_share_one_conversation() {
        let scratch = Scratch::new("two-turns");
        let model = ScriptedModel::new(vec![Ok(answer("first")), Ok(answer("second"))]);
        let agent = scratch.agent(model.clone());
        agent.run_turn("one", &Cancellation::new()).unwrap();
        agent.run_turn("two", &Cancellation::new()).unwrap();
        let messages = agent.session().messages().unwrap();
        assert_eq!(messages.len(), 4);
        let seen = model.seen.lock().unwrap();
        // system + user + assistant + the second turn's own user message.
        assert_eq!(
            seen[1].len(),
            4,
            "the second request carries the first turn"
        );
    }

    /// What a [`Reports`] tool was handed, readable after the turn.
    #[derive(Clone, Default)]
    struct Runs(Arc<Mutex<Vec<(String, Value)>>>);

    impl Runs {
        fn all(&self) -> Vec<(String, Value)> {
            self.0.lock().unwrap().clone()
        }
    }

    /// A tool that reports the mode it was given and the arguments it saw, so a widening is
    /// measured where the tool runs rather than where the loop decided.
    struct Reports {
        runs: Runs,
    }

    /// The tool and the handle the test reads it through.
    fn reports() -> (Box<dyn Tool>, Runs) {
        let runs = Runs::default();
        (Box::new(Reports { runs: runs.clone() }), runs)
    }

    impl Tool for Reports {
        fn name(&self) -> &'static str {
            "read"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::function("read", "reads", serde_json::json!({"type": "object"}))
        }
        fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
            self.runs
                .0
                .lock()
                .unwrap()
                .push((ctx.policy.mode().as_str().to_string(), args.clone()));
            ToolResult::ok("read", "read it")
        }
    }

    /// A person who answers from a script, so an approval is exercised with no page and no socket.
    struct Person {
        answer: crate::approve::Answer,
        asked: Mutex<Vec<deepnest_sandbox::EscalationRequest>>,
    }

    impl Person {
        fn new(answer: crate::approve::Answer) -> Arc<Self> {
            Arc::new(Self {
                answer,
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<deepnest_sandbox::EscalationRequest> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Approver for Person {
        fn ask(&self, request: &deepnest_sandbox::EscalationRequest) -> crate::approve::Answer {
            self.asked.lock().unwrap().push(request.clone());
            self.answer.clone()
        }
    }

    fn widening(from_request: &str) -> ToolCall {
        call(
            "read",
            &format!(r#"{{"request_mode":"danger-full-access","justification":"{from_request}"}}"#),
        )
    }

    #[test]
    fn an_approved_widening_widens_the_call_that_asked_and_no_other() {
        let scratch = Scratch::new("widened");
        let (tool, runs) = reports();
        let person = Person::new(crate::approve::Answer::Approved);
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![widening("the build writes outside the app")])),
            Ok(asks(vec![call("read", "{}")])),
            Ok(answer("done")),
        ]);
        let agent = scratch
            .agent_with_tools(model, vec![tool])
            .with_approver(person.clone());

        agent.run_turn("do both", &Cancellation::new()).unwrap();

        let runs = runs.all();
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs[0].0, "danger-full-access",
            "the approved call ran wider"
        );
        assert_eq!(
            runs[1].0, "workspace-write",
            "the next call is back at the session's mode"
        );
        assert_eq!(agent.ctx.policy.mode(), Mode::WorkspaceWrite);
        // The request keys are the loop's, not the tool's.
        assert_eq!(runs[0].1, serde_json::json!({}), "{:?}", runs[0].1);
        assert_eq!(person.asked().len(), 1, "one person, one question");
        assert_eq!(person.asked()[0].to(), Mode::DangerFullAccess);
    }

    #[test]
    fn an_approved_call_says_it_ran_wider_and_that_the_session_did_not_move() {
        let scratch = Scratch::new("widened-said");
        let (tool, _runs) = reports();
        let person = Person::new(crate::approve::Answer::Approved);
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![widening("needs to write to the toolchain")])),
            Ok(answer("done")),
        ]);
        let agent = scratch
            .agent_with_tools(model, vec![tool])
            .with_approver(person);
        agent.run_turn("do it", &Cancellation::new()).unwrap();

        let text = agent
            .session()
            .messages()
            .unwrap()
            .into_iter()
            .find(|message| message.role == "tool")
            .and_then(|message| message.content)
            .expect("the tool result is in the conversation");
        assert!(text.contains("ran at danger-full-access"), "{text}");
        assert!(text.contains("this call only"), "{text}");
        assert!(text.contains("still workspace-write"), "{text}");
        assert!(
            text.contains("read it"),
            "the tool's own words survive: {text}"
        );
    }

    #[test]
    fn a_request_that_does_not_widen_never_reaches_a_person() {
        let scratch = Scratch::new("no-widen");
        let (tool, runs) = reports();
        let person = Person::new(crate::approve::Answer::Approved);
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call(
                "read",
                r#"{"request_mode":"workspace-write","justification":"same thing again"}"#,
            )])),
            Ok(answer("ok")),
        ]);
        let agent = scratch
            .agent_with_tools(model, vec![tool])
            .with_approver(person.clone());
        agent.run_turn("try it", &Cancellation::new()).unwrap();

        assert!(runs.all().is_empty(), "nothing ran");
        assert!(
            person.asked().is_empty(),
            "the jump table answers before the host is bothered"
        );
        let text = agent
            .session()
            .messages()
            .unwrap()
            .into_iter()
            .find(|message| message.role == "tool")
            .and_then(|message| message.content)
            .unwrap();
        assert!(text.contains("does not widen"), "{text}");
    }

    #[test]
    fn a_host_who_says_no_runs_nothing_and_their_words_reach_the_model() {
        let scratch = Scratch::new("denied");
        let (tool, runs) = reports();
        let person = Person::new(crate::approve::Answer::Denied {
            reason: "not for a note app".to_string(),
        });
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![widening("wants the whole disk")])),
            Ok(answer("understood")),
        ]);
        let agent = scratch
            .agent_with_tools(model, vec![tool])
            .with_approver(person);
        agent.run_turn("do it", &Cancellation::new()).unwrap();

        assert!(runs.all().is_empty(), "a refused widening runs nothing");
        let text = agent
            .session()
            .messages()
            .unwrap()
            .into_iter()
            .find(|message| message.role == "tool")
            .and_then(|message| message.content)
            .unwrap();
        assert!(text.contains("refused it"), "{text}");
        assert!(text.contains("not for a note app"), "{text}");
        assert!(text.contains("rather than asking again"), "{text}");
    }

    #[test]
    fn an_unanswered_request_is_not_a_refusal_and_says_what_to_do_instead() {
        let scratch = Scratch::new("unanswered");
        let (tool, _runs) = reports();
        let person = Person::new(crate::approve::Answer::Unanswered {
            reason: "nobody answered within two minutes".to_string(),
        });
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![widening("nobody is looking")])),
            Ok(answer("I will explain")),
        ]);
        let agent = scratch
            .agent_with_tools(model, vec![tool])
            .with_approver(person);
        agent.run_turn("do it", &Cancellation::new()).unwrap();
        let text = agent
            .session()
            .messages()
            .unwrap()
            .into_iter()
            .find(|message| message.role == "tool")
            .and_then(|message| message.content)
            .unwrap();
        assert!(text.contains("nobody answered"), "{text}");
        assert!(text.contains("Tell the user"), "{text}");
    }

    #[test]
    fn an_agent_with_no_page_in_front_of_it_refuses_to_widen_rather_than_widening() {
        let scratch = Scratch::new("no-approver");
        let (tool, runs) = reports();
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![widening("nobody to ask")])),
            Ok(answer("ok")),
        ]);
        let agent = scratch.agent_with_tools(model, vec![tool]);
        agent.run_turn("do it", &Cancellation::new()).unwrap();

        assert!(runs.all().is_empty(), "no approval, no widest mode");
        let text = agent
            .session()
            .messages()
            .unwrap()
            .into_iter()
            .find(|message| message.role == "tool")
            .and_then(|message| message.content)
            .unwrap();
        assert!(text.contains("nobody to ask"), "{text}");
    }
}
