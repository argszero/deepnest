//! `bash` — one command, a fence around it, and a result a model can read.
//!
//! The order is fixed and it is the point of the tool:
//!
//! 1. **The command scans first** ([`deepnest_command_scan::refuse`]). A fence narrows what a
//!    process can *write*; it says nothing about what the process *is*, so the acts that escape
//!    a fence — stopping the daemon, fetching another machine's credential, opening a tunnel —
//!    are refused before anything is spawned. The scanner's own words are handed back unchanged:
//!    a paraphrase is a second rule that can disagree with the first.
//! 2. **Then the backend builds the confinement** ([`deepnest_sandbox::SandboxBackend::confine`]).
//!    `danger-full-access` is the only mode that returns nothing to prepend.
//! 3. **Then it spawns**, with stdin at EOF and output captured to files.
//!
//! Three details below are choices, not accidents:
//!
//! - **The inner shell is `bash`, never `sh`.** `sh` on macOS is bash in POSIX mode, on Debian it
//!   is dash, and a command that works in one and silently differs in the other is a bug the
//!   model cannot see from a success.
//! - **stdin is `/dev/null`.** A command that waits for input would otherwise sit until the
//!   timeout, and the timeout would report "too slow" for what is really "asked a question
//!   nobody can answer".
//! - **A missing launcher fails closed.** The profile *is* the confinement: spawning without it
//!   is not a weaker sandbox, it is none.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use deepnest_llm::ToolDefinition;
use deepnest_sandbox::backend::{classify, which, Outcome};
use deepnest_sandbox::{available_backend, Confinement, SandboxBackend, SandboxError};
use serde_json::{json, Value};

use crate::args::{count_argument, string_argument};
use crate::{Tool, ToolContext, ToolResult, MAX_RESULT_CHARS};

/// How long a command may run when the caller does not say, in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 120;

/// The longest a single call may ask for, in seconds.
pub const MAX_TIMEOUT_SECONDS: u64 = 3_600;

/// What a finished command produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellResult {
    /// The exit status, or `None` when the process was killed by a signal (Unix) or by the
    /// timeout.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The call's own timeout expired and the child was killed.
    pub timed_out: bool,
    /// The signal that ended the child, where the platform reports one.
    pub signal: Option<i32>,
}

/// The seam between the tool's logic and the operating system, so the logic is testable
/// without spawning anything.
pub trait Spawner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        timeout: Duration,
    ) -> io::Result<ShellResult>;
}

/// The real spawner.
///
/// Output goes to files rather than pipes and the child is polled with `try_wait`. A pipe would
/// deadlock the moment a command filled the pipe buffer while nobody was draining it, and
/// draining it needs a thread per stream — for a tool whose whole job is "show me the output",
/// two temporary files are the smaller machine.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdSpawner;

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Spawner for StdSpawner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        timeout: Duration,
    ) -> io::Result<ShellResult> {
        let serial = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let stem =
            std::env::temp_dir().join(format!("deepnest-bash-{}-{serial}", std::process::id()));
        let out_path = stem.with_extension("out");
        let err_path = stem.with_extension("err");
        let out = std::fs::File::create(&out_path)?;
        let err = std::fs::File::create(&err_path)?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        let started = Instant::now();
        let mut child = command.spawn()?;
        let mut timed_out = false;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if started.elapsed() >= timeout {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = read_and_remove(&out_path);
        let stderr = read_and_remove(&err_path);
        Ok(ShellResult {
            status: status.and_then(|status| status.code()),
            signal: status.and_then(signal_of),
            timed_out,
            stdout,
            stderr,
        })
    }
}

#[cfg(unix)]
fn signal_of(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

fn read_and_remove(path: &Path) -> String {
    // Lossy on purpose: a command may print bytes that are not UTF-8, and turning the result
    // into an error would hide the output that would explain it.
    let text = std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(path);
    text
}

/// What a call would run, before anything runs.
///
/// Exposed because it is the part worth asserting on: the argv, and which confinement wraps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    pub program: String,
    pub args: Vec<String>,
    pub confinement: Confinement,
}

impl ShellOutcome {
    /// The final argv, launcher included, for a log line or a test assertion.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = vec![self.program.clone()];
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Whether the call runs under a fence at all.
    pub fn confined(&self) -> bool {
        !matches!(self.confinement, Confinement::Unconfined)
    }
}

