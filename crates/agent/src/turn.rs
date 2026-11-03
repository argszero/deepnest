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
//! The **cancellation checkpoint is not advisory**: it is checked before every request, before
//! every tool call in a batch, and *inside* the model call — `Model::turn_until` hands the signal
//! to the adapter, which abandons the request at the socket. A turn cancelled mid-batch runs no
//! further command, and one cancelled mid-call does not wait that call out. And a **refusal is
//! never read as an over-long request**: a provider's content filter arrives as a 400, and treating
//! it as a length problem shrinks the history and resends the refused text — forever.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use deepnest_llm::{AssistantTurn, ChatRequest, LlmError, Message, ToolDefinition, Usage};
use deepnest_sandbox::Grant;
use deepnest_tools::{Tool, ToolContext, ToolResult};
use serde_json::Value;

use crate::approve::{self, Approver};
use crate::session::{
    fit_tail, fold, fold_start, fold_tool_results, Record, Session, SUMMARY_PREFIX,
};

/// The model, as this loop needs it.
///
/// A trait rather than `deepnest_llm::Client` for two reasons: the loop is then testable with no
/// network and no HTTP adapter, and the adapter itself (which lives in `crates/gateway`) can be
/// swapped without touching the loop.
pub trait Model: Send + Sync {
    /// The model this asks, when it knows. Recorded in the app's model log beside the request, so
    /// a log read a week later says which model produced an answer — the config file it came from
    /// may have changed since. `None` for a model with no name to give, which is every fake in a
    /// test and no reason to make one write a stub.
    fn model_name(&self) -> Option<&str> {
        None
    }

    /// One turn: the messages in, the reply out.
    fn turn(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<AssistantTurn, LlmError>;

    /// The same turn, reporting the answer's text as the model writes it.
    ///
    /// A default implementation rather than a required method, for the same reason
    /// `Transport::post_stream` has one: a model that cannot report early — a fake in a test, an
    /// adapter that only has a whole response — must not be made to write a stub to say so, and a
    /// loop that asks for fragments must still get its turn from one that stays quiet. The
    /// observer is then simply never called, which is the truthful report of a model that had
    /// nothing to say early rather than an error.
    ///
    /// What is observable is *arrival*, not content: the returned turn is the same turn
    /// [`Self::turn`] would have produced. A caller that ignores `on_text` therefore loses nothing,
    /// which is what lets the loop always ask the watched way.
    fn turn_with(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        on_text: &mut dyn FnMut(&str),
    ) -> Result<AssistantTurn, LlmError> {
        let _ = on_text;
        self.turn(messages, tools)
    }

    /// The same turn, abandoned the moment `stop` says so.
    ///
    /// A model call is the longest thing in a turn, and this is the only hook that can reach
    /// *inside* one. Without it a stop is read between calls, so pressing it during a call means
    /// waiting out that call — measured on the running page as a send box locked for exactly as
    /// long as the call had left to run.
    ///
    /// The default is [`Self::turn_with`] with the signal ignored: an adapter that cannot abandon
    /// a request in flight still answers correctly, it just cannot end early. A model that can —
    /// the real one does — overrides this.
    fn turn_until(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        on_text: &mut dyn FnMut(&str),
        stop: &Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<AssistantTurn, LlmError> {
        let _ = stop;
        self.turn_with(messages, tools, on_text)
    }
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

    fn turn_with(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        on_text: &mut dyn FnMut(&str),
    ) -> Result<AssistantTurn, LlmError> {
        let request = ChatRequest::new(self.model().to_string(), messages.to_vec())
            .streaming()
            .with_tools(tools.to_vec());
        self.chat_stream_with(&request, on_text)
    }

    fn turn_until(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        on_text: &mut dyn FnMut(&str),
        stop: &Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<AssistantTurn, LlmError> {
        let request = ChatRequest::new(self.model().to_string(), messages.to_vec())
            .streaming()
            .with_tools(tools.to_vec());
        self.chat_stream_with_until(&request, on_text, Arc::clone(stop))
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
    ///
    /// This is a loop guard **and** a spend ceiling: every round is one model call, so the number
    /// is also the most one turn can bill. It was 24, and 24 was a number nobody chose — it arrived
    /// with the first version of this loop, with no reason written beside it. What that cost is
    /// measured rather than imagined: a turn spent all 24 walking one app's directory with `ls`,
    /// `find` and `glob`, reported "I'll start by looking at the app as it stands", and stopped
    /// before it had written a line.
    ///
    /// 540 was the owner's number, chosen to let a long turn finish. It is kept, but as a bound
    /// read off a real turn instead of a round number: in the one app on this machine with a long
    /// history (`apps/feishu/.agent/history.jsonl`, 9 turns, 1021 rounds), those 9 turns ran 24,
    /// 24, 24, 24, 24, 24, 24, 313 and 540 rounds — seven cut off by the old 24, one cut off by
    /// 540, and exactly one that ended on its own at **313**. So the ceiling has to sit above the
    /// only measured need and as close to it as prudence allows: 360 leaves a turn of that size
    /// about 15% headroom, and bounds a turn that would otherwise spend another 180 rounds
    /// re-sending its own conversation.
    ///
    /// What this number is **not**: a real bound on money. A round's cost is the conversation it
    /// re-sends, and in that same app 99% of it was tool output — which is why the rounds a turn
    /// needs is the thing to fix, and this ceiling only decides when to stop paying.
    pub max_rounds: usize,
    /// Characters of conversation sent per request, system prompt included.
    pub max_history_chars: usize,
    /// Messages a compaction keeps as they were, when the conversation outgrows the budget.
    ///
    /// It is the size of the window the summary has to describe everything outside of, and the
    /// number is a judgement rather than a measurement: too small and the agent loses the thread of
    /// what it was doing, too large and folding frees nothing. Six is one user turn with its tool
    /// round-trip and the answer, which is the shape a turn usually has.
    ///
    /// `usize::MAX` turns compaction off. Nothing can then be folded, because nothing counts as
    /// old, and the history is bounded the way it was before this existed — by leaving the oldest
    /// records out of the request, silently.
    pub compact_keep: usize,
    /// Tool results a request keeps in full, when it carries more than this many.
    ///
    /// Not a measured optimum but a reading of how a turn is shaped: the model runs about two
    /// calls per round, so six leaves this round and the two before it intact — the results a model
    /// is still working from — and folds everything older to a line.
    ///
    /// The bound this frees is the one the budget was really spent on. In the same measured app 99%
    /// of the characters sent over 1021 rounds were tool output, and every round re-sent all of it.
    /// Folding shortens the conversation without losing it, which the trim cannot do, so it runs
    /// first and the trim then finds room for more of the real conversation.
    ///
    /// `usize::MAX` turns folding off.
    pub fold_keep: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rounds: 360,
            max_history_chars: 120_000,
            compact_keep: 6,
            fold_keep: 6,
        }
    }
}

/// How a turn ended, and what it cost.
///
/// Every variant carries [`Usage`] because every variant spent something: a turn the budget stopped
/// and a turn the caller cancelled each made requests whose answers the provider billed for, and a
/// cost that exists only on the successful ending is a cost that disappears exactly where the money
/// went furthest. It is the sum over the turn's rounds — one round's price is the price of a
/// history that the next round sends again — and only the loop can add it up, since only the loop
/// sees each round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The model answered and asked for nothing further.
    Answered {
        text: String,
        rounds: usize,
        usage: Usage,
    },
    /// The caller cancelled. No further tool ran.
    Cancelled { note: String, usage: Usage },
    /// The round budget ran out with the model still asking for work.
    RoundLimit {
        text: String,
        rounds: usize,
        usage: Usage,
    },
    /// The batch reached calls that do not run in this process, and the turn is handed back.
    ///
    /// This is the shape a handoff takes, and it exists because of *whose* tools are being called.
    /// An app declares capabilities the agent cannot have — a todo list in its own database, an
    /// account behind its own CLI — and offers them over MCP. The obvious implementation is to call
    /// the endpoint from inside the turn. It deadlocks: the app made the request the turn is
    /// answering, and a program that answers one request at a time — which the app template
    /// generated until now is, and which an app the agent writes may well be again — cannot answer
    /// a callback from inside a request it has not finished. Concurrency on this side does not fix
    /// that; the wait is on the other side.
    ///
    /// So the turn does not call back. It runs every call of the round that **can** run here, states
    /// the rest in [`PendingCall`]s, and ends. The caller runs them in whatever way it likes and
    /// posts the answers back; [`Agent::resume`] continues the same turn from the same file. One
    /// round is one handoff, which is the batch rule the prompt already teaches the model, seen from
    /// the other side.
    ///
    /// Nothing about the pause lives in this process. What is owed is visible in `history.jsonl` —
    /// calls recorded with no answers after them — so an app that crashes between the handoff and
    /// the answers can read the same list back from [`crate::Session::owed`].
    Paused {
        text: String,
        rounds: usize,
        usage: Usage,
        pending: Vec<PendingCall>,
    },
}

/// One call the turn hands back: what the model asked for, addressed to whoever declared the tool.
///
/// `arguments` is the model's JSON, minus the `intent` field this workspace adds to every schema —
/// an app's tool never declared that property, and passing it through would be handing a program a
/// field it has no reason to accept.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingCall {
    /// Which call this answers, as the transcript spells it.
    pub call_id: String,
    /// The tool's name, as the server that declared it spelled it.
    pub name: String,
    /// Its arguments, as a JSON object in a string.
    pub arguments: String,
}

