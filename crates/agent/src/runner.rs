//! The `subagent` tool's other half: the child agent the tool delegates to.
//!
//! `crates/tools` owns the tool's shape — its schema, its argument readers and its refusals — and
//! takes the loop as an injected [`SubagentRunner`]. This is that runner. The split is what keeps
//! the dependency one-way: tools below, agent above.
//!
//! Three decisions are here rather than in the tool:
//!
//! - **Every child gets its own conversation file**, under `subagents/NNNN/history.jsonl`. A
//!   child that appended to the parent's file would be a second writer on one conversation, and
//!   the parent would then be reading its child's reasoning as its own.
//! - **The child inherits the policy, not a wider one.** The `ToolContext` is rebuilt from the
//!   one the tool was called with, so a subagent can do exactly what its parent could.
//! - **The timeout is enforced at round boundaries**, not mid-request. A synchronous model call
//!   cannot be preempted from here; what this can do is stop the child at the next point where it
//!   would act, which is where a runaway child actually costs something.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use deepnest_tools::subagent::{SubagentRequest, SubagentRunner};
use deepnest_tools::{Tool, ToolContext, ToolResult, MAX_RESULT_CHARS};

use crate::session::Session;
use crate::turn::{Agent, Cancellation, Limits, Model, TurnOutcome};
use crate::AgentPaths;

/// Runs each `subagent` call as a child agent with its own conversation.
pub struct LoopRunner {
    model: Arc<dyn Model>,
    /// Builds the child's tool set. A factory rather than a list because a tool may hold state
    /// that belongs to one call, and a child sharing the parent's instances is two agents in one
    /// buffer.
    tools: Arc<dyn Fn() -> Vec<Box<dyn Tool>> + Send + Sync>,
    paths: AgentPaths,
    system: String,
    limits: Limits,
    serial: AtomicU64,
}

impl LoopRunner {
    pub fn new(model: Arc<dyn Model>, paths: AgentPaths, system: impl Into<String>) -> Self {
        Self {
            model,
            tools: Arc::new(deepnest_tools::registry::all),
            paths,
            system: system.into(),
            limits: Limits::default(),
            serial: AtomicU64::new(0),
        }
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Give children a different tool set — fewer tools, or a set a test controls.
    pub fn with_tools(
        mut self,
        factory: Arc<dyn Fn() -> Vec<Box<dyn Tool>> + Send + Sync>,
    ) -> Self {
        self.tools = factory;
        self
    }

    /// The system prompt a child is given.
    ///
    /// The parent's prompt plus one paragraph, because a child that believed it was the main
    /// agent would try to talk to the user — and it has no user, only the brief it was handed.
    fn child_system(&self) -> String {
        format!(
            "{}\n\n## You are a subagent\n\n\
You were started by another agent to do one job, and you cannot see the conversation that \
produced your brief. Nothing you say reaches the user directly: your final message is the only \
thing that comes back, and it goes to the agent that asked. So state the result plainly, and say \
where you worked.\n",
            self.system
        )
    }

    /// Build the child's context from the parent's: the same policy, the same working directory,
    /// the same scanner configuration.
    fn child_context(ctx: &ToolContext) -> ToolContext {
        ToolContext::new(ctx.policy.clone(), ctx.workdir.clone()).with_scan(ctx.scan.clone())
    }
}

impl SubagentRunner for LoopRunner {
    fn run(&self, request: &SubagentRequest, ctx: &ToolContext) -> ToolResult {
        let serial = self.serial.fetch_add(1, Ordering::SeqCst);
        let dir = self.paths.subagent_dir(serial);
        let outcome = match self.run_child(request, ctx, &dir) {
            Ok(outcome) => outcome,
            Err(result) => return result,
        };
        match outcome {
            TurnOutcome::Answered { text, rounds } => ToolResult::ok(
                "subagent",
                format!(
                    "{text}\n\n[the child finished in {rounds} round(s); its conversation is at {}]",
                    dir.join("history.jsonl").display()
                ),
            )
            .truncate(MAX_RESULT_CHARS),
            TurnOutcome::RoundLimit { text, rounds } => ToolResult::error(
                "subagent",
                format!(
                    "{text}\n\n[the child hit its round budget of {rounds} and did not finish; \
                     the brief is too large for one call — split it]"
                ),
            )
            .truncate(MAX_RESULT_CHARS),
            TurnOutcome::Cancelled { note } => ToolResult::error(
                "subagent",
                format!(
                    "{note}. The child stopped at {}; nothing further ran.",
                    dir.join("history.jsonl").display()
                ),
            ),
        }
    }
}

impl LoopRunner {
    /// The child run itself, kept separate so the tool-result shaping above stays readable.
    fn run_child(
        &self,
        request: &SubagentRequest,
        ctx: &ToolContext,
        dir: &std::path::Path,
    ) -> Result<TurnOutcome, ToolResult> {
        let session = Session::open(dir.join("history.jsonl")).map_err(|err| {
            ToolResult::error(
                "subagent",
                format!("the child's conversation could not be started: {err}"),
            )
        })?;
        let agent = Agent::new(
            self.model.clone(),
            (self.tools)(),
            Self::child_context(ctx),
            session,
            self.child_system(),
        )
        .with_limits(self.limits);
        // The child's own cancellation: a deadline thread flips it, and the loop stops at the
        // next round boundary.
        let cancel = Cancellation::new();
        let deadline = cancel.clone();
        let seconds = request.timeout_seconds;
        let watchdog = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_secs(seconds) {
                if deadline.is_cancelled() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            deadline.cancel();
        });
        let outcome = agent.run_turn(&request.prompt, &cancel);
        cancel.cancel();
        let _ = watchdog.join();
        outcome.map_err(|error| ToolResult::error("subagent", error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_llm::{AssistantTurn, FinishReason, LlmError, Message, ToolDefinition};
    use deepnest_sandbox::{Mode, Policy};
    #[allow(unused_imports)]
    use deepnest_tools::Tool as _;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct FixedModel {
        reply: String,
        seen: Mutex<Vec<Vec<Message>>>,
    }

    impl Model for FixedModel {
        fn turn(
            &self,
            messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            self.seen.lock().unwrap().push(messages.to_vec());
            Ok(AssistantTurn {
                content: Some(self.reply.clone()),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
            })
        }
    }

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "deepnest-agent-runner-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(
                Policy::from_roots(
                    Mode::WorkspaceWrite,
                    self.base.clone(),
                    self.base.clone(),
                    self.base.clone(),
                ),
                self.base.clone(),
            )
        }