/// Run one shell command inside the fence.
pub struct BashTool {
    spawner: Box<dyn Spawner>,
    backend: Box<dyn SandboxBackend>,
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            spawner: Box::new(StdSpawner),
            backend: available_backend(),
        }
    }

    /// The tool with its two seams supplied — the shape the tests use, so no test spawns a
    /// confined process.
    pub fn with_parts(spawner: Box<dyn Spawner>, backend: Box<dyn SandboxBackend>) -> Self {
        Self { spawner, backend }
    }

    /// The shell invocation for this platform, before any confinement.
    ///
    /// `bash -c` on Unix and PowerShell on Windows. Not `sh` — see the module note.
    pub fn shell_argv(command: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "powershell.exe".to_string(),
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    command.to_string(),
                ],
            )
        } else {
            (
                "bash".to_string(),
                vec!["-c".to_string(), command.to_string()],
            )
        }
    }

    /// Resolve everything but the spawn: the shell, the launcher, the confinement.
    pub fn plan(&self, command: &str, ctx: &ToolContext) -> Result<ShellOutcome, String> {
        let (program, args) = Self::shell_argv(command);
        let mut inner = Vec::with_capacity(args.len() + 1);
        inner.push(program.clone());
        inner.extend(args.iter().cloned());
        let confinement = self
            .backend
            .confine(&ctx.policy, &inner)
            .map_err(|error: SandboxError| error.to_string())?;
        if let Some(launcher) = self.backend.launcher() {
            // `confine` derives the profile; whether the launcher exists is a separate question
            // and it is answered here, before a spawn can fail open.
            if !self.backend.launcher_available(&which) {
                return Err(SandboxError::LauncherMissing {
                    program: launcher,
                    platform: self.backend.platform().as_str(),
                }
                .to_string());
            }
        }
        match confinement {
            Confinement::Unconfined => Ok(ShellOutcome {
                program,
                args,
                confinement: Confinement::Unconfined,
            }),
            Confinement::Launcher(invocation) => {
                // The backend's own invocation already carries the command: `confine` is handed
                // the argv and returns the launcher plus everything to pass it. Extending again
                // here would run the command twice over.
                Ok(ShellOutcome {
                    program: invocation.program.clone(),
                    args: invocation.args.clone(),
                    confinement: Confinement::Launcher(invocation),
                })
            }
            Confinement::Acl(_) => Err(
                "this platform's confinement is an ACL plan plus a restricted token, and the \
                 applier for it is not part of this build yet. The call is refused rather than \
                 run unconfined: a plan that is derived but not applied is no fence at all."
                    .to_string(),
            ),
        }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "bash",
            "Run a shell command inside the app's sandbox and return its exit status, stdout and \
             stderr. The command runs with bash, from the app directory, with stdin closed — a \
             command that would prompt must be given its input instead. A non-zero exit is a \
             result, not a tool failure.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command line to run."
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "description": format!("Seconds before the command is killed. Defaults to {DEFAULT_TIMEOUT_SECONDS}; at most {MAX_TIMEOUT_SECONDS}.")
                    }
                },
                "required": ["command"]
            }),
        )
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let command = match string_argument(args, &["command"]) {
            Ok(command) => command,
            Err(reason) => return ToolResult::refused("bash", reason),
        };
        if command.trim().is_empty() {
            return ToolResult::refused("bash", "command is empty; there is nothing to run");
        }
        let seconds = match count_argument(
            args,
            &["timeout", "timeout_seconds"],
            1,
            Some(DEFAULT_TIMEOUT_SECONDS),
        ) {
            Ok(Some(seconds)) => seconds.min(MAX_TIMEOUT_SECONDS),
            Ok(None) => DEFAULT_TIMEOUT_SECONDS,
            Err(reason) => return ToolResult::refused("bash", reason),
        };
        // Rule 1: the acts a fence cannot see. Its message is returned unchanged.
        if let Some(refusal) = deepnest_command_scan::refuse(&command, &ctx.scan) {
            return ToolResult::refused("bash", refusal.message);
        }
        // Rule 2: the confinement, or the reason there is none to build.
        let plan = match self.plan(&command, ctx) {
            Ok(plan) => plan,
            Err(reason) => return ToolResult::refused("bash", reason),
        };
        // Rule 3: the spawn.
        let result = match self.spawner.run(
            &plan.program,
            &plan.args,
            &ctx.workdir,
            Duration::from_secs(seconds),
        ) {
            Ok(result) => result,
            Err(err) => {
                let hint = if err.kind() == io::ErrorKind::NotFound {
                    format!(
                        " — {} was not found, and it is what this tool runs every command with",
                        plan.program
                    )
                } else {
                    String::new()
                };
                return ToolResult::error("bash", format!("cannot start the command: {err}{hint}"));
            }
        };
        render(&result, seconds, plan.confined(), self.backend.as_ref()).truncate(MAX_RESULT_CHARS)
    }
}