/// One answer to a [`PendingCall`], as the caller reports it back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallResult {
    pub call_id: String,
    /// What the model reads.
    pub content: String,
    /// Whether the call failed in its own terms — MCP's `isError`, or the caller's own judgement
    /// that the work did not happen. Not a refusal: nothing here was denied by a policy.
    #[serde(default)]
    pub is_error: bool,
}

impl TurnOutcome {
    /// What the user sees.
    pub fn text(&self) -> &str {
        match self {
            TurnOutcome::Answered { text, .. } => text,
            TurnOutcome::Cancelled { note, .. } => note,
            TurnOutcome::RoundLimit { text, .. } => text,
            TurnOutcome::Paused { text, .. } => text,
        }
    }

    /// What the whole turn cost, added up over every request it made.
    pub fn usage(&self) -> Usage {
        match self {
            TurnOutcome::Answered { usage, .. }
            | TurnOutcome::Cancelled { usage, .. }
            | TurnOutcome::RoundLimit { usage, .. }
            | TurnOutcome::Paused { usage, .. } => *usage,
        }
    }
}

/// One call, as a handoff spells it.
///
/// The arguments cross as the model wrote them, with one field removed: `intent` is added to every
/// schema by `deepnest_tools::registry` so a transcript row can say why a call was made, and a tool
/// an app declared never had that property — passing it on would be handing a program a field it
/// has no reason to accept, and an argument validator that rejects unknown properties would refuse
/// every call. A call whose arguments are not a JSON object cannot be run by anybody: no server
/// could dispatch it, which is why it becomes a result here rather than a round trip.
fn handoff(call: &deepnest_llm::ToolCall) -> Result<String, String> {
    let text = call.function.arguments.trim();
    let value: Value = if text.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(text).map_err(|error| {
            format!(
                "the arguments for {} are not valid JSON ({error}), so no server could run this \
                 call. They were: {}",
                call.function.name,
                text.chars().take(400).collect::<String>()
            )
        })?
    };
    let Value::Object(mut object) = value else {
        return Err(format!(
            "the arguments for {} must be a JSON object, and they were {value}",
            call.function.name
        ));
    };
    object.remove("intent");
    Ok(Value::Object(object).to_string())
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
    /// There is no handed-back turn to continue, or the answers do not match the calls owed.
    ///
    /// A separate ending from [`AgentError::Session`] because it is not a fault of the file: the
    /// file was read and said something, and what it said was that nothing is waiting. A caller
    /// that retries will get the same answer, which is why it is worth saying plainly.
    Paused { reason: String },
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
            AgentError::Paused { reason } => write!(f, "{reason}"),
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
    /// Where every request and its answer are recorded, when the caller wants them recorded.
    /// `None` is the default: a test has no app directory, and a turn that had to invent one to
    /// keep a log would be a turn that writes where it was not asked to.
    llm_log: Option<crate::llmlog::LlmLog>,
    /// Whether a call that does not run in this process ends the turn or is made here.
    ///
    /// `false` is right for a turn nobody is waiting on: the dialog's page asked for it, the app
    /// is idle, so calling its endpoint from inside the turn nests nothing and costs one round
    /// trip fewer. `true` is for a turn the **app itself** asked for — the app is then inside a
    /// request of its own, a program answering one request at a time cannot answer a callback from
    /// inside it, and that is the deadlock [`TurnOutcome::Paused`] exists to break.
    ///
    /// The same agent cannot do both: which of the two it is, is a fact about who is waiting, and
    /// that is decided by whoever assembled the turn.
    handoff: bool,
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
            llm_log: None,
            handoff: false,
        }
    }

    /// Hand a round's remote calls back instead of making them here.
    ///
    /// Set this when the caller **is** the process the calls would reach. A turn started by the
    /// app's own code runs inside a request that app has not finished answering; calling its tool
    /// endpoint from inside that turn asks a serial server to be in two places at once, and the
    /// turn waits for an answer that cannot come. See [`TurnOutcome::Paused`].
    ///
    /// A caller that leaves this alone keeps the old behaviour exactly: a remote tool is called,
    /// here and now, and the turn continues with its result.
    pub fn with_handoff(mut self, handoff: bool) -> Self {
        self.handoff = handoff;
        self
    }

    /// Record every request this turn sends and what came back, in this file.
    ///
    /// The log is diagnostic and its failure is not the turn's: a full disk costs the record, not
    /// the work. What is written, and why the file is not `history.jsonl`, is in [`crate::llmlog`].
    pub fn with_llm_log(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.llm_log = Some(crate::llmlog::LlmLog::new(path));
        self
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
    ///
    /// Through `registry::definitions` and not through `tool.definition()`, because that function is
    /// where the one field every call must state is added: `intent`. Reading the definitions
    /// straight off the tools was one line shorter and silently wrong — the schema sent to the model
    /// had no `intent` property, so no model ever wrote one, and the dialog's rows fell back to the
    /// raw arguments while the registry's own tests went on passing against a list nothing used.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        deepnest_tools::registry::definitions(&self.tools)
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
        self.run_turn_watched(user_text, images, cancel, &mut |_| {})
    }

    /// The same turn, with the model's text handed to `on_text` as it is written.
    ///
    /// The observer is a parameter of the *call* rather than a field of the agent, because who is
    /// watching is a property of this turn and not of the conversation: the same agent answers a
    /// request nobody is watching and one a page is drawing, and only the second has anywhere to
    /// put a fragment. Every round reports — a round that ends by asking for a tool has usually
    /// said something first, and holding that back until the round ends is the delay this exists
    /// to remove.
    ///
    /// It reports and changes nothing else: the conversation written to `history.jsonl`, the
    /// outcome and the stop conditions are exactly those of [`Self::run_turn_with`].
    pub fn run_turn_watched(
        &self,
        user_text: &str,
        images: &[String],
        cancel: &Cancellation,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<TurnOutcome, AgentError> {
        let opening = if images.is_empty() {
            Message::user(user_text)
        } else {
            Message::with_images(Some(user_text.to_string()), images.to_vec())
        };
        // A turn that was handed back and never answered is closed before a new one opens. A
        // provider refuses a request in which an assistant's calls have no answers, so an
        // abandoned handoff left in the file would make every later turn of this conversation fail
        // with a complaint about a batch nobody remembers. The calls did not run, and the
        // transcript says so rather than the conversation becoming unusable.
        if let Some(owed) = self
            .session
            .owed()
            .map_err(|err| AgentError::Session { reason: err.to_string() })?
        {
            for call in &owed.calls {
                self.append_result(
                    &Message::tool(
                        call.id.clone(),
                        format!(
                            "nothing reported a result for this call. The turn that asked for it \
                             was abandoned, so `{}` did not run.",
                            call.function.name
                        ),
                    ),
                    "failed",
                )?;
            }
            self.note(
                "a handed-back turn was abandoned without its results; its calls are recorded as \
                 not run",
            )?;
        }
        self.append(&opening)?;
        self.drive(cancel, on_text, 0)
    }

    /// Continue a turn that was handed back, once the caller has run the calls it took.
    ///
    /// `results` answers the calls [`TurnOutcome::Paused`] listed, and it must answer **all** of
    /// them: a half-filled batch is a transcript no later request can be built from, so a partial
    /// reply is refused here, before a single record is written, rather than being accepted and
    /// becoming a provider error on the next round.
    ///
    /// What is owed is read back out of the conversation, not held in memory — see
    /// [`crate::Session::owed`]. That is what lets the app answer in a second request, minutes
    /// later, or after it has been restarted.
    pub fn resume(
        &self,
        results: &[CallResult],
        cancel: &Cancellation,
    ) -> Result<TurnOutcome, AgentError> {
        self.resume_watched(results, cancel, &mut |_| {})
    }

    /// The same, with the model's text handed to `on_text` as it is written.
    pub fn resume_watched(
        &self,
        results: &[CallResult],
        cancel: &Cancellation,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<TurnOutcome, AgentError> {
        let owed = self
            .session
            .owed()
            .map_err(|err| AgentError::Session { reason: err.to_string() })?
            .ok_or_else(|| AgentError::Paused {
                reason: "no tool call is waiting for a result: the model's last request was \
                         answered, or it asked for nothing. Send the message again rather than a \
                         result."
                    .to_string(),
            })?;

        let mut supplied: BTreeMap<&str, &CallResult> = BTreeMap::new();
        for result in results {
            if supplied.insert(result.call_id.as_str(), result).is_some() {
                return Err(AgentError::Paused {
                    reason: format!("`{}` was answered twice", result.call_id),
                });
            }
        }
        let owed_ids: Vec<&str> = owed.calls.iter().map(|call| call.id.as_str()).collect();
        if let Some(missing) = owed_ids.iter().find(|id| !supplied.contains_key(*id)) {
            return Err(AgentError::Paused {
                reason: format!(
                    "the call `{missing}` is still waiting for its result; the {} answer(s) that \
                     arrived cover the rest. Nothing was written, because a batch answered in part \
                     cannot be sent to the model.",
                    results.len()
                ),
            });
        }
        if let Some(unexpected) = supplied.keys().find(|id| !owed_ids.contains(id)) {
            return Err(AgentError::Paused {
                reason: format!(
                    "nothing is waiting for a result for `{unexpected}`; the calls owed are {}",
                    owed_ids.join(", ")
                ),
            });
        }

        // Written in the order the model asked for them, not the order the answers arrived: the
        // transcript is read by a person as well as by the model, and a batch whose rows disagree
        // with the request above them is harder to follow for no gain.
        for call in &owed.calls {
            let result = supplied[call.id.as_str()];
            let content = if result.content.trim().is_empty() {
                // A provider refuses an empty tool message, and an app whose tool means "done,
                // nothing to say" should not have its silence read as a broken request.
                "(the call returned no text)".to_string()
            } else {
                result.content.clone()
            };
            let outcome = if result.is_error { "failed" } else { "ok" };
            self.append_result(&Message::tool(call.id.clone(), content), outcome)?;
        }

        self.drive(cancel, on_text, owed.rounds)
    }

    /// The round loop, from the file as it stands.
    ///
    /// `rounds_before` is what a caller continuing a handed-back turn has already spent. The budget
    /// belongs to the turn, and a turn that restarted its count every time it was handed back would
    /// be a turn that runs for as long as the app keeps answering.
    fn drive(
        &self,
        cancel: &Cancellation,
        on_text: &mut dyn FnMut(&str),
        rounds_before: usize,
    ) -> Result<TurnOutcome, AgentError> {
        let definitions = self.definitions();
        let mut budget = self.limits.max_history_chars;
        let mut shrinks = 0usize;
        // One compaction attempt per turn that fails. A summariser that is unreachable is
        // unreachable on every round of this turn too, and re-asking it 540 times spends the
        // turn's budget learning the same thing.
        let mut compaction_failed = false;
        let mut last_text = String::new();
        // What this turn has cost so far. Summed here rather than reported per round, because what
        // a caller pays for is the turn: every round sends the conversation again, so the last
        // round's count is the smallest part of the bill and the most misleading one to hand back.
        let mut usage = Usage::default();

        for round in rounds_before..self.limits.max_rounds {
            if cancel.is_cancelled() {
                return Ok(self.cancel_note(round, usage));
            }
            let (messages, dropped) = self.request_messages(budget)?;
            // The moment a request has to leave records out is the moment this exists for: what
            // does not fit can be summarised rather than never mentioned. `fit_tail` is the
            // measurement — the same characters, the same budget the request is about to be sent
            // under — so a folding and a trimming can never disagree about what fits.
            let messages = if dropped > 0 && !compaction_failed {
                match self.compact(budget)? {
                    Compacted::Rewritten(spent) => {
                        // Counted in the cost and not in the rounds, which is the distinction #105
                        // drew and this keeps: the summary is not a step of the work, so the caller
                        // is not asked to think of it as one — but it is a model call this turn
                        // made, so it is money this turn spent, and leaving it out of `usage`
                        // would put the one number a person reads below what they were billed.
                        usage = usage.plus(spent);
                        self.request_messages(budget)?.0
                    }
                    Compacted::Skipped => messages,
                    Compacted::Failed(spent) => {
                        usage = usage.plus(spent);
                        compaction_failed = true;
                        messages
                    }
                }
            } else {
                messages
            };
            // What was sent is recorded before the answer is acted on: a request that is then
            // answered with a failure is exactly the one somebody comes to this file to read.
            self.record_request(&messages, &definitions)?;
            // The stop flag travels *into* the call, not just around it. This is the one wait in a
            // turn long enough that reading the flag only between calls reads as a button that
            // does nothing: a model call is tens of seconds, and a person who stopped the work
            // must be free to ask something else in the meantime.
            let stop = {
                let cancel = cancel.clone();
                Arc::new(move || cancel.is_cancelled()) as Arc<dyn Fn() -> bool + Send + Sync>
            };
            let turn = match self
                .model
                .turn_until(&messages, &definitions, &mut *on_text, &stop)
            {
                Ok(turn) => {
                    self.record_answer(&turn)?;
                    turn
                }
                Err(error) => {
                    // A call the person abandoned is not a failure to report as one. The loop ends
                    // the turn as cancelled, which is the same ending a stop between calls gets, so
                    // the page says "stopped at your request" rather than naming a broken endpoint.
                    if error.is_stopped() {
                        self.record_failure(&error)?;
                        return Ok(self.cancel_note(round, usage));
                    }
                    self.record_failure(&error)?;
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

            // Counted before the answer is acted on and before the refusal check below: either
            // way the request was made and the provider billed for it, so a turn that is cut short
            // reports its cost rather than losing it. A round that failed outright reaches neither
            // (it `continue`s or returns inside the match above) and reported no usage to count.
            usage = usage.plus(turn.usage);

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
                    usage,
                });
            }

            // The round's work, split by where it can run. Everything this process can do, it does
            // now: the round is already paid for, and holding a local call back until after a
            // handoff would make it wait on a process with no part in it. Everything it cannot do
            // is collected and handed back together — one round, one handoff.
            let mut pending: Vec<PendingCall> = Vec::new();
            for call in &turn.tool_calls {
                // The context every call of this turn runs under. Built once per turn so that the
                // borrowed path inside `dispatch_in` still borrows, and so that a call which
                // escalates inherits the same stop signal for free.
                let call_ctx = self.ctx.clone().with_stop({
                    let cancel = cancel.clone();
                    move || cancel.is_cancelled()
                });
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
                        usage,
                    });
                }
                // Whether this call is this process's work. A call naming no tool of this session
                // is **not** handed back: `dispatch_in` answers it with the list of what exists,
                // which is a sentence the app cannot write and the model needs.
                let declared = self
                    .tools
                    .iter()
                    .find(|tool| tool.name() == call.function.name);
                if self.handoff && declared.is_some_and(|tool| !tool.runs_here()) {
                    match handoff(call) {
                        Ok(arguments) => pending.push(PendingCall {
                            call_id: call.id.clone(),
                            name: call.function.name.clone(),
                            arguments,
                        }),
                        // Arguments no server could dispatch cannot be run by anybody, so handing
                        // them back would be a round trip that ends in the same refusal.
                        Err(reason) => {
                            let result = ToolResult::error(call.function.name.clone(), reason);
                            let outcome = result.outcome();
                            self.append_result(
                                &Message::tool(call.id.clone(), result.content),
                                outcome,
                            )?;
                        }
                    }
                    continue;
                }
                let result = self.dispatch_in(call, &call_ctx);
                // Read the ending before the content is moved into the message: both come from
                // the same result, and only one of them is for the model.
                let outcome = result.outcome();
                self.append_result(&Message::tool(call.id.clone(), result.content), outcome)?;
                // A call that was abandoned reports it, and the turn ends here rather than at the
                // top of the next one: the checkpoint before the next round would work too, and
                // it would ask the model for another request first — the one thing a stop exists
                // to prevent. The tool that ran already wrote its own ending to the record, so
                // the file keeps the whole of what happened.
                if cancel.is_cancelled() {
                    self.note(
                        "the turn was stopped while a tool was running; the tool was abandoned \
                         and no further call was made",
                    )?;
                    return Ok(TurnOutcome::Cancelled {
                        note: "the turn was stopped; the running tool was abandoned".to_string(),
                        usage,
                    });
                }
            }

            if !pending.is_empty() {
                // The handoff. Every call above that this process could run has run and been
                // answered; these are the ones it cannot, and they leave together. The note is
                // written first, so a file read after the app has been handed the batch shows the
                // pause before it shows anything the app did with it.
                self.note(format!(
                    "the turn is handed back: {} call(s) of this round run outside this process \
                     ({})",
                    pending.len(),
                    pending
                        .iter()
                        .map(|call| call.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))?;
                return Ok(TurnOutcome::Paused {
                    text: last_text,
                    rounds: round + 1,
                    usage,
                    pending,
                });
            }
        }

        self.note(format!(
            "the round budget of {} ran out with the model still asking for work",
            self.limits.max_rounds
        ))?;
        Ok(TurnOutcome::RoundLimit {
            text: last_text,
            rounds: self.limits.max_rounds,
            usage,
        })
    }

    /// Run one tool call, turning every failure into a result the model can read.
    ///
    /// Nothing here panics and nothing is silently dropped: a call whose arguments do not parse,
    /// or that names a tool this agent does not have, comes back as a refusal naming what is
    /// wrong and what exists. A model told only "error" will make the same call again.
    pub fn dispatch(&self, call: &deepnest_llm::ToolCall) -> ToolResult {
        self.dispatch_in(call, &self.ctx)
    }

    /// The same dispatch, under a context that carries this turn's stop signal.
    ///
    /// The context is the parameter rather than the field because the one thing a turn adds to it
    /// is something only the turn has: [`Cancellation`]. Everything else — the policy, the working
    /// directory, the scanner — is what `self.ctx` already holds, and the widening path below still
    /// builds its own rather than mutating either.
    pub fn dispatch_in(&self, call: &deepnest_llm::ToolCall, base: &ToolContext) -> ToolResult {
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
        let (arguments, wanted) = match approve::take_request(&arguments, base.policy.mode()) {
            Ok(pair) => pair,
            Err(reason) => return ToolResult::refused(name.to_string(), reason),
        };
        // The context for *this* call. Only a widening produces a second one: otherwise the agent's
        // own is borrowed, so the path every call takes allocates nothing and "no approval, no new
        // policy" is visible in the code that runs.
        let widened;
        let ctx = match &wanted {
            None => base,
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
                match Grant::approve(request.clone(), &base.policy) {
                    Ok(grant) => {
                        widened = ToolContext::new(grant.policy().clone(), base.workdir.clone())
                            .with_scan(base.scan.clone())
                            .stop_like(base);
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
                    base.policy.mode(),
                    result.content
                ),
                ..result
            },
            None => result,
        }
    }

    /// The messages for one request — the system prompt, then as much conversation as fits — and
    /// how many records the budget left out.
    ///
    /// The count is not a diagnostic: it is the signal that a request is about to be sent with a
    /// hole in its beginning, which is what [`Self::compact`] acts on.
    fn request_messages(&self, budget: usize) -> Result<(Vec<Message>, usize), AgentError> {
        let mut history = self.session.messages().map_err(|err| AgentError::Session {
            reason: err.to_string(),
        })?;
        history.insert(0, Message::system(self.system.clone()));
        // Fold before the trim, not after: a folded result costs a line instead of a page, so the
        // budget then reaches further back into the conversation instead of cutting more of it off.
        let (history, _folded) = fold_tool_results(&history, self.limits.fold_keep);
        let (mut messages, dropped) = fit_tail(&history, budget);
        // `fit_tail` keeps a leading system message that was in the input; with several it keeps
        // them all, and one prompt is what this loop sends.
        if messages.first().map(|message| message.role.as_str()) != Some("system") {
            messages.insert(0, Message::system(self.system.clone()));
        }
        Ok((messages, dropped))
    }

    /// Replace the older part of the conversation with a summary of it.
    ///
    /// Called when the next request would have to leave records out. The model is asked once, with
    /// no tools and nothing but the part of the conversation that is about to be dropped, and what
    /// it answers is written into the file as an ordinary message ahead of the messages that
    /// survive — so every later turn replays the summary from the file too, and none of this lives
    /// in memory.
    ///
    /// Three things it deliberately does not do. It does not fail the turn: a summary that cannot
    /// be written costs the summary, and the request then goes out trimmed exactly as it would have
    /// without this. It does not spend a round: `rounds` counts the caller's requests, and the
    /// summariser is a cost of staying inside the budget rather than a step of the work — though
    /// what it *spends* is returned to the caller and added to the turn's `usage`, because a model
    /// call this turn made is money this turn is billed for whether or not it advanced the work.
    /// And it drops no note: those are the agent's own record of what happened, they are never sent
    /// to the model, and the fold keeps every one of them.
    ///
    /// It also refuses a fold that cannot help. If the messages the cut would keep do not fit the
    /// budget on their own, summarising what came before them frees nothing: the request would be
    /// trimmed inside the window anyway, and the summary would be written, dropped, and written
    /// again on the next round — one model call per round, spent to make no difference. Refusing
    /// here leaves exactly the behaviour that existed before compaction, which is the honest
    /// answer for a conversation whose *recent* messages are the problem.
    fn compact(&self, budget: usize) -> Result<Compacted, AgentError> {
        let keep = self.limits.compact_keep;
        let records = self.session.records().map_err(|err| AgentError::Session {
            reason: err.to_string(),
        })?;
        let Some(at) = fold_start(&records, keep) else {
            return Ok(Compacted::Skipped);
        };
        let window: usize = records[at..]
            .iter()
            .filter(|record| record.kind == "message")
            .map(Record::chars)
            .sum();
        if window + self.system.chars().count() > budget {
            return Ok(Compacted::Skipped);
        }
        let request = vec![
            Message::system(SUMMARISER),
            Message::user(self.transcript(&records[..at])),
        ];
        self.record_request(&request, &[])?;
        let turn = match self.model.turn(&request, &[]) {
            Ok(turn) => {
                self.record_answer(&turn)?;
                turn
            }
            Err(error) => {
                self.record_failure(&error)?;
                self.note(format!(
                    "the conversation was not compacted: the summary call failed: {}",
                    error.body
                ))?;
                return Ok(Compacted::Failed(Usage::default()));
            }
        };
        let summary = turn.content.as_deref().unwrap_or("").trim();
        let spent = turn.usage;
        if summary.is_empty() {
            self.note(
                "the conversation was not compacted: the summary call answered with no text",
            )?;
            // The call was made and billed even though the answer was unusable, so its price is
            // returned rather than dropped: this turn spent that money and the person is owed it.
            return Ok(Compacted::Failed(spent));
        }
        let replaced = records[..at]
            .iter()
            .filter(|record| record.kind == "message")
            .count();
        let Some(folded) = fold(&records, keep, &format!("{SUMMARY_PREFIX}\n{summary}")) else {
            return Ok(Compacted::Skipped);
        };
        self.session
            .replace(&folded)
            .map_err(|err| AgentError::Session {
                reason: err.to_string(),
            })?;
        self.note(format!(
            "the conversation outgrew one request, so {replaced} earlier message(s) were folded \
             into a summary; the newest {keep} are still sent in full."
        ))?;
        Ok(Compacted::Rewritten(spent))
    }

    /// The records a compaction is about to drop, as text for the summariser, capped at one
    /// request's worth.
    ///
    /// The **end** of that region is what survives the cap, not its beginning: it is the part
    /// nearest the messages the model can still see, so the summary joins up with the conversation
    /// instead of describing a prehistory that nothing connects to. The summariser is told nothing
    /// about the cap, because it is summarising what it is given.
    fn transcript(&self, dropped: &[Record]) -> String {
        let cap = self.limits.max_history_chars;
        let lines: Vec<String> = dropped
            .iter()
            .filter(|record| record.kind == "message")
            .map(|record| {
                let text = record.content.as_deref().unwrap_or("");
                let calls: String = record
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|call| format!(" [called {}]", call.function.name))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                format!("{}: {text}{calls}", record.role)
            })
            .collect();
        let mut budget = cap;
        let mut kept: Vec<&str> = Vec::new();
        for line in lines.iter().rev() {
            let cost = line.chars().count() + 1;
            if cost > budget {
                break;
            }
            budget -= cost;
            kept.push(line);
        }
        kept.reverse();
        let transcript = kept.join("\n");
        if transcript.is_empty() {
            // One record can be larger than a whole request — a single `bash` output, a directory
            // listing of a whole tree. Its beginning is then what the summariser sees, because a
            // truncated account of it beats asking a model about an empty string.
            return lines
                .last()
                .map(|line| line.chars().take(cap).collect())
                .unwrap_or_default();
        }
        transcript
    }

    /// Write the request half of an exchange, when a log was asked for.
    fn record_request(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<(), AgentError> {
        let Some(log) = &self.llm_log else {
            return Ok(());
        };
        let record = crate::llmlog::Request::now(
            self.model.model_name().unwrap_or("unknown"),
            messages,
            tools,
        );
        self.note_a_log_failure(log.append(&record))
    }

    /// Write what the model answered, with the cost the provider reported for it.
    fn record_answer(&self, turn: &AssistantTurn) -> Result<(), AgentError> {
        let Some(log) = &self.llm_log else {
            return Ok(());
        };
        let record = crate::llmlog::Response::answered(turn);
        self.note_a_log_failure(log.append(&record))
    }

    /// Write a failed call, with its status and the provider's own words.
    ///
    /// Recorded even though the loop may recover from it — a shrunk request that then succeeds
    /// leaves a conversation that says nothing about the `400` that made it shrink, and that is
    /// the failure somebody reads this file to find.
    fn record_failure(&self, error: &LlmError) -> Result<(), AgentError> {
        let Some(log) = &self.llm_log else {
            return Ok(());
        };
        let record = crate::llmlog::Response::failed(error);
        self.note_a_log_failure(log.append(&record))
    }

    /// A log that cannot be written costs the record, never the turn: this file is a diagnostic,
    /// and an agent that stopped working because its log directory was full would be trading the
    /// work for the account of it. Said out loud once, in the conversation, so it is not silent.
    fn note_a_log_failure(&self, written: std::io::Result<()>) -> Result<(), AgentError> {
        if let Err(error) = written {
            self.note(format!("the model log could not be written: {error}"))?;
        }
        Ok(())
    }

    fn append(&self, message: &Message) -> Result<(), AgentError> {
        self.session
            .append_message(message)
            .map(|_| ())
            .map_err(|err| AgentError::Session {
                reason: err.to_string(),
            })
    }

    /// Write a tool result together with how its call ended.
    fn append_result(&self, message: &Message, outcome: &str) -> Result<(), AgentError> {
        self.session
            .append_result(message, outcome)
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

    fn cancel_note(&self, round: usize, usage: Usage) -> TurnOutcome {
        let note = "the turn was cancelled before it finished; nothing further was run";
        let _ = self.note(format!("{note} (round {round})"));
        TurnOutcome::Cancelled {
            note: note.to_string(),
            usage,
        }
    }
}

/// What a compaction attempt did, and what it cost.
///
/// Three answers rather than a bool, because the caller acts differently on each: a rewrite means
/// the request is rebuilt from the file, a skip means leaving records out was the answer and no
/// model call was spent finding that out, and a failure means this turn stops asking.
///
/// Both answers that made a model call carry its [`Usage`], because the summary is **money this
/// turn spent and not a round of its work** — and those two facts have to travel separately. A
/// turn that compacts is billed for a request the caller never asked for, so a report that counts
/// rounds reports the wrong number of steps, and a report that counts only rounds *misses* the
/// money. Measured on a cold-run app (eight turns building one small app, `llm.jsonl` against the
/// reply): the two summariser calls those turns made were 84,560 prompt tokens that appeared in
/// the log and in no answer the person could see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compacted {
    /// The older messages were replaced by a summary on disk, for this price.
    Rewritten(Usage),
    /// There was nothing to fold, so no model call was made.
    Skipped,
    /// The fold was attempted and did not happen, for this price when the call itself was billed.
    /// The file is untouched.
    Failed(Usage),
}

