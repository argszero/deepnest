//! Turning a created app into a binary deepnest can start.
//!
//! Creating an app writes text; this runs the toolchain, and the difference between those two is the
//! reason they are separate steps with separate failure modes. A create that fails halfway leaves a
//! directory that is not an app. A build that fails halfway leaves a source tree that is fine and a
//! toolchain that said no — and the only useful thing to hand the user is **which step** and **its
//! output**.
//!
//! Four decisions:
//!
//! - **The plan is data, not a shell script.** [`plan`] returns the steps, in order, each with its
//!   own directory and arguments. A generated `#!/bin/sh` file would have been a line shorter and
//!   untestable: what a build does would then live in a string nobody reads, on a platform whose
//!   shell is not the one the string was written for.
//! - **Running a step is a seam** ([`Runner`]). The interesting behaviour is the ordering, the
//!   first-failure stop and the report; a test that needed bun and a C compiler to observe that
//!   would be a test that fails for reasons unrelated to this code.
//! - **The front end builds before the server.** The server reads `frontend/dist` per request, so
//!   the order is not load-bearing for linking — it is chosen so that a build failing on the front
//!   end has not already spent a minute compiling SQLite.
//! - **Success means the binary is there.** The plan's exit codes say the toolchain was happy; the
//!   supervisor's contract is a path, and a build reporting success without it turns into "nothing
//!   has been built yet" on the next start — one step away from the cause.
//!
//! What this does **not** do: it does not watch, and it does not run in the background. A build
//! occupies the request that asked for it, and this binary serves one connection at a time, so the
//! panel waits. A build that reports progress and lets the panel stay live belongs to
//! `crates/gateway`, with the real server loop.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::process::{run_command, EXPECTED_BINARY};

/// How long one step may take before it is killed.
///
/// A cold build of a generated app measured 31.4 s with everything to download, so ten minutes is
/// two orders of magnitude of headroom. It exists so that a hung toolchain reports "did not finish"
/// instead of hanging the one connection this server serves.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(600);

/// How often a running step is checked for having finished.
const POLL: Duration = Duration::from_millis(50);

/// One command in a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What to call it in a report: `bun install`, `bun run build`.
    pub label: &'static str,
    /// The program, resolved through `PATH`.
    pub program: &'static str,
    /// Its arguments, in order.
    pub args: &'static [&'static str],
    /// Where to run it. An app's front end is not built from the app's root.
    pub cwd: PathBuf,
}

/// How a step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The exit code, when the platform gave one.
    pub code: Option<i32>,
    /// The platform's own words, or the reason there is no code.
    pub detail: String,
}

impl Outcome {
    /// Whether the step succeeded.
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs one build step.
///
/// A seam rather than a direct `Command`, for the reason every other one in this workspace exists:
/// the behaviour worth testing is the ordering and the report, and both are observable without a
/// toolchain.
pub trait Runner: Send + Sync {
    /// Run `step`, appending its output to `log`.
    fn run(&self, step: &Step, log: &Path) -> io::Result<Outcome>;
}

/// The real runner.
///
/// Output is appended to the log rather than piped: an undrained pipe fills and deadlocks the child,
/// the same reason `crates/tools` redirects to a file. stdin is `/dev/null` so that a toolchain which
/// decides to ask a question fails instead of taking the terminal of whatever started deepnest.
#[derive(Debug, Clone)]
pub struct StdRunner {
    /// How long one step may run.
    pub timeout: Duration,
}

impl Default for StdRunner {
    fn default() -> Self {
        Self {
            timeout: STEP_TIMEOUT,
        }
    }
}

impl StdRunner {
    /// A runner with the default timeout.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same runner, waiting at most `timeout` per step.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Runner for StdRunner {
    fn run(&self, step: &Step, log: &Path) -> io::Result<Outcome> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let sink = OpenOptions::new().create(true).append(true).open(log)?;
        let sink_err = sink.try_clone()?;