/// Turn a finished command into the text a model reads.
///
/// The status line comes first and always, because "no output" and "no output because it was
/// denied" must not look the same. A non-zero exit is **not** a tool error: the command ran.
fn render(
    result: &ShellResult,
    seconds: u64,
    confined: bool,
    fence: &dyn SandboxBackend,
) -> ToolResult {
    let mut notes: Vec<String> = Vec::new();
    let status_line = if result.timed_out {
        notes.push(format!(
            "timed out after {seconds}s and the process was killed; give it a longer timeout, or \
             run something narrower"
        ));
        format!("timed out after {seconds}s")
    } else if let Some(signal) = result.signal {
        notes.push(format!(
            "the process was ended by signal {signal}; it did not exit on its own"
        ));
        format!("killed by signal {signal}")
    } else {
        match result.status {
            Some(code) => format!("exit status {code}"),
            None => "exit status unknown".to_string(),
        }
    };
    // The backend's own signatures, so a denial is named as one rather than left for the model to
    // infer from an `operation not permitted` buried in a stack trace. One classification pass:
    // reading the same stderr twice is how two notes end up disagreeing.
    let outcome = if confined {
        classify(fence, &result.stderr)
    } else {
        Outcome::Passed
    };
    let mut runner_failed = false;
    match &outcome {
        Outcome::RunnerFailure(signature) => {
            runner_failed = true;
            notes.push(format!(
                "the sandbox runner ({}) failed before the command started, so the command never \
                 ran — this is not a denial and not the command's own failure: matched \
                 {signature:?}",
                fence.name()
            ));
        }
        Outcome::Denied(signature) => notes.push(format!(
            "the sandbox denied a write (matched {signature:?}); the command ran and was stopped at \
             that point"
        )),
        Outcome::Passed => {}
    }
    let mut body = format!("[{status_line}]");
    for note in &notes {
        body.push_str(&format!("\n[{note}]"));
    }
    let stdout = result.stdout.trim_end_matches('\n');
    let stderr = result.stderr.trim_end_matches('\n');
    if !stdout.is_empty() {
        body.push_str("\n--- stdout ---\n");
        body.push_str(stdout);
    }
    if !stderr.is_empty() {
        body.push_str("\n--- stderr ---\n");
        body.push_str(stderr);
    }
    if stdout.is_empty() && stderr.is_empty() && !result.timed_out {
        body.push_str("\n(no output)");
    }
    if result.timed_out || runner_failed {
        ToolResult::error("bash", body)
    } else {
        ToolResult::ok("bash", body)
    }
}

/// A backend whose answers are fixed, so the tests can assert the tool obeys them.
#[cfg(test)]
pub(crate) struct StubBackend {
    /// The launcher this backend claims to need, if any.
    pub launcher: Option<&'static str>,
    /// Whether that launcher is reachable.
    pub launcher_present: bool,
    pub confine: Confinement,
}

#[cfg(test)]
impl SandboxBackend for StubBackend {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn platform(&self) -> deepnest_sandbox::backend::Platform {
        deepnest_sandbox::backend::Platform::current()
    }