/// What the summariser is told to produce.
///
/// It is an assistant with no tools and no history: it sees one transcript and answers with a
/// summary, so everything it needs to know about the job has to be said here. What it is asked to
/// keep is what a coding turn cannot recover afterwards — the person's goal and their constraints,
/// what was changed where, and what is left — and what it is asked to drop is what made the
/// conversation too long in the first place: raw output.
const SUMMARISER: &str = "You compact the older part of a long conversation between a user and a \
coding agent, so that the agent can carry on inside a smaller context. Answer with the summary and \
nothing else: no preamble, no description of what you are doing. Keep what the agent still needs — \
what the user asked for and any decision or constraint they stated, which files were read or \
written and what changed in them, which commands were run and what they established, what was \
tried and failed and why, and what is left to do. Drop raw tool output, whole file listings and \
dead ends that led nowhere; if a dead end still matters, say in one line what not to retry. Write \
it as prose the agent can read as its own memory of the earlier work, in at most 400 words, and \
never state anything the transcript does not contain.";

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
        /// What each request offered, in the order the requests were made.
        offered: Mutex<Vec<Vec<ToolDefinition>>>,
    }

    impl ScriptedModel {
        fn new(replies: Vec<Result<AssistantTurn, LlmError>>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
                offered: Mutex::new(Vec::new()),
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
            tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.offered.lock().unwrap().push(tools.to_vec());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(answer("out of script")))
        }

        fn turn_until(
            &self,
            messages: &[Message],
            tools: &[ToolDefinition],
            _on_text: &mut dyn FnMut(&str),
            stop: &Arc<dyn Fn() -> bool + Send + Sync>,
        ) -> Result<AssistantTurn, LlmError> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.offered.lock().unwrap().push(tools.to_vec());
            // Whatever this scripted model was going to answer is beside the point: the scripted
            // reply is given only when nobody has stopped the turn, which is the one thing this
            // fake has to model about a call that is still in flight.
            if stop() {
                return Err(LlmError::stopped());
            }
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
            usage: deepnest_llm::Usage::default(),
        }
    }

    /// The same answer, with the cost a provider reports for it.
    fn answer_costing(text: &str, prompt_tokens: u64, completion_tokens: u64) -> AssistantTurn {
        AssistantTurn {
            usage: deepnest_llm::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_tokens: Some(prompt_tokens / 2),
            },
            ..answer(text)
        }
    }

    fn asks(calls: Vec<ToolCall>) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: calls,
            finish_reason: FinishReason::ToolCalls,
            usage: deepnest_llm::Usage::default(),
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

    /// A conversation already written to the file, as a previous turn would have left it.
    fn already_said(scratch: &Scratch, turns: usize) -> Session {
        let session = Session::open(scratch.base.join("history.jsonl")).unwrap();
        for index in 0..turns {
            session
                .append_message(&Message::user(format!(
                    "question {index}: {}",
                    "ask ".repeat(12)
                )))
                .unwrap();
            session
                .append_message(&Message::assistant(format!(
                    "answer {index}: {}",
                    "say ".repeat(12)
                )))
                .unwrap();
        }
        session
    }

    /// The tight limits these tests need: a budget the conversation above cannot fit in, and a
    /// window small enough that the fold has to move the cut.
    fn folding_limits() -> Limits {
        Limits {
            max_history_chars: 300,
            compact_keep: 2,
            ..Limits::default()
        }
    }

    /// A history as a turn that walked an app's directory leaves it: long tool results the model
    /// has already read, re-sent with every round that follows.
    fn already_ran(scratch: &Scratch, calls: usize) {
        let session = Session::open(scratch.base.join("history.jsonl")).unwrap();
        session
            .append_message(&Message::user("walk the app"))
            .unwrap();
        for index in 0..calls {
            let id = format!("call_{index}");
            session
                .append_message(&Message::assistant_with_tools(
                    None,
                    vec![ToolCall {
                        id: id.clone(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments: format!("{{\"command\":\"ls {index}\"}}"),
                        },
                    }],
                ))
                .unwrap();
            session
                .append_message(&Message::tool(
                    id,
                    format!("result {index}: {}", "x".repeat(2_000)),
                ))
                .unwrap();
        }
    }

    /// Every definition a request carries asks for `intent`, and asks for it in `required`.
    ///
    /// This is the test that would have caught the wiring gap: `crates/tools` injected the field in
    /// `registry::definitions` and pinned it there, and the loop was sending `tool.definition()`
    /// instead — so the sentence the dialog draws had nothing to draw, and every existing test still
    /// passed because each of them read the list the other side produced.
    #[test]
    fn the_request_asks_every_tool_for_the_intent_behind_its_call() {
        let scratch = Scratch::new("intent-in-request");
        let model = ScriptedModel::new(vec![Ok(answer("done"))]);
        let agent = scratch.agent(model.clone());

        agent.run_turn("hello", &Cancellation::new()).unwrap();

        let offered = model.offered.lock().unwrap();
        assert_eq!(offered.len(), 1, "one request was made");
        assert!(!offered[0].is_empty(), "no tools were offered at all");
        for definition in &offered[0] {
            let name = &definition.function.name;
            let parameters = &definition.function.parameters;
            assert_eq!(
                parameters["properties"]["intent"]["type"], "string",
                "{name} does not offer an intent"
            );
            let required = parameters["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} requires nothing"));
            assert!(
                required.contains(&serde_json::json!("intent")),
                "{name} offers an intent and does not ask for it: {required:?}"
            );
        }
    }

    /// What the fold is for, at the loop's own seam: the request carries the conversation, and the
    /// tool output in it costs a line instead of the page it was. The file is untouched — the next
    /// turn, and anyone reading the app afterwards, still sees what the call returned.
    #[test]
    fn a_request_folds_tool_output_the_model_has_finished_with() {
        let scratch = Scratch::new("fold-request");
        already_ran(&scratch, 8);
        let model = ScriptedModel::new(vec![Ok(answer("done"))]);
        let agent = scratch.agent(model.clone());

        let outcome = agent
            .run_turn("and now this", &Cancellation::new())
            .unwrap();
        assert_eq!(outcome.text(), "done");

        let seen = model.seen.lock().unwrap();
        let sent = &seen[0];
        let markers: Vec<&str> = sent
            .iter()
            .filter(|message| message.role == "tool")
            .map(|message| message.content.as_deref().unwrap_or(""))
            .filter(|content| content.starts_with("[bash result folded"))
            .collect();
        // Eight results, the newest six of which the default limit keeps in full.
        assert_eq!(markers.len(), 2, "{sent:?}");
        assert!(
            markers[0].contains("2010 characters withheld"),
            "{}",
            markers[0]
        );
        let whole = sent
            .iter()
            .filter(|message| {
                message.role == "tool"
                    && message
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .starts_with("result ")
            })
            .count();
        assert_eq!(whole, 6, "{sent:?}");

        // The file keeps all eight exactly as the calls returned them.
        let recorded = agent.session().messages().unwrap();
        let kept = recorded
            .iter()
            .filter(|message| {
                message.role == "tool"
                    && message
                        .content
                        .as_deref()
                        .unwrap_or("")
                        .starts_with("result ")
            })
            .count();
        assert_eq!(kept, 8);
    }

    /// What compaction is for: what does not fit is summarised instead of never being mentioned.
    #[test]
    fn a_history_that_does_not_fit_is_folded_into_a_summary() {
        let scratch = Scratch::new("compact");
        already_said(&scratch, 8);
        let model = ScriptedModel::new(vec![
            Ok(answer(
                "eight questions were asked; question 7 was the last",
            )),
            Ok(answer("done")),
        ]);
        let agent = scratch.agent(model.clone()).with_limits(folding_limits());

        let outcome = agent
            .run_turn("and now this", &Cancellation::new())
            .unwrap();

        // Two calls and not three: the summary, then the request it made room for. The summariser
        // is asked before the request goes out, not after the provider complains.
        assert_eq!(outcome.text(), "done");
        assert_eq!(model.calls(), 2);
        let seen = model.seen.lock().unwrap();
        assert!(
            seen[0][0]
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with("You compact"),
            "the first call is the summariser: {:?}",
            seen[0][0]
        );
        let sent = seen[1][1].content.as_deref().unwrap_or("");
        assert!(sent.starts_with(SUMMARY_PREFIX), "{sent}");

        let messages = agent.session().messages().unwrap();
        assert!(
            messages[0]
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with(SUMMARY_PREFIX),
            "{messages:?}"
        );
        // The newest messages are still sent as they were, the person's own words included.
        assert!(messages
            .iter()
            .any(|message| message.content.as_deref() == Some("and now this")));
        assert_eq!(messages.last().unwrap().content.as_deref(), Some("done"));

        let notes: Vec<String> = agent
            .session()
            .records()
            .unwrap()
            .into_iter()
            .filter(|record| record.kind == "note")
            .filter_map(|record| record.content)
            .collect();
        assert!(
            notes
                .iter()
                .any(|note| note.contains("were folded into a summary")),
            "{notes:?}"
        );
    }

    /// A summariser that cannot be reached costs the summary and nothing else: the request goes out
    /// trimmed, exactly as it did before compaction existed, and the file is not touched.
    #[test]
    fn a_summary_that_fails_leaves_the_history_alone_and_the_turn_still_answers() {
        let scratch = Scratch::new("compact-fail");
        let before = already_said(&scratch, 8).messages().unwrap();
        let model = ScriptedModel::new(vec![
            Err(LlmError {
                status: None,
                body: "connection refused".into(),
                kind: FailureKind::Other,
            }),
            Ok(answer("done anyway")),
        ]);
        let agent = scratch.agent(model.clone()).with_limits(folding_limits());

        let outcome = agent
            .run_turn("and now this", &Cancellation::new())
            .unwrap();

        assert_eq!(outcome.text(), "done anyway");
        // Asked once, not once per round: an unreachable summariser stays unreachable.
        assert_eq!(model.calls(), 2);
        let messages = agent.session().messages().unwrap();
        assert_eq!(messages.len(), before.len() + 2, "nothing was dropped");
        assert!(
            !messages.iter().any(|message| message
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with(SUMMARY_PREFIX)),
            "no summary was written"
        );
        let notes: Vec<String> = agent
            .session()
            .records()
            .unwrap()
            .into_iter()
            .filter(|record| record.kind == "note")
            .filter_map(|record| record.content)
            .collect();
        assert!(
            notes.iter().any(|note| note.contains("was not compacted")),
            "{notes:?}"
        );
    }

    /// Compaction where it can help, and nowhere else. When the messages the cut would keep are
    /// themselves too big for the request, summarising what came before them frees nothing — the
    /// summary would be written, dropped, and written again next round, one model call per round to
    /// make no difference.
    #[test]
    fn a_window_too_big_to_help_is_not_summarised() {
        let scratch = Scratch::new("compact-window");
        let session = already_said(&scratch, 3);
        for index in 0..2 {
            session
                .append_message(&Message::user(format!(
                    "a long one {index} {}",
                    "x".repeat(600)
                )))
                .unwrap();
        }
        let model = ScriptedModel::new(vec![Ok(answer("done"))]);
        let agent = scratch.agent(model.clone()).with_limits(folding_limits());

        let outcome = agent.run_turn("and this", &Cancellation::new()).unwrap();

        assert_eq!(outcome.text(), "done");
        assert_eq!(model.calls(), 1, "the summariser was not asked");
        let messages = agent.session().messages().unwrap();
        assert!(
            !messages.iter().any(|message| message
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with(SUMMARY_PREFIX)),
            "nothing was folded"
        );
    }

    /// A note is the agent's own record of what happened, never sent to the model — so a fold that
    /// size was the reason for keeping it out of the request has no reason to delete it.
    #[test]
    fn a_fold_keeps_the_notes_that_explain_the_conversation() {
        let scratch = Scratch::new("compact-notes");
        let session = already_said(&scratch, 8);
        session
            .append(&Record::note("the host refused a widening to read-only"))
            .unwrap();
        let model = ScriptedModel::new(vec![Ok(answer("summed up")), Ok(answer("done"))]);
        let agent = scratch.agent(model).with_limits(folding_limits());

        agent.run_turn("carry on", &Cancellation::new()).unwrap();

        let records = agent.session().records().unwrap();
        assert!(
            records.iter().any(|record| record.kind == "note"
                && record
                    .content
                    .as_deref()
                    .unwrap_or("")
                    .contains("refused a widening")),
            "{records:?}"
        );
        assert!(
            records.iter().any(|record| record.kind == "message"
                && record.role == "user"
                && record
                    .content
                    .as_deref()
                    .unwrap_or("")
                    .starts_with(SUMMARY_PREFIX)),
            "the summary is in the file as an ordinary message: {records:?}"
        );
    }

    /// What a turn sent and what it cost, in the app's own file.
    ///
    /// The conversation says what was said; only this file says what each request cost, which is
    /// the one number a token bill cannot be reconstructed from afterwards.
    #[test]
    fn a_turn_records_every_request_and_what_it_cost() {
        let scratch = Scratch::new("llm-log");
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call("read", "{}")])),
            Ok(answer_costing("done", 900, 30)),
        ]);
        let log = scratch.base.join("llm.jsonl");
        let agent = scratch.agent(model).with_llm_log(&log);

        let outcome = agent.run_turn("do it", &Cancellation::new()).unwrap();
        assert_eq!(outcome.text(), "done");

        let text = std::fs::read_to_string(&log).expect("the log was written");
        let records: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        // Two rounds: the model asked for a tool, then answered. Four records, in order.
        assert_eq!(records.len(), 4, "{text}");
        assert_eq!(records[0]["type"], "request");
        assert_eq!(records[1]["type"], "response");
        assert_eq!(
            records[1]["finish_reason"], "tool_calls",
            "the provider's own word for why it stopped: {text}"
        );
        assert_eq!(records[2]["type"], "request");
        assert_eq!(records[3]["type"], "response");
        assert_eq!(records[3]["content"], "done");
        assert_eq!(records[3]["finish_reason"], "stop");
        assert_eq!(
            records[3]["usage"]["prompt_tokens"], 900,
            "the cost the provider reported is what makes this file worth keeping: {text}"
        );
        assert_eq!(
            records[3]["usage"]["cached_tokens"], 450,
            "and which part of it was served from a cache: {text}"
        );

        // The request record is the whole body, because "what did we actually send" is the
        // question a wrong answer sends a reader back to.
        let first = &records[0];
        assert_eq!(first["messages"][0]["role"], "system");
        assert!(
            first["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|m| m["content"] == "do it")),
            "the words the person typed are in it: {text}"
        );
        assert!(
            first["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty()),
            "and the tool set the turn could actually dispatch: {text}"
        );
    }

    /// A failed call is recorded too, even though the loop may recover from it: a shrunk request
    /// that then succeeds leaves a conversation that says nothing about the `400` behind it.
    #[test]
    fn a_model_call_that_failed_is_also_recorded() {
        let scratch = Scratch::new("llm-log-failure");
        let model = ScriptedModel::new(vec![Err(LlmError::classify(
            Some(400),
            "context length exceeded",
        ))]);
        let log = scratch.base.join("llm.jsonl");
        let agent = scratch.agent(model).with_llm_log(&log);

        // The loop shrinks and asks again with the same script, so the second call fails too and
        // the turn ends as an error. What is under test is the record, not the ending.
        let _ = agent.run_turn("do it", &Cancellation::new());

        let text = std::fs::read_to_string(&log).expect("the log was written");
        let records: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let failures: Vec<&serde_json::Value> = records
            .iter()
            .filter(|record| record["type"] == "response")
            .collect();
        assert!(!failures.is_empty(), "{text}");
        assert_eq!(failures[0]["http_status"], 400, "{text}");
        assert_eq!(failures[0]["error"], "context length exceeded", "{text}");
        assert!(
            failures[0].get("content").is_none(),
            "a failure answers nothing and must not claim it did: {text}"
        );
    }

    /// No log asked for, no file written: a turn in a test writes where it was asked to and
    /// nowhere else.
    #[test]
    fn a_turn_without_a_log_writes_none() {
        let scratch = Scratch::new("llm-log-absent");
        let model = ScriptedModel::new(vec![Ok(answer("hi"))]);
        let agent = scratch.agent(model);
        agent.run_turn("do it", &Cancellation::new()).unwrap();
        assert!(
            !scratch.base.join("llm.jsonl").exists(),
            "a log nobody asked for is not created"
        );
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

    /// A model that reports its answer while writing it, the way a streamed endpoint does.
    struct DictatingModel {
        pieces: Vec<&'static str>,
    }

    impl Model for DictatingModel {
        fn turn(
            &self,
            messages: &[Message],
            tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            self.turn_with(messages, tools, &mut |_| {})
        }

        fn turn_with(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            on_text: &mut dyn FnMut(&str),
        ) -> Result<AssistantTurn, LlmError> {
            for piece in &self.pieces {
                on_text(piece);
            }
            Ok(answer(&self.pieces.concat()))
        }
    }

    /// A watched turn is the same turn, with its text handed over in the pieces the model wrote
    /// instead of in one lump at the end.
    #[test]
    fn a_watched_turn_hands_the_text_over_while_the_model_is_still_writing() {
        let scratch = Scratch::new("watched");
        let model: Arc<dyn Model> = Arc::new(DictatingModel {
            pieces: vec!["here ", "is ", "your app"],
        });
        let agent = scratch.agent(model);

        let mut seen: Vec<String> = Vec::new();
        let outcome = agent
            .run_turn_watched(
                "build me a note app",
                &[],
                &Cancellation::new(),
                &mut |text| seen.push(text.to_string()),
            )
            .unwrap();

        assert_eq!(seen, vec!["here ", "is ", "your app"]);
        assert_eq!(outcome.text(), "here is your app");
    }

    /// A model that cannot report early is not made to pretend it can: the trait's default leaves
    /// the observer alone, and the turn is the turn it always was.
    #[test]
    fn a_watched_turn_through_a_model_that_stays_quiet_is_the_plain_turn() {
        let scratch = Scratch::new("quiet");
        let agent = scratch.agent(ScriptedModel::new(vec![Ok(answer("one whole answer"))]));

        let mut seen: Vec<String> = Vec::new();
        let outcome = agent
            .run_turn_watched("hello", &[], &Cancellation::new(), &mut |text| {
                seen.push(text.to_string())
            })
            .unwrap();

        assert!(
            seen.is_empty(),
            "a model with nothing to say early reported {seen:?}"
        );
        assert_eq!(outcome.text(), "one whole answer");
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
                rounds: 1,
                usage: Usage::default()
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
            fn name(&self) -> &str {
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

    /// A tool that was running when the stop arrived ends the turn there.
    ///
    /// The checkpoint before the next round already stopped the *next* request, and what it left
    /// behind was the thing a person pressed stop to end: the call in flight. So the flag is
    /// handed to the tool, the tool abandons its work — [`deepnest_tools::shell`] kills the child —
    /// and the loop reads the ending it comes back with. Two things are asserted, because the
    /// difference between them is the whole point: the turn ends as cancelled, and the model is
    /// never asked again.
    #[test]
    fn a_stop_while_a_tool_is_running_ends_the_turn_without_another_request() {
        struct StopsMidCall {
            cancel: Cancellation,
            saw_signal: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl Tool for StopsMidCall {
            fn name(&self) -> &str {
                "read"
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::function("read", "reads", serde_json::json!({"type": "object"}))
            }
            fn call(&self, _args: &Value, ctx: &ToolContext) -> ToolResult {
                // What a real tool does: ask the context, from inside the call, whether the turn
                // it belongs to has been told to stop — and find out that the answer is yes.
                self.saw_signal
                    .store(ctx.stop.is_some(), std::sync::atomic::Ordering::SeqCst);
                self.cancel.cancel();
                assert!(ctx.stopped(), "the context carries the turn\'s own answer");
                ToolResult::error("read", "abandoned while running")
            }
        }
        let scratch = Scratch::new("cancel-running-tool");
        let cancel = Cancellation::new();
        let saw = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call("read", "{}")])),
            Ok(answer("should never be asked for")),
        ]);
        let agent = scratch.agent_with_tools(
            model.clone(),
            vec![Box::new(StopsMidCall {
                cancel: cancel.clone(),
                saw_signal: saw.clone(),
            })],
        );
        let outcome = agent.run_turn("read one thing", &cancel).unwrap();
        assert!(
            saw.load(std::sync::atomic::Ordering::SeqCst),
            "the tool was given a stop handle, not an empty context"
        );
        assert!(
            matches!(outcome, TurnOutcome::Cancelled { .. }),
            "{outcome:?}"
        );
        assert_eq!(model.calls(), 1, "the stop saved the second request");
        let records = agent.session().records().unwrap();
        assert!(
            records.iter().any(|record| record.role == "agent"
                && record
                    .content
                    .as_deref()
                    .is_some_and(|text| text.contains("abandoned"))),
            "the turn's own note says the tool was abandoned"
        );
    }

    /// The other half of the stop, and the one the person actually feels.
    ///
    /// The test above stops a turn between calls. This one stops it *inside* one — the case that
    /// used to make `Stop` look broken, because the flag was only read at round boundaries and a
    /// model call is the longest stretch of a turn. The turn must end as cancelled, not as a
    /// failure: the endpoint did nothing wrong, and reporting it as broken is a sentence about
    /// something the person did not experience.
    #[test]
    fn a_stop_inside_a_model_call_ends_the_turn_as_cancelled() {
        struct StoppedMidCall {
            cancel: Cancellation,
        }
        impl Model for StoppedMidCall {
            fn turn(
                &self,
                _messages: &[Message],
                _tools: &[ToolDefinition],
            ) -> Result<AssistantTurn, LlmError> {
                panic!("the loop must ask the interruptible way, or a stop cannot reach it");
            }

            fn turn_until(
                &self,
                _messages: &[Message],
                _tools: &[ToolDefinition],
                _on_text: &mut dyn FnMut(&str),
                _stop: &Arc<dyn Fn() -> bool + Send + Sync>,
            ) -> Result<AssistantTurn, LlmError> {
                // What a stop during a streamed answer does: the call comes back abandoned.
                self.cancel.cancel();
                Err(LlmError::stopped())
            }
        }
        let scratch = Scratch::new("cancel-inside-call");
        let cancel = Cancellation::new();
        let model: Arc<dyn Model> = Arc::new(StoppedMidCall {
            cancel: cancel.clone(),
        });
        let agent = scratch.agent(model);
        let outcome = agent.run_turn("write me an app", &cancel).unwrap();
        assert!(
            matches!(outcome, TurnOutcome::Cancelled { .. }),
            "a call abandoned by the person is a turn they ended, not a broken endpoint: {outcome:?}"
        );
        let records = agent.session().records().unwrap();
        assert!(
            records.iter().any(|record| record.role == "agent"
                && record
                    .content
                    .as_deref()
                    .is_some_and(|text| text.contains("cancel"))),
            "the record says the turn was cancelled"
        );
    }

    /// The cost of a turn is the sum of its rounds, not the last round's number.
    ///
    /// A turn here is not one request: every round sends the conversation again, and on a long turn
    /// that re-sending is most of the bill. Reported per round, the last round of a two-round turn
    /// names the smaller half of what the turn cost — and on the 313-round turn this project
    /// measured in its own history it would have named a third of one percent of it.
    #[test]
    fn the_turn_reports_the_cost_of_every_round_added_up() {
        let scratch = Scratch::new("usage-sum");
        // Two rounds costing two different amounts, so a sum and a last-round reading are two
        // different numbers rather than one number spelled twice.
        let mut asking = asks(vec![call("read", "{}")]);
        asking.usage = deepnest_llm::Usage {
            prompt_tokens: 900,
            completion_tokens: 30,
            total_tokens: 930,
            cached_tokens: Some(450),
        };
        let model = ScriptedModel::new(vec![Ok(asking), Ok(answer_costing("done", 1_200, 40))]);
        let agent = scratch.agent(model);

        let outcome = agent.run_turn("do it", &Cancellation::new()).unwrap();
        assert_eq!(outcome.text(), "done");
        let usage = outcome.usage();
        assert_eq!(usage.prompt_tokens, 2_100, "900 + 1,200");
        assert_eq!(usage.completion_tokens, 70, "30 + 40");
        assert_eq!(usage.total_tokens, 2_170, "930 + 1,240");
        // Half of each round's prompt was served from the provider's cache, and adding them keeps
        // both: 450 + 600.
        assert_eq!(usage.cached_tokens, Some(1_050));
    }

    /// A summary is not a round, and it is still money.
    ///
    /// The distinction #105 drew and this keeps: `rounds` counts the caller's requests, so the
    /// summariser is not one of them — the person asked for one thing and the turn did one thing.
    /// But the summariser is a model call this turn made, so its tokens are this turn's bill, and a
    /// report that left them out would put the number a person reads *below* what the provider
    /// charged. Measured on a cold-run app (eight turns building one small app), the turns that
    /// compacted spent 84,560 prompt tokens that the answer never mentioned.
    #[test]
    fn a_summary_costs_money_without_being_a_round() {
        let scratch = Scratch::new("compact-cost");
        already_said(&scratch, 8);
        let model = ScriptedModel::new(vec![
            Ok(answer_costing(
                "eight questions were asked; question 7 was the last",
                5_000,
                200,
            )),
            Ok(answer_costing("done", 1_200, 40)),
        ]);
        let agent = scratch.agent(model.clone()).with_limits(folding_limits());

        let outcome = agent
            .run_turn("and now this", &Cancellation::new())
            .unwrap();

        // One round: the caller asked once, and the summariser did not turn that into two steps.
        match &outcome {
            TurnOutcome::Answered { rounds, .. } => {
                assert_eq!(*rounds, 1, "the summary is not a round of the work")
            }
            other => panic!("expected an answer, got {other:?}"),
        }
        // Both calls are in the bill, and the ticket is larger than the answer's own request.
        let usage = outcome.usage();
        assert_eq!(usage.prompt_tokens, 6_200, "5,000 summary + 1,200 round");
        assert_eq!(usage.completion_tokens, 240, "200 summary + 40 round");
        assert_eq!(usage.total_tokens, 6_440);
        // And the log agrees: every request this turn made is in it, so the reply and the file
        // that an outside reader reconstructs the cost from cannot disagree.
        assert_eq!(model.calls(), 2);
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
                rounds: 2,
                usage: Usage::default()
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
            usage: deepnest_llm::Usage::default(),
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
        fn name(&self) -> &str {
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

    /// The floor the budget must not fall back under.
    ///
    /// 270 is the long-task budget the sibling agent settled on after 30 rounds kept truncating
    /// real work, and it is the reference number for "enough to finish". Pinned as a floor rather
    /// than as equality: raising the budget is a tuning decision, and lowering it is the regression
    /// this test exists for — what shipped before was 24, which could not finish a turn.
    #[test]
    fn the_default_round_budget_stays_above_the_floor_a_long_task_needed() {
        let limits = Limits::default();
        // 313 is measured, not chosen: it is the one turn in `apps/feishu/.agent/history.jsonl`
        // that ended on its own rather than being cut off, out of nine. A budget under it would
        // truncate the only kind of turn that has been observed to finish, which is the failure
        // this ceiling exists to avoid — the ceiling bounds spend, it does not decide how much
        // work fits.
        assert!(
            limits.max_rounds >= 313,
            "the round budget fell to {}, under the 313 a real turn needed",
            limits.max_rounds
        );
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

    /// A tool whose work belongs to another process, as an app's MCP tools do.
    struct Remote {
        name: &'static str,
        ran: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Remote {
        fn new(name: &'static str) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
            let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Self {
                    name,
                    ran: Arc::clone(&ran),
                },
                ran,
            )
        }
    }

    impl Tool for Remote {
        fn name(&self) -> &str {
            self.name
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::function(
                self.name.to_string(),
                "remote work".to_string(),
                serde_json::json!({"type": "object"}),
            )
        }
        fn call(&self, _args: &Value, _ctx: &ToolContext) -> ToolResult {
            self.ran.fetch_add(1, Ordering::SeqCst);
            ToolResult::ok(self.name, "ran locally, which the turn must not do")
        }
        fn runs_here(&self) -> bool {
            false
        }
    }

    /// The whole point: a call that does not run here ends the request instead of nesting a
    /// callback inside it, and comes back as a list the caller can run.
    #[test]
    fn a_batch_of_remote_calls_ends_the_turn_instead_of_calling_back() {
        let scratch = Scratch::new("pause");
        let (todos, ran) = Remote::new("todos_list");
        let (done, _) = Remote::new("todos_done");
        let model = ScriptedModel::new(vec![Ok(asks(vec![
            call("todos_list", r#"{"intent":"see what is open","state":"open"}"#),
            call("todos_done", r#"{"id":3}"#),
        ]))]);
        let agent = scratch
            .agent_with_tools(
                model.clone(),
                vec![Box::new(todos) as Box<dyn Tool>, Box::new(done)],
            )
            .with_handoff(true);

        let outcome = agent.run_turn("what is left?", &Cancellation::new()).unwrap();
        let TurnOutcome::Paused {
            pending, rounds, ..
        } = &outcome
        else {
            panic!("a remote batch did not pause: {outcome:?}");
        };
        // One round, one handoff: both calls leave together.
        assert_eq!(*rounds, 1);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].name, "todos_list");
        assert_eq!(pending[1].name, "todos_done");
        // The model's own arguments cross, minus the field this workspace adds to every schema
        // and no app's tool ever declared.
        let arguments: Value = serde_json::from_str(&pending[0].arguments).unwrap();
        assert_eq!(arguments["state"], "open");
        assert!(arguments.get("intent").is_none(), "{arguments}");
        // Nothing called the app: that is the deadlock this shape exists to avoid.
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        // And the model was not asked again — a paused turn has no round after the handoff.
        assert_eq!(model.calls(), 1);
    }

    /// The other half of the same decision, and the default: a turn nobody is blocking for calls a
    /// remote tool here and now, because there is no request on the other side to deadlock with.
    #[test]
    fn a_turn_that_does_not_hand_back_calls_the_remote_tool_where_it_stands() {
        let scratch = Scratch::new("inline");
        let (todos, ran) = Remote::new("todos_list");
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call("todos_list", "{}")])),
            Ok(answer("done")),
        ]);
        let agent = scratch.agent_with_tools(model.clone(), vec![Box::new(todos) as Box<dyn Tool>]);

        let outcome = agent.run_turn("list", &Cancellation::new()).unwrap();
        assert!(
            matches!(outcome, TurnOutcome::Answered { rounds: 2, .. }),
            "{outcome:?}"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the tool was not called");
        assert!(agent.session().owed().unwrap().is_none());
    }

    /// A round's local work is done before the handoff: holding it back would make it wait on a
    /// process that has no part in it, and the round is already paid for.
    #[test]
    fn the_local_calls_of_a_round_run_before_the_batch_is_handed_back() {
        let scratch = Scratch::new("mixed");
        std::fs::write(scratch.base.join("a.txt"), "the local answer").unwrap();
        let (remote, ran) = Remote::new("todos_list");
        let model = ScriptedModel::new(vec![Ok(asks(vec![
            call("read", r#"{"file_path":"a.txt"}"#),
            call("todos_list", "{}"),
        ]))]);
        let mut tools = deepnest_tools::registry::all();
        tools.push(Box::new(remote) as Box<dyn Tool>);
        let agent = scratch.agent_with_tools(model, tools).with_handoff(true);

        let outcome = agent.run_turn("read and list", &Cancellation::new()).unwrap();
        assert!(matches!(outcome, TurnOutcome::Paused { .. }), "{outcome:?}");
        assert_eq!(ran.load(Ordering::SeqCst), 0);

        let messages = agent.session().messages().unwrap();
        let local = messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("call_read"))
            .expect("the local call was answered before the handoff");
        assert!(local.content.as_deref().unwrap().contains("the local answer"));
        assert!(
            !messages
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some("call_todos_list")),
            "a handed-back call must not have an answer yet"
        );
    }

    /// The answers come back in a second request, and the turn continues where it stopped.
    #[test]
    fn the_results_continue_the_same_turn_and_the_round_count_carries_on() {
        let scratch = Scratch::new("resume");
        let (todos, _) = Remote::new("todos_list");
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call("todos_list", "{}")])),
            Ok(answer("three things are open")),
        ]);
        let agent = scratch
            .agent_with_tools(model.clone(), vec![Box::new(todos) as Box<dyn Tool>])
            .with_handoff(true);
        agent
            .run_turn("what is open?", &Cancellation::new())
            .unwrap();

        let outcome = agent
            .resume(
                &[CallResult {
                    call_id: "call_todos_list".into(),
                    content: "3 open".into(),
                    is_error: false,
                }],
                &Cancellation::new(),
            )
            .unwrap();

        // The second round of the same turn, not the first round of a new one.
        assert!(
            matches!(outcome, TurnOutcome::Answered { ref text, rounds: 2, .. } if text == "three things are open"),
            "{outcome:?}"
        );
        // The model's second request had the answer in it, in the right place.
        let seen = model.seen.lock().unwrap();
        let second = seen.last().unwrap();
        let answer_row = second
            .iter()
            .rposition(|message| message.tool_call_id.as_deref() == Some("call_todos_list"))
            .expect("the result is in the second request");
        assert!(second[answer_row].content.as_deref().unwrap().contains("3 open"));
        assert_eq!(second[answer_row].role, "tool");
    }

    /// A batch answered in part is refused before a record is written. Accepting it would leave a
    /// transcript no later request can be built from, and the provider's complaint would name a
    /// call the app thought it had answered.
    #[test]
    fn a_partial_answer_continues_nothing_and_writes_nothing() {
        let scratch = Scratch::new("partial");
        let (todos, _) = Remote::new("todos_list");
        let (done, _) = Remote::new("todos_done");
        let model = ScriptedModel::new(vec![Ok(asks(vec![
            call("todos_list", "{}"),
            call("todos_done", "{}"),
        ]))]);
        let agent = scratch
            .agent_with_tools(model, vec![Box::new(todos) as Box<dyn Tool>, Box::new(done)])
            .with_handoff(true);
        agent.run_turn("do both", &Cancellation::new()).unwrap();
        let before = agent.session().records().unwrap().len();

        let error = agent
            .resume(
                &[CallResult {
                    call_id: "call_todos_list".into(),
                    content: "3 open".into(),
                    is_error: false,
                }],
                &Cancellation::new(),
            )
            .unwrap_err();
        assert!(matches!(error, AgentError::Paused { .. }), "{error:?}");
        assert!(
            error.to_string().contains("call_todos_done"),
            "the refusal must name the call still owed: {error}"
        );
        assert_eq!(
            agent.session().records().unwrap().len(),
            before,
            "a refused resume wrote to the conversation"
        );
    }

    /// What is owed survives the process: it is in the file, not in this turn's memory.
    #[test]
    fn what_a_paused_turn_owes_is_read_back_out_of_the_conversation() {
        let scratch = Scratch::new("owed");
        let (todos, _) = Remote::new("todos_list");
        let model = ScriptedModel::new(vec![Ok(asks(vec![call("todos_list", "{}")]))]);
        let agent = scratch
            .agent_with_tools(model, vec![Box::new(todos) as Box<dyn Tool>])
            .with_handoff(true);
        agent.run_turn("what is open?", &Cancellation::new()).unwrap();

        // A different handle on the same file, as the next request would open it.
        let reopened = Session::open(scratch.base.join("history.jsonl")).unwrap();
        let owed = reopened.owed().unwrap().expect("the pause is in the file");
        assert_eq!(owed.calls.len(), 1);
        assert_eq!(owed.calls[0].function.name, "todos_list");
        assert_eq!(owed.rounds, 1);

        // And once answered, nothing is owed.
        agent
            .resume(
                &[CallResult {
                    call_id: "call_todos_list".into(),
                    content: "nothing".into(),
                    is_error: false,
                }],
                &Cancellation::new(),
            )
            .unwrap();
        assert!(reopened.owed().unwrap().is_none());
    }

    /// An abandoned handoff must not poison the conversation. A provider refuses a request whose
    /// assistant calls have no answers, so the next turn closes them and says so.
    #[test]
    fn a_new_turn_closes_a_handoff_nobody_answered() {
        let scratch = Scratch::new("abandoned");
        let (todos, _) = Remote::new("todos_list");
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call("todos_list", "{}")])),
            Ok(answer("starting over")),
        ]);
        let agent = scratch
            .agent_with_tools(model.clone(), vec![Box::new(todos) as Box<dyn Tool>])
            .with_handoff(true);
        agent.run_turn("what is open?", &Cancellation::new()).unwrap();

        agent.run_turn("never mind", &Cancellation::new()).unwrap();

        // Every call of the abandoned batch has an answer, and it says it did not run.
        let seen = model.seen.lock().unwrap();
        let last = seen.last().unwrap();
        let closed = last
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("call_todos_list"))
            .expect("the abandoned call was closed before the next request");
        assert!(closed.content.as_deref().unwrap().contains("did not run"));
        assert!(agent.session().owed().unwrap().is_none());
    }

    /// A kind the turn does not know, or arguments no server could dispatch, is a result rather
    /// than a round trip: the app cannot answer a question about a tool it never declared.
    #[test]
    fn arguments_no_server_could_run_are_a_result_and_not_a_handoff() {
        let scratch = Scratch::new("badargs");
        let (todos, _) = Remote::new("todos_list");
        let model = ScriptedModel::new(vec![
            Ok(asks(vec![call("todos_list", "{not json")])),
            Ok(answer("sorry")),
        ]);
        let agent = scratch
            .agent_with_tools(model, vec![Box::new(todos) as Box<dyn Tool>])
            .with_handoff(true);
        let outcome = agent.run_turn("list", &Cancellation::new()).unwrap();
        assert!(
            matches!(outcome, TurnOutcome::Answered { ref text, .. } if text == "sorry"),
            "{outcome:?}"
        );
        let messages = agent.session().messages().unwrap();
        let row = messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("call_todos_list"))
            .expect("the malformed call was answered here");
        assert!(row
            .content
            .as_deref()
            .unwrap()
            .contains("not valid JSON"));
    }
}