        fn runner(&self, model: Arc<dyn Model>) -> LoopRunner {
            LoopRunner::new(
                model,
                AgentPaths::for_app(&self.base),
                "you are the app agent",
            )
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn brief(prompt: &str) -> SubagentRequest {
        SubagentRequest {
            prompt: prompt.to_string(),
            timeout_seconds: 30,
        }
    }

    #[test]
    fn a_child_reports_its_answer_and_says_where_it_worked() {
        let scratch = Scratch::new("answer");
        let model = Arc::new(FixedModel {
            reply: "the answer is 42".into(),
            seen: Mutex::new(Vec::new()),
        });
        let runner = scratch.runner(model.clone());
        let result = runner.run(&brief("work out the answer"), &scratch.ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("the answer is 42"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("history.jsonl"),
            "{}",
            result.content
        );
    }

    #[test]
    fn the_child_gets_its_own_conversation_and_not_the_parent_s() {
        let scratch = Scratch::new("own-file");
        let model = Arc::new(FixedModel {
            reply: "done".into(),
            seen: Mutex::new(Vec::new()),
        });
        let runner = scratch.runner(model.clone());
        runner.run(&brief("first child"), &scratch.ctx());
        let second = runner.run(&brief("second child"), &scratch.ctx());
        assert!(!second.is_error, "{}", second.content);
        // Two children, two files, and the second one's request carries only its own brief.
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let second_request = &seen[1];
        let user_texts: Vec<&str> = second_request
            .iter()
            .filter(|message| message.role == "user")
            .filter_map(|message| message.content.as_deref())
            .collect();
        assert_eq!(user_texts, vec!["second child"]);
    }

    #[test]
    fn the_child_is_told_it_is_a_child() {
        let scratch = Scratch::new("child-prompt");
        let model = Arc::new(FixedModel {
            reply: "ok".into(),
            seen: Mutex::new(Vec::new()),
        });
        let runner = scratch.runner(model.clone());
        runner.run(&brief("do the job"), &scratch.ctx());
        let seen = model.seen.lock().unwrap();
        let system = seen[0][0].content.clone().unwrap();
        assert!(
            system.starts_with("you are the app agent"),
            "the parent prompt is kept"
        );
        assert!(system.contains("You are a subagent"), "{system}");
        assert!(system.contains("cannot see the conversation"), "{system}");
    }

    #[test]
    fn a_child_inherits_the_parent_s_policy_rather_than_a_wider_one() {
        let scratch = Scratch::new("inherits-policy");
        let model = Arc::new(FixedModel {
            reply: "ok".into(),
            seen: Mutex::new(Vec::new()),
        });
        let runner = scratch.runner(model);
        let ctx = ToolContext::new(
            Policy::from_roots(
                Mode::ReadOnly,
                scratch.base.clone(),
                scratch.base.clone(),
                scratch.base.clone(),
            ),
            scratch.base.clone(),
        );
        let child = LoopRunner::child_context(&ctx);
        assert_eq!(child.policy.mode(), Mode::ReadOnly);
        assert_eq!(child.workdir, ctx.workdir);
        // And a write is still refused inside the child's context.
        assert!(child
            .policy
            .write_refusal(scratch.base.join("new.txt"))
            .is_some());
        let result = runner.run(&brief("try to write"), &ctx);
        // The turn itself succeeded; what matters is that the policy was read-only.
        assert!(!result.is_error, "{}", result.content);
    }

    #[test]
    fn a_model_failure_comes_back_as_a_tool_error_and_not_a_panic() {
        struct Failing;
        impl Model for Failing {
            fn turn(
                &self,
                _messages: &[Message],
                _tools: &[ToolDefinition],
            ) -> Result<AssistantTurn, LlmError> {
                Err(LlmError {
                    status: None,
                    body: "connection refused".into(),
                    kind: deepnest_llm::FailureKind::Other,
                })
            }
        }
        let scratch = Scratch::new("failure");
        let runner = scratch.runner(Arc::new(Failing));
        let result = runner.run(&brief("do it"), &scratch.ctx());
        assert!(result.is_error);
        assert!(
            result.content.contains("connection refused"),
            "{}",
            result.content
        );
    }
}
