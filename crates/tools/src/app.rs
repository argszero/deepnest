//! `app` — the app this agent is for: what state it is in, how to start and stop it, and what it
//! said when it ran.
//!
//! The tool's **shape** belongs here and its **reach** does not, for the reason `subagent`'s and
//! `install_skill`'s do not: the supervisor that owns an app's process is `crates/apps`', the home
//! whose `logs/` holds what the app printed is the binary's, and neither is a dependency of this
//! crate. A tool that imported them would decide for every caller which deepnest home it acts on,
//! and which app.
//!
//! Two decisions are made **here**, in the shape, rather than left to the implementation:
//!
//! - **The call names no app.** It cannot: the tool is built for one turn of one app's agent, and
//!   the implementation already knows which app that is. An `app` argument would be one dialog able
//!   to stop a process it does not own, and the name it passed would be a value nothing in this
//!   crate could check against the fence.
//! - **Reading is not writing.** `status` and `logs` ask a question and are allowed in every mode;
//!   `start`, `stop` and `restart` change the machine, so a `read-only` turn is refused in the words
//!   the rest of the fence uses — it names the mode to ask for rather than only saying no.
//!
//! The design this fills in is `docs/design.md` §4, which promises the per-app agent "starting and
//! stopping the app, reading its logs". Until this module existed the set was six leaves and two
//! seams, and every one of those five actions was reachable only by reading the control token and
//! calling the panel's own routes — which is the one thing the fence exists to stop.

use std::sync::Arc;

use deepnest_llm::ToolDefinition;
use deepnest_sandbox::Mode;
use serde_json::{json, Value};

use crate::args::{count_argument, optional_string_argument, string_argument};
use crate::{Tool, ToolContext, ToolResult};

/// The name the model calls.
pub const NAME: &str = "app";

/// How many lines of a log a `logs` call reads when it does not say.
///
/// Small on purpose: the reader is an agent looking for the end of a failure, and a log that fits
/// in the window costs the turn its context before it costs anyone an answer.
pub const DEFAULT_LOG_LINES: u64 = 100;

/// The most lines one call may ask for.
///
/// A bound rather than a refusal at a higher number: a model that asked for ten thousand lines
/// worked out that it wants the whole log, and cutting it to a window it can read is a better
/// answer than refusing the call and being asked again with the same number.
pub const MAX_LOG_LINES: u64 = 5000;

/// One of the five things a call can ask about the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppAction {
    /// What is true about it now.
    Status,
    /// Start it and wait until it answers.
    Start,
    /// Stop it. Stopping one that is already stopped is not a failure.
    Stop,
    /// Stop it, then start it on a fresh port.
    Restart,
    /// The last lines of what it printed.
    Logs,
}

impl AppAction {
    /// Every action, in the spelling the model sends.
    pub const ALL: [&'static str; 5] = ["status", "start", "stop", "restart", "logs"];

    /// Read the action the model asked for, or `None` when it is not one of [`Self::ALL`].
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "status" => Some(Self::Status),
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "restart" => Some(Self::Restart),
            "logs" => Some(Self::Logs),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Logs => "logs",
        }
    }

    /// Whether this action changes the machine rather than asking about it.
    ///
    /// The one question the fence is asked about an app call, which is why it is a method here
    /// rather than a `matches!` in [`AppTool::call`]: an action added later has to answer it where
    /// the action is declared, not in a branch somebody may not find.
    pub fn changes(self) -> bool {
        matches!(self, Self::Start | Self::Stop | Self::Restart)
    }
}

/// Which log a `logs` call reads.
///
/// Two files rather than one, because the two answer different questions — "why did the build
/// fail" and "what is the app saying now" — and `crates/apps` already writes them apart. Which one
/// a reader wants is the only thing this argument decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// What the app itself printed while running.
    App,
    /// What the last build said.
    Build,
}

impl LogKind {
    /// Read the log a call asked for; absent is the running app's, which is the commoner question.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "app" => Some(Self::App),
            "build" => Some(Self::Build),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Build => "build",
        }
    }
}