        let mut child = Command::new(step.program)
            .args(step.args)
            .current_dir(&step.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(sink))
            .stderr(Stdio::from(sink_err))
            .spawn()?;

        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Outcome {
                    code: status.code(),
                    detail: match status.code() {
                        Some(code) => format!("exited with {code}"),
                        None => "was ended by a signal".to_string(),
                    },
                });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                return Ok(Outcome {
                    code: None,
                    detail: format!("did not finish within {}s", self.timeout.as_secs()),
                });
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Why an app did not build.
#[derive(Debug)]
pub enum BuildError {
    /// The directory is not an app: no manifest, or one that does not parse. That is its own
    /// problem rather than a build failure — there is nothing here to build.
    NotApp { dir: PathBuf, reason: String },
    /// The program a step needs could not be started: usually "not on this machine", and the
    /// `source` says which, because a `NotFound` and a permission error want different actions.
    MissingTool {
        step: &'static str,
        program: &'static str,
        source: io::Error,
    },
    /// It ran and failed.
    Failed {
        step: &'static str,
        outcome: Outcome,
        log: PathBuf,
    },
    /// Every step succeeded and the binary the supervisor looks for is still not there.
    NoBinary { expected: PathBuf, log: PathBuf },
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotApp { dir, reason } => write!(f, "{} is not an app: {reason}", dir.display()),
            Self::MissingTool {
                step,
                program,
                source,
            } => write!(
                f,
                "`{step}` needs `{program}`, which could not be started: {source}"
            ),
            Self::Failed { step, outcome, log } => write!(
                f,
                "`{step}` {} — its output is in {}",
                outcome.detail,
                log.display()
            ),
            Self::NoBinary { expected, log } => write!(
                f,
                "the build reported success but {} is not there — see {}",
                expected.display(),
                log.display()
            ),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MissingTool { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// An app that has been built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The app directory.
    pub dir: PathBuf,
    /// The binary the supervisor starts.
    pub binary: PathBuf,
    /// Where the build's output went.
    pub log: PathBuf,
}

/// The steps that build the app in `dir`, in the order they run.
///
/// `bun run build` rather than the bundler's own arguments spelled out here: how the front end is
/// bundled is the app's to change, so the plan names the script instead of copying it.
pub fn plan(dir: &Path) -> Vec<Step> {
    let frontend = dir.join("frontend");
    vec![
        Step {
            label: "bun install",
            program: "bun",
            args: &["install"],
            cwd: frontend.clone(),
        },
        Step {
            label: "bun run build",
            program: "bun",
            args: &["run", "build"],
            cwd: frontend,
        },
        Step {
            label: "cargo build --release",
            program: "cargo",
            args: &["build", "--release"],
            cwd: dir.to_path_buf(),
        },
    ]
}

/// Build the app in `dir`, appending output to `log`.
pub fn build(dir: &Path, log: PathBuf, runner: &dyn Runner) -> Result<Built, BuildError> {
    // It has to be an app before it can be built: a directory with no manifest is not a build that
    // failed, it is the wrong directory.
    crate::manifest::load(dir).map_err(|error| BuildError::NotApp {
        dir: dir.to_path_buf(),
        reason: error.to_string(),
    })?;

    for step in plan(dir) {
        note(&log, &format!("== {} ==", step.label));
        let outcome = runner
            .run(&step, &log)
            .map_err(|source| BuildError::MissingTool {
                step: step.label,
                program: step.program,
                source,
            })?;
        if !outcome.succeeded() {
            note(&log, &format!("   {} — stopped here", outcome.detail));
            return Err(BuildError::Failed {
                step: step.label,
                outcome,
                log,
            });
        }
        note(&log, "   ok");
    }

    // The exit codes said the toolchain was happy; the contract is a path.
    let Some(binary) = run_command(dir) else {
        return Err(BuildError::NoBinary {
            expected: dir.join(EXPECTED_BINARY),
            log,
        });
    };
    Ok(Built {
        dir: dir.to_path_buf(),
        binary,
        log,
    })
}

/// Write one line to the build log, best effort.
///
/// A log that cannot be written is not a reason to abandon a build: the steps still run and their
/// own output still goes to the same file. What is lost is the framing, which is worth less than the
/// build.
fn note(log: &Path, line: &str) {
    let _ = (|| -> io::Result<()> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(log)?;
        writeln!(file, "{line}")
    })();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::Slug;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-build-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn dir(&self) -> PathBuf {
            self.base.join("apps").join("notes")
        }