    fn denial_signatures(&self) -> &'static [&'static str] {
        &["the policy said no"]
    }

    fn runner_failure_signatures(&self) -> &'static [&'static str] {
        &["stub-runner: "]
    }

    fn launcher(&self) -> Option<&'static str> {
        self.launcher
    }

    fn confine(
        &self,
        _policy: &deepnest_sandbox::Policy,
        command: &[String],
    ) -> Result<Confinement, SandboxError> {
        match &self.confine {
            Confinement::Unconfined => Ok(Confinement::Unconfined),
            Confinement::Launcher(invocation) => {
                let mut args = invocation.args.clone();
                args.extend(command.iter().cloned());
                Ok(Confinement::Launcher(deepnest_sandbox::Invocation::new(
                    invocation.program.clone(),
                    args,
                )))
            }
            other => Ok(other.clone()),
        }
    }

    fn launcher_available(&self, _lookup: &dyn Fn(&str) -> Option<std::path::PathBuf>) -> bool {
        self.launcher_present
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_sandbox::{AclPlan, Invocation, Mode, Policy};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Records what it was asked to run and answers with what the test queued.
    struct RecordingSpawner {
        seen: Mutex<Vec<(String, Vec<String>)>>,
        result: ShellResult,
    }

    impl RecordingSpawner {
        fn new(result: ShellResult) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                result,
            }
        }
    }

    impl Spawner for RecordingSpawner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _cwd: &Path,
            _timeout: Duration,
        ) -> io::Result<ShellResult> {
            self.seen
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            Ok(self.result.clone())
        }
    }

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "deepnest-tools-shell-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn ctx(&self, mode: Mode) -> ToolContext {
            ToolContext::new(
                Policy::from_roots(
                    mode,
                    self.base.clone(),
                    self.base.clone(),
                    self.base.clone(),
                ),
                self.base.clone(),
            )
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// A backend that confines with a launcher it says is present.
    fn stub(confinement: Confinement, launcher_present: bool) -> Box<dyn SandboxBackend> {
        Box::new(StubBackend {
            launcher: Some("stub-launcher"),
            launcher_present,
            confine: confinement,
        })
    }

    fn exiting(code: i32, stdout: &str, stderr: &str) -> ShellResult {
        ShellResult {
            status: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            timed_out: false,
            signal: None,
        }
    }

    #[test]
    fn an_unconfined_call_runs_the_inner_shell_with_the_command() {
        let scratch = Scratch::new("plain");
        let spawner = RecordingSpawner::new(exiting(0, "hi\n", ""));
        let tool = BashTool::with_parts(Box::new(spawner), stub(Confinement::Unconfined, true));
        let result = tool.call(
            &json!({"command": "echo hi"}),
            &scratch.ctx(Mode::DangerFullAccess),
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("exit status 0"),
            "{}",
            result.content
        );
        assert!(result.content.contains("hi"), "{}", result.content);
    }

    #[cfg(unix)]
    #[test]
    fn the_inner_shell_is_bash_and_not_sh() {
        // `sh` is bash in POSIX mode on macOS and dash on Debian; a command that differs between
        // them fails in a way the model cannot see from a success.
        let (program, args) = BashTool::shell_argv("echo hi");
        assert_eq!(program, "bash");
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    #[test]
    fn a_confined_call_prepends_the_launcher_and_its_profile() {
        let scratch = Scratch::new("confined");
        let plan = temp_plan(&scratch, Mode::WorkspaceWrite);
        assert_eq!(plan.program, "stub-launcher");
        assert_eq!(plan.args[0], "-p");
        assert!(
            plan.args.contains(&"echo hi".to_string()),
            "{:?}",
            plan.args
        );
        assert!(plan.confined());
    }

    fn temp_plan(scratch: &Scratch, mode: Mode) -> ShellOutcome {
        let tool = BashTool::with_parts(
            Box::new(RecordingSpawner::new(exiting(0, "", ""))),
            stub(
                Confinement::Launcher(Invocation::new(
                    "stub-launcher",
                    vec!["-p".to_string(), "profile".to_string(), "--".to_string()],
                )),
                true,
            ),
        );
        tool.plan("echo hi", &scratch.ctx(mode)).unwrap()
    }

    #[test]
    fn the_scanner_s_words_are_returned_unchanged() {
        let scratch = Scratch::new("scan");
        let spawner = RecordingSpawner::new(exiting(0, "", ""));
        let tool = BashTool::with_parts(Box::new(spawner), stub(Confinement::Unconfined, true));
        let result = tool.call(
            &json!({"command": "deepnest stop"}),
            &scratch.ctx(Mode::DangerFullAccess),
        );
        assert!(result.is_error);
        assert!(
            result.content.contains("stops or restarts deepnest"),
            "{}",
            result.content
        );
    }

    #[test]
    fn a_denial_is_named_rather_than_left_in_a_stack_trace() {
        let scratch = Scratch::new("denied");
        let spawner = RecordingSpawner::new(exiting(1, "", "bash: /x: the policy said no\n"));
        let tool = BashTool::with_parts(
            Box::new(spawner),
            stub(
                Confinement::Launcher(Invocation::new("stub-launcher", Vec::new())),
                true,
            ),
        );
        let result = tool.call(
            &json!({"command": "echo hi > /x"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(!result.is_error, "the command ran: {}", result.content);
        assert!(
            result.content.contains("the sandbox denied a write"),
            "{}",
            result.content
        );
    }

    #[test]
    fn a_runner_failure_says_the_command_never_ran() {
        let scratch = Scratch::new("runner-failure");
        let spawner = RecordingSpawner::new(exiting(1, "", "stub-runner: cannot build profile\n"));
        let tool = BashTool::with_parts(
            Box::new(spawner),
            stub(
                Confinement::Launcher(Invocation::new("stub-launcher", Vec::new())),
                true,
            ),
        );
        let result = tool.call(
            &json!({"command": "echo hi"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("never"), "{}", result.content);
    }

    #[test]
    fn a_timeout_is_reported_as_one() {
        let scratch = Scratch::new("timeout");
        let spawner = RecordingSpawner::new(ShellResult {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
            signal: None,
        });
        let tool = BashTool::with_parts(Box::new(spawner), stub(Confinement::Unconfined, true));
        let result = tool.call(
            &json!({"command": "sleep 100", "timeout": 3}),
            &scratch.ctx(Mode::DangerFullAccess),
        );
        assert!(result.is_error);
        assert!(
            result.content.contains("timed out after 3s"),
            "{}",
            result.content
        );
    }

    #[test]
    fn a_non_zero_exit_is_a_result_and_not_a_tool_failure() {
        let scratch = Scratch::new("non-zero");
        let spawner = RecordingSpawner::new(exiting(2, "", "grep: no such file\n"));
        let tool = BashTool::with_parts(Box::new(spawner), stub(Confinement::Unconfined, true));
        let result = tool.call(
            &json!({"command": "grep x missing"}),
            &scratch.ctx(Mode::DangerFullAccess),
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("exit status 2"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("no such file"),
            "{}",
            result.content
        );
    }

    #[test]
    fn a_missing_launcher_fails_closed_rather_than_running_unconfined() {
        // The launcher's profile *is* the confinement: spawning without it is not a weaker
        // sandbox, it is none at all.
        let scratch = Scratch::new("no-launcher");
        let tool = BashTool::with_parts(
            Box::new(RecordingSpawner::new(exiting(0, "", ""))),
            stub(
                Confinement::Launcher(Invocation::new("stub-launcher", Vec::new())),
                false,
            ),
        );
        let refusal = tool
            .plan("echo hi", &scratch.ctx(Mode::WorkspaceWrite))
            .unwrap_err();
        assert!(refusal.contains("stub-launcher"), "{refusal}");
        assert!(
            refusal.contains("does not run a write-capable command unconfined"),
            "{refusal}"
        );
        let result = tool.call(
            &json!({"command": "echo hi"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error, "{}", result.content);
    }

    #[test]
    fn an_acl_confinement_is_refused_while_its_applier_does_not_exist() {
        let scratch = Scratch::new("acl");
        let tool = BashTool::with_parts(
            Box::new(RecordingSpawner::new(exiting(0, "", ""))),
            Box::new(StubBackend {
                launcher: None,
                launcher_present: true,
                confine: Confinement::Acl(AclPlan {
                    deny: Vec::new(),
                    allow: Vec::new(),
                    restricted_token: true,
                    mode: Mode::WorkspaceWrite,
                }),
            }),
        );
        let refusal = tool
            .plan("echo hi", &scratch.ctx(Mode::WorkspaceWrite))
            .unwrap_err();
        assert!(refusal.contains("ACL plan"), "{refusal}");
    }

    #[test]
    fn an_empty_command_is_refused() {
        let scratch = Scratch::new("empty");
        let tool = BashTool::with_parts(
            Box::new(RecordingSpawner::new(exiting(0, "", ""))),
            stub(Confinement::Unconfined, true),
        );
        let result = tool.call(
            &json!({"command": "   "}),
            &scratch.ctx(Mode::DangerFullAccess),
        );
        assert!(result.is_error);
        assert!(result.content.contains("empty"), "{}", result.content);
    }

    #[test]
    fn an_out_of_domain_timeout_is_refused_rather_than_clamped() {
        let scratch = Scratch::new("bad-timeout");
        let tool = BashTool::with_parts(
            Box::new(RecordingSpawner::new(exiting(0, "", ""))),
            stub(Confinement::Unconfined, true),
        );
        let result = tool.call(
            &json!({"command": "echo hi", "timeout": -1}),
            &scratch.ctx(Mode::DangerFullAccess),
        );
        assert!(result.is_error);
        assert!(result.content.contains("timeout"), "{}", result.content);
    }
}