/// One call, already read and defaulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRequest {
    pub action: AppAction,
    /// Read only by [`AppAction::Logs`]; carried on every request so the implementation has one
    /// value to match on rather than an `Option` it has to answer for.
    pub log: LogKind,
    /// How many lines of it. Bounded by [`MAX_LOG_LINES`], never zero.
    pub lines: u64,
}

/// Controlling the app, as this crate needs to see it.
///
/// Synchronous like [`Tool::call`], and given the whole [`ToolContext`] because the decision this
/// tool exists to make at `read-only` is a fence decision — the implementation must be able to see
/// the mode it was handed rather than one it remembers. The implementation lives in
/// `crates/deepnest`, which is the only crate that holds the supervisor and the home; nothing here
/// may assume it exists.
pub trait AppControl: Send + Sync {
    fn act(&self, request: &AppRequest, ctx: &ToolContext) -> ToolResult;
}

/// The app the turn is for, as one tool.
pub struct AppTool {
    control: Arc<dyn AppControl>,
}

impl AppTool {
    pub fn new(control: Arc<dyn AppControl>) -> Self {
        Self { control }
    }
}

impl Tool for AppTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            NAME,
            "Act on the app this conversation builds — the one whose page you are in — rather than \
             on any other: `status` says whether it is running, on which port, and why it stopped \
             if it did; `start` starts it and waits until it answers; `stop` stops it; `restart` \
             stops it and starts it again on a fresh port; `logs` reads the end of what it printed, \
             either its own output (`log: \"app\"`) or what the last build said (`log: \"build\"`). \
             Reach for this instead of running the app yourself through `bash`: the port is \
             allocated per start and never leaves this process, and a second copy started by hand \
             races the one the page is being served from. A build failure is already in front of \
             you before you write, so `logs` with `log: \"build\"` is the next question when the \
             note you were given was not enough.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": AppAction::ALL,
                        "description": "What to do: `status`, `start`, `stop`, `restart` or `logs`."
                    },
                    "log": {
                        "type": "string",
                        "enum": ["app", "build"],
                        "description": "Read by `action: \"logs\"` only: `app` for what the running app printed (the default), `build` for what the last build said."
                    },
                    "lines": {
                        "type": "integer",
                        "description": "Read by `action: \"logs\"` only: how many lines from the end of the log. Defaults to 100; a larger number is cut to 5000."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let spelled = match string_argument(args, &["action"]) {
            Ok(text) => text,
            Err(reason) => return ToolResult::refused(NAME, reason),
        };
        let Some(action) = AppAction::parse(&spelled) else {
            // The list, not just the refusal: a model that sent `"launch"` is one wrong word away
            // from the call it meant, and the words it could have sent are what it needs.
            return ToolResult::refused(
                NAME,
                format!(
                    "action must be one of {}; {spelled:?} is not one of them",
                    AppAction::ALL.join(", ")
                ),
            );
        };

        // The two log arguments are read by `logs` and by nothing else. Refused on the other
        // actions rather than ignored: a call that sent `lines` to `restart` believes restart reads
        // them, and answering `ok` would leave that belief in place for the next call.
        let reads_log_arguments = action == AppAction::Logs;

        let log = match optional_string_argument(args, &["log"]) {
            Ok(Some(text)) => match LogKind::parse(&text) {
                Some(kind) if reads_log_arguments => kind,
                Some(_) => {
                    return ToolResult::refused(
                        NAME,
                        format!("`log` is read by action \"logs\" only, not by {spelled:?}"),
                    )
                }
                None => {
                    return ToolResult::refused(
                        NAME,
                        format!("log must be \"app\" or \"build\"; {text:?} is neither"),
                    )
                }
            },
            Ok(None) => LogKind::App,
            Err(reason) => return ToolResult::refused(NAME, reason),
        };

        let lines = match count_argument(args, &["lines"], 1, Some(DEFAULT_LOG_LINES)) {
            Ok(count) => count.unwrap_or(DEFAULT_LOG_LINES),
            Err(reason) => return ToolResult::refused(NAME, reason),
        };
        if !reads_log_arguments && lines != DEFAULT_LOG_LINES {
            return ToolResult::refused(
                NAME,
                format!("`lines` is read by action \"logs\" only, not by {spelled:?}"),
            );
        }

        if action.changes() && ctx.policy.mode() == Mode::ReadOnly {
            return ToolResult::refused(
                NAME,
                format!(
                    "this call is read-only, so it cannot {spelled} the app; the same call with \
                     `request_mode` and `justification` asks the user on the page, and the app \
                     changes only if they approve"
                ),
            );
        }

        let request = AppRequest {
            action,
            log,
            lines: lines.min(MAX_LOG_LINES),
        };
        self.control.act(&request, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_sandbox::Policy;
    use std::sync::Mutex;

    struct Recording {
        seen: Mutex<Vec<AppRequest>>,
    }

    impl Recording {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<AppRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl AppControl for Recording {
        fn act(&self, request: &AppRequest, ctx: &ToolContext) -> ToolResult {
            self.seen.lock().unwrap().push(request.clone());
            ToolResult::ok(
                NAME,
                format!(
                    "{} at {}",
                    request.action.as_str(),
                    ctx.policy.mode().as_str()
                ),
            )
        }
    }

    fn ctx(mode: Mode) -> ToolContext {
        let base = std::env::temp_dir().join(format!("deepnest-tools-app-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        ToolContext::new(
            Policy::from_roots(mode, base.clone(), base.clone(), base.clone()),
            base,
        )
    }

    fn tool() -> (AppTool, Arc<Recording>) {
        let recording = Recording::new();
        (AppTool::new(recording.clone()), recording)
    }

    #[test]
    fn every_action_is_named_once_and_parses_back_to_itself() {
        // The enum in the schema is `ALL`, and the parser is a second table that has to agree with
        // it: an action offered and not parsed is a call the model is told about and that is
        // refused, which is the exact failure the registry's one-list rule exists to prevent.
        for spelled in AppAction::ALL {
            let action =
                AppAction::parse(spelled).unwrap_or_else(|| panic!("{spelled} does not parse"));
            assert_eq!(action.as_str(), spelled);
        }
        assert_eq!(AppAction::ALL.len(), 5);
    }

    #[test]
    fn an_unknown_action_is_refused_with_the_ones_that_would_have_worked() {
        let (tool, recording) = tool();
        let result = tool.call(&json!({"action": "launch"}), &ctx(Mode::WorkspaceWrite));
        assert!(result.is_error);
        assert!(result.refused, "{}", result.content);
        assert!(result.content.contains("launch"), "{}", result.content);
        for expected in AppAction::ALL {
            assert!(result.content.contains(expected), "{}", result.content);
        }
        assert!(recording.requests().is_empty(), "nothing may run");
    }

    #[test]
    fn a_call_with_no_action_is_refused_before_the_control_is_reached() {
        let (tool, recording) = tool();
        let result = tool.call(&json!({}), &ctx(Mode::WorkspaceWrite));
        assert!(result.is_error);
        assert!(result.content.contains("action"), "{}", result.content);
        assert!(recording.requests().is_empty());
    }

    #[test]
    fn reading_the_app_is_allowed_in_every_mode_and_changing_it_is_not() {
        // The shape's own rule: `status` and `logs` ask a question, and a question is not a write.
        let (tool, recording) = tool();
        for action in ["status", "logs"] {
            let result = tool.call(&json!({"action": action}), &ctx(Mode::ReadOnly));
            assert!(!result.is_error, "{action}: {}", result.content);
        }
        assert_eq!(recording.requests().len(), 2);

        for action in ["start", "stop", "restart"] {
            let result = tool.call(&json!({"action": action}), &ctx(Mode::ReadOnly));
            assert!(result.is_error, "{action} was allowed in read-only");
            assert!(result.refused, "{action}: {}", result.content);
            // The refusal names the mode to ask for, rather than only saying no: a model that
            // cannot see the next step sends the same call again unchanged.
            assert!(
                result.content.contains("request_mode"),
                "{}",
                result.content
            );
        }
        assert_eq!(
            recording.requests().len(),
            2,
            "a refused call must not reach the supervisor"
        );
        // And in the mode the dialog actually runs at, the same three calls go through.
        for action in ["start", "stop", "restart"] {
            let result = tool.call(&json!({"action": action}), &ctx(Mode::WorkspaceWrite));
            assert!(!result.is_error, "{action}: {}", result.content);
        }
        assert_eq!(recording.requests().len(), 5);
    }

    #[test]
    fn logs_default_to_the_app_log_and_a_hundred_lines() {
        let (tool, recording) = tool();
        let result = tool.call(&json!({"action": "logs"}), &ctx(Mode::WorkspaceWrite));
        assert!(!result.is_error, "{}", result.content);
        let seen = recording.requests();
        assert_eq!(seen[0].log, LogKind::App);
        assert_eq!(seen[0].lines, DEFAULT_LOG_LINES);
    }

    #[test]
    fn the_build_log_is_a_different_file_and_asking_for_it_is_one_word() {
        let (tool, recording) = tool();
        let result = tool.call(
            &json!({"action": "logs", "log": "build", "lines": 40}),
            &ctx(Mode::WorkspaceWrite),
        );
        assert!(!result.is_error, "{}", result.content);
        let seen = recording.requests();
        assert_eq!(seen[0].log, LogKind::Build);
        assert_eq!(seen[0].lines, 40);
    }

    #[test]
    fn a_log_kind_that_is_not_one_is_refused_rather_than_read_as_the_default() {
        let (tool, recording) = tool();
        let result = tool.call(
            &json!({"action": "logs", "log": "runtime"}),
            &ctx(Mode::WorkspaceWrite),
        );
        assert!(result.refused, "{}", result.content);
        assert!(result.content.contains("build"), "{}", result.content);
        assert!(recording.requests().is_empty());
    }

    #[test]
    fn the_log_arguments_are_refused_on_the_actions_that_do_not_read_them() {
        // Ignoring them would answer `ok` to a call whose author believed restart reads `lines`,
        // and that belief would survive to the next call.
        let (tool, recording) = tool();
        for args in [
            json!({"action": "restart", "lines": 20}),
            json!({"action": "start", "log": "build"}),
        ] {
            let result = tool.call(&args, &ctx(Mode::WorkspaceWrite));
            assert!(result.refused, "{args}: {}", result.content);
        }
        assert!(recording.requests().is_empty());
    }

    #[test]
    fn a_huge_line_count_is_cut_to_the_window_rather_than_refused() {
        let (tool, recording) = tool();
        let result = tool.call(
            &json!({"action": "logs", "lines": 500000}),
            &ctx(Mode::WorkspaceWrite),
        );
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(recording.requests()[0].lines, MAX_LOG_LINES);
    }

    #[test]
    fn zero_lines_is_refused_because_it_asks_for_nothing() {
        let (tool, recording) = tool();
        let result = tool.call(
            &json!({"action": "logs", "lines": 0}),
            &ctx(Mode::WorkspaceWrite),
        );
        assert!(result.refused, "{}", result.content);
        assert!(recording.requests().is_empty());
    }

    #[test]
    fn the_description_names_the_app_it_acts_on_and_the_one_it_does_not() {
        // The tool takes no app argument, so the sentence in front of the model is the only place
        // that says which app it reaches. A model that read it as `bash` with extra steps would
        // start a second copy of the app that is already being served.
        let definition = tool().0.definition();
        let description = &definition.function.description;
        assert!(
            description.contains("this conversation builds"),
            "{description}"
        );
        assert!(description.contains("logs"), "{description}");
        assert!(
            !definition.function.parameters["properties"]["app"].is_object(),
            "an `app` argument would let one dialog reach another's process"
        );
        assert_eq!(
            definition.function.parameters["required"],
            json!(["action"])
        );
    }
}
