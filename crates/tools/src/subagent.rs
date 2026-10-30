//! `subagent` — one child agent, and the seam that keeps this crate a leaf.
//!
//! The tool's **shape** belongs here: the name, the schema, the argument readers, the wording of
//! a refusal. What it must not own is the thing it delegates to — a child session with its own
//! policy, its own conversation and its own round budget — because that is the agent loop, and
//! the agent loop lives in `crates/agent`, which depends on this crate. A tool that reached for
//! it directly would make the dependency a cycle.
//!
//! So the loop is **injected** ([`SubagentRunner`]), and this module supplies the one thing the
//! caller should not have to restate: what a subagent call *is* — a prompt, a timeout, and a
//! result rendered the same way every other tool result is.

use std::sync::Arc;

use deepnest_llm::ToolDefinition;
use serde_json::{json, Value};

use crate::args::{count_argument, string_argument};
use crate::{Tool, ToolContext, ToolResult};

/// How long a subagent may run when the caller does not say, in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 600;

/// The longest a subagent call may ask for, in seconds.
pub const MAX_TIMEOUT_SECONDS: u64 = 3_600;

/// One subagent call, already read and validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRequest {
    pub prompt: String,
    pub timeout_seconds: u64,
}

/// The agent loop, as this crate needs to see it.
///
/// Synchronous like [`Tool::call`]: the caller decides which runtime a child round runs on. The
/// implementation lives in `crates/agent`; nothing in this crate may assume it exists.
pub trait SubagentRunner: Send + Sync {
    fn run(&self, request: &SubagentRequest, ctx: &ToolContext) -> ToolResult;
}

/// Hand one self-contained job to a child agent.
pub struct SubagentTool {
    runner: Arc<dyn SubagentRunner>,
}

impl SubagentTool {
    pub fn new(runner: Arc<dyn SubagentRunner>) -> Self {
        Self { runner }
    }
}

impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "subagent",
            "Delegate one self-contained job to a child agent and wait for its answer. The child \
             has its own context and its own tools; only its final answer comes back. Use it when \
             a job is large enough that doing it here would flood this conversation, and state \
             the job completely — the child cannot see anything you do not tell it.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The complete brief: what to do, where to work, and what to report back."
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "description": format!("Seconds the child may run. Defaults to {DEFAULT_TIMEOUT_SECONDS}; at most {MAX_TIMEOUT_SECONDS}.")
                    }
                },
                "required": ["prompt"]
            }),
        )
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let prompt = match string_argument(args, &["prompt", "intent"]) {
            Ok(prompt) => prompt,
            Err(reason) => return ToolResult::refused("subagent", reason),
        };
        if prompt.trim().is_empty() {
            // An empty brief is not a smaller job: the child sees nothing and answers nothing,
            // and the round costs the parent's whole wait.
            return ToolResult::refused(
                "subagent",
                "prompt is empty; the child cannot see this conversation, so the brief has to say \
                 what to do, where, and what to report",
            );
        }
        let timeout_seconds = match count_argument(
            args,
            &["timeout", "timeout_s"],
            1,
            Some(DEFAULT_TIMEOUT_SECONDS),
        ) {
            Ok(Some(seconds)) => seconds.min(MAX_TIMEOUT_SECONDS),
            Ok(None) => DEFAULT_TIMEOUT_SECONDS,
            Err(reason) => return ToolResult::refused("subagent", reason),
        };
        let request = SubagentRequest {
            prompt,
            timeout_seconds,
        };
        self.runner.run(&request, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_sandbox::{Mode, Policy};
    use std::sync::Mutex;

    struct RecordingRunner {
        seen: Mutex<Vec<SubagentRequest>>,
    }

    impl SubagentRunner for RecordingRunner {
        fn run(&self, request: &SubagentRequest, _ctx: &ToolContext) -> ToolResult {
            self.seen.lock().unwrap().push(request.clone());
            ToolResult::ok("subagent", "the child reported back")
        }
    }

    fn ctx() -> ToolContext {
        let base =
            std::env::temp_dir().join(format!("deepnest-tools-subagent-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        ToolContext::new(
            Policy::from_roots(
                Mode::WorkspaceWrite,
                base.clone(),
                base.clone(),
                base.clone(),
            ),
            base,
        )
    }

    fn tool() -> (SubagentTool, Arc<RecordingRunner>) {
        let runner = Arc::new(RecordingRunner {
            seen: Mutex::new(Vec::new()),
        });
        (SubagentTool::new(runner.clone()), runner)
    }

    #[test]
    fn a_call_is_handed_to_the_runner_with_its_brief_and_default_timeout() {
        let (tool, runner) = tool();
        let result = tool.call(&json!({"prompt": "read the docs and report"}), &ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("reported back"),
            "{}",
            result.content
        );
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].prompt, "read the docs and report");
        assert_eq!(seen[0].timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
    }

    #[test]
    fn an_empty_brief_is_refused_before_the_runner_is_reached() {
        let (tool, runner) = tool();
        let result = tool.call(&json!({"prompt": "   "}), &ctx());
        assert!(result.is_error);
        assert!(
            result.content.contains("cannot see this conversation"),
            "{}",
            result.content
        );
        assert!(
            runner.seen.lock().unwrap().is_empty(),
            "the runner must not run"
        );
    }

    #[test]
    fn a_timeout_past_the_ceiling_is_clamped_to_it() {
        let (tool, runner) = tool();
        let result = tool.call(
            &json!({"prompt": "report", "timeout": MAX_TIMEOUT_SECONDS + 5}),
            &ctx(),
        );
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            runner.seen.lock().unwrap()[0].timeout_seconds,
            MAX_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn a_timeout_outside_its_domain_is_refused() {
        let (tool, runner) = tool();
        let result = tool.call(&json!({"prompt": "report", "timeout": 0}), &ctx());
        assert!(result.is_error);
        assert!(result.content.contains("timeout"), "{}", result.content);
        assert!(runner.seen.lock().unwrap().is_empty());
    }
}
