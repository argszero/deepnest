//! The agent's hands, and the one rule every hand obeys.
//!
//! **Every tool goes through the fence.** A file tool asks
//! [`deepnest_sandbox::Policy::write_refusal`] before it touches a path; the shell tool asks
//! [`deepnest_sandbox::SandboxBackend::confine`] for a confinement and
//! [`deepnest_command_scan::refuse`] for the things a fence cannot see. No tool decides for
//! itself what is allowed — that decision has one home per question, and a tool that
//! re-derived it would be the second answer that eventually disagrees with the first.
//!
//! Three shapes are uniform because a model reads them:
//!
//! - **[`ToolResult`]** — the text a model gets, with `is_error` saying whether the call
//!   failed. A refusal is a *result*, not a panic and not a silent no-op: the model has to be
//!   able to read why, or it will retry the same call unchanged.
//! - **[`args`]** — strict readers for a tool call's parameters. A parameter outside its domain
//!   is **refused, never rounded**: the measured failures this avoids are a negative
//!   `line_limit` slicing a file from the end, and a `"false"` string read as `true` by a
//!   truthiness test. Rust has no truthiness, but `Value::as_bool().unwrap_or(false)` is the
//!   same defect wearing a type, so the readers here refuse anything that is not the type.
//! - **[`SpecialFile`]** — a FIFO, a socket or a device node. These do not fail when opened;
//!   they **block**, so a tool that reaches `open` on one never returns. They are refused by
//!   kind, before any read or write is attempted.
//!
//! The **browser** is deliberately absent: driving a browser and taking screenshots is a skill
//! (a `SKILL.md` plus its own tooling under `~/.deepnest/skills`), not a core tool, so the core
//! only opens a page in the system browser. And `subagent` is present as a **seam** rather than a
//! leaf: its schema, its argument readers and its refusals live in [`subagent::SubagentTool`],
//! and the agent loop it delegates to is injected by `crates/agent` — which is what keeps this
//! crate underneath that one instead of beside it. `install_skill` is the same shape for the same
//! reason ([`skill::InstallSkillTool`]): the tool's words live here, and the catalog, the state
//! file and the publisher a skill install needs are injected by the binary, which is the only
//! crate that knows where the home is.

pub mod args;
pub mod files;
pub mod path;
pub mod registry;
pub mod search;
pub mod shell;
pub mod skill;
pub mod subagent;

use std::path::PathBuf;

use deepnest_llm::ToolDefinition;
use serde_json::Value;

pub use args::{boolean_argument, count_argument, optional_string_argument, string_argument};
pub use path::{resolve_target, special_file_kind, SpecialFile};
pub use shell::{BashTool, ShellOutcome, ShellResult, Spawner, StdSpawner};
pub use skill::{InstallSkillTool, SkillInstallRequest, SkillInstaller};
pub use subagent::{SubagentRequest, SubagentRunner, SubagentTool};

/// The text budget for one tool result, in characters.
///
/// A result that overflows a model's context costs the whole turn, so every tool that can
/// produce unbounded output cuts it and *says* it cut it — a silent cut reads as a short
/// output, and the model concludes the file ended there.
pub const MAX_RESULT_CHARS: usize = 30_000;

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// The tool's name, so a result pasted into a conversation says which call it answers.
    pub name: String,
    pub content: String,
    /// `true` when the call failed or was refused. A non-zero exit from a shell command is
    /// **not** an error: the command ran, and the model decides what its status means.
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            content: content.into(),
            is_error: false,
        }
    }

    /// A failure — the call did not do what it asked.
    pub fn error(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            content: content.into(),
            is_error: true,
        }
    }

    /// A refusal — the call was understood and denied. The same shape as an error to a model
    /// reading `is_error`, and a distinct constructor for the code that produces it, because
    /// "denied by policy" and "failed while trying" need different words in the text.
    pub fn refused(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::error(name, reason)
    }

    /// Cut the content to `budget` characters, saying so.
    ///
    /// The note names both numbers, so a reader can tell a short file from a cut one without
    /// knowing the budget.
    pub fn truncate(mut self, budget: usize) -> Self {
        if self.content.chars().count() <= budget {
            return self;
        }
        let kept: String = self.content.chars().take(budget).collect();
        let total = self.content.chars().count();
        self.content = format!("{kept}\n[truncated: showed {budget} of {total} characters]");
        self
    }
}

/// What a tool is given besides its arguments: the policy, the working directory, and the
/// platform seams.
///
/// One context per call rather than one per session: the policy is a value the caller resolved
/// for *this* tool call, so an escalation approved for one call cannot leak into the next.
pub struct ToolContext {
    /// The file-effect policy for this call.
    pub policy: deepnest_sandbox::Policy,
    /// Where a relative path is joined. The app directory, never the daemon's own cwd — the
    /// two differ, and the fence judges the joined path.
    pub workdir: PathBuf,
    /// The command scanner's configuration. Carried here rather than read from a global so a
    /// test can set the program names it expects.
    pub scan: deepnest_command_scan::ScanConfig,
}

impl ToolContext {
    pub fn new(policy: deepnest_sandbox::Policy, workdir: impl Into<PathBuf>) -> Self {
        Self {
            policy,
            workdir: workdir.into(),
            scan: deepnest_command_scan::ScanConfig::default(),
        }
    }

    pub fn with_scan(mut self, scan: deepnest_command_scan::ScanConfig) -> Self {
        self.scan = scan;
        self
    }
}

/// One of the agent's hands.
///
/// Synchronous and object-safe: the caller decides which runtime a tool runs on, exactly as
/// `crates/llm` leaves the choice of runtime to its adapter. A tool must not spawn a runtime of
/// its own, because a binary that ships one per tool ships the wrong number of them.
pub trait Tool: Send + Sync {
    /// The name the model calls.
    fn name(&self) -> &'static str;

    /// The definition the model is offered, parameters included.
    fn definition(&self) -> ToolDefinition;

    /// Run one call.
    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_under_the_budget_is_untouched() {
        let result = ToolResult::ok("read", "short").truncate(100);
        assert_eq!(result.content, "short");
    }

    #[test]
    fn a_cut_says_how_much_it_cut() {
        let result = ToolResult::ok("read", "abcdefghij").truncate(4);
        assert!(result.content.starts_with("abcd"), "{}", result.content);
        assert!(
            result.content.contains("showed 4 of 10"),
            "{}",
            result.content
        );
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // A byte-based cut would split a multi-byte character and produce invalid text.
        let result = ToolResult::ok("read", "日本語のテキスト").truncate(2);
        assert!(result.content.starts_with("日本"), "{}", result.content);
        assert!(
            result.content.contains("showed 2 of 8"),
            "{}",
            result.content
        );
    }

    #[test]
    fn a_refusal_is_an_error_without_being_a_panic() {
        let refused = ToolResult::refused("write", "outside the writable set");
        assert!(refused.is_error);
        assert_eq!(refused.content, "outside the writable set");
    }
}