        fn log(&self) -> PathBuf {
            self.base.join("logs").join("notes.build.log")
        }

        /// An app created by the template: a manifest plus the files a build expects.
        fn app(&self) -> PathBuf {
            let dir = self.dir();
            crate::template::create(&Slug::parse("notes").unwrap(), &dir, "Notes").unwrap();
            dir
        }

        /// A manifest only, with no source: a directory that is an app and cannot be built.
        fn manifest_only(&self) -> PathBuf {
            let dir = self.dir();
            crate::Manifest::new("Notes").save(&dir).unwrap();
            dir
        }

        fn put_binary(&self, dir: &Path) {
            let binary = dir.join(EXPECTED_BINARY);
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, b"a stand-in for the built app").unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// A runner that records what it was asked and answers with scripted outcomes.
    struct FakeRunner {
        steps: Mutex<Vec<Step>>,
        outcomes: Mutex<Vec<Outcome>>,
    }

    impl FakeRunner {
        fn succeeding(count: usize) -> Self {
            Self::with_outcomes(vec![
                Outcome {
                    code: Some(0),
                    detail: "exited with 0".to_string(),
                };
                count
            ])
        }

        fn with_outcomes(outcomes: Vec<Outcome>) -> Self {
            Self {
                steps: Mutex::new(Vec::new()),
                outcomes: Mutex::new(outcomes),
            }
        }

        fn steps(&self) -> Vec<Step> {
            self.steps.lock().unwrap().clone()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, step: &Step, _log: &Path) -> io::Result<Outcome> {
            self.steps.lock().unwrap().push(step.clone());
            let mut outcomes = self.outcomes.lock().unwrap();
            if outcomes.is_empty() {
                return Ok(Outcome {
                    code: Some(0),
                    detail: "exited with 0".to_string(),
                });
            }
            Ok(outcomes.remove(0))
        }
    }

    fn ok() -> Outcome {
        Outcome {
            code: Some(0),
            detail: "exited with 0".to_string(),
        }
    }

    fn failed() -> Outcome {
        Outcome {
            code: Some(101),
            detail: "exited with 101".to_string(),
        }
    }

    /// A runner whose first call cannot start the program at all.
    struct NoTool;

    impl Runner for NoTool {
        fn run(&self, _step: &Step, _log: &Path) -> io::Result<Outcome> {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no such file or directory",
            ))
        }
    }

    #[test]
    fn the_plan_builds_the_frontend_first_and_the_server_last() {
        let scratch = Scratch::new("plan");
        let dir = scratch.app();
        let steps = plan(&dir);

        assert_eq!(
            steps.iter().map(|step| step.label).collect::<Vec<_>>(),
            ["bun install", "bun run build", "cargo build --release"]
        );
        // The front end's two steps run where the front end is; the cargo step runs at the app root.
        assert_eq!(steps[0].cwd, dir.join("frontend"));
        assert_eq!(steps[1].cwd, dir.join("frontend"));
        assert_eq!(steps[2].cwd, dir);
        assert_eq!(steps[2].args, ["build", "--release"]);
    }

    #[test]
    fn a_successful_build_reports_the_binary_at_the_path_the_supervisor_uses() {
        let scratch = Scratch::new("success");
        let dir = scratch.app();
        // The steps are what produce it; the fake runner stands in for the toolchain that would.
        scratch.put_binary(&dir);

        let built = build(&dir, scratch.log(), &FakeRunner::succeeding(3)).unwrap();
        assert_eq!(built.binary, dir.join(EXPECTED_BINARY));
        assert_eq!(built.dir, dir);
    }

    #[test]
    fn every_step_runs_in_the_order_it_was_planned() {
        let scratch = Scratch::new("order");
        let dir = scratch.app();
        scratch.put_binary(&dir);
        let runner = FakeRunner::succeeding(3);

        build(&dir, scratch.log(), &runner).unwrap();
        assert_eq!(runner.steps(), plan(&dir));
    }

    #[test]
    fn the_first_failing_step_stops_the_build_and_the_plans_after_it_are_not_run() {
        // A front end that will not bundle leaves no reason to compile SQLite for a minute.
        let scratch = Scratch::new("stop");
        let dir = scratch.app();
        let runner = FakeRunner::with_outcomes(vec![ok(), failed()]);

        let error = build(&dir, scratch.log(), &runner).unwrap_err();
        match &error {
            BuildError::Failed { step, outcome, .. } => {
                assert_eq!(*step, "bun run build");
                assert_eq!(outcome.code, Some(101));
            }
            other => panic!("expected the second step to fail, got {other:?}"),
        }
        assert_eq!(runner.steps().len(), 2, "the cargo step must not run");
    }

    #[test]
    fn a_missing_tool_names_the_step_and_the_program() {
        // "bun is not installed" and "your front end has a typo" are the same failure to the user
        // unless the report says which one it is.
        let scratch = Scratch::new("no-tool");
        let dir = scratch.app();
        let error = build(&dir, scratch.log(), &NoTool).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("bun install"), "{message}");
        assert!(message.contains("`bun`"), "{message}");
    }

    #[test]
    fn a_directory_that_is_not_an_app_is_refused_before_any_step_runs() {
        let scratch = Scratch::new("not-app");
        let dir = scratch.dir();
        std::fs::create_dir_all(&dir).unwrap();
        let runner = FakeRunner::succeeding(3);

        let error = build(&dir, scratch.log(), &runner).unwrap_err();
        assert!(matches!(error, BuildError::NotApp { .. }), "{error:?}");
        assert!(runner.steps().is_empty(), "nothing should have been run");
    }

    #[test]
    fn a_build_that_reports_success_without_a_binary_is_an_error_not_a_success() {
        // Otherwise the next start says "nothing has been built yet", one step away from here.
        let scratch = Scratch::new("no-binary");
        let dir = scratch.manifest_only();
        let error = build(&dir, scratch.log(), &FakeRunner::succeeding(3)).unwrap_err();

        match &error {
            BuildError::NoBinary { expected, log } => {
                assert!(expected.ends_with(EXPECTED_BINARY));
                assert_eq!(log, &scratch.log());
            }
            other => panic!("expected NoBinary, got {other:?}"),
        }
    }

    #[test]
    fn the_build_log_records_each_step_so_a_failure_can_be_read_without_a_console() {
        let scratch = Scratch::new("log");
        let dir = scratch.app();
        let runner = FakeRunner::with_outcomes(vec![ok(), failed()]);

        let _ = build(&dir, scratch.log(), &runner);
        let log = std::fs::read_to_string(scratch.log()).unwrap();

        assert!(log.contains("== bun install =="), "{log}");
        assert!(log.contains("== bun run build =="), "{log}");
        assert!(log.contains("exited with 101 — stopped here"), "{log}");
    }

    #[test]
    fn the_real_runner_reports_a_program_that_is_not_there_rather_than_panicking() {
        // The seam's other half: `StdRunner` against a program no machine has.
        let scratch = Scratch::new("std-missing");
        let step = Step {
            label: "definitely not installed",
            program: "deepnest-no-such-program",
            args: &[],
            cwd: scratch.base.clone(),
        };
        let error = StdRunner::new().run(&step, &scratch.log()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_real_runner_runs_a_step_and_reports_its_exit_code() {
        // One real process, so the seam is not only ever exercised by a fake. `true` exists on
        // every platform this project targets except Windows, where `cmd /c exit 0` is the same
        // thing — the assertion is on the code, not on the program.
        let scratch = Scratch::new("std-ok");
        #[cfg(windows)]
        let (program, args): (&'static str, &'static [&'static str]) = ("cmd", &["/C", "exit 0"]);
        #[cfg(not(windows))]
        let (program, args): (&'static str, &'static [&'static str]) = ("true", &[]);

        let step = Step {
            label: "a program that succeeds",
            program,
            args,
            cwd: scratch.base.clone(),
        };
        let outcome = StdRunner::new().run(&step, &scratch.log()).unwrap();
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        assert!(outcome.succeeded());
    }
}
