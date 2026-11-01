//! The install an *agent* runs: `install_skill`, wired to the catalog the panel's refresh route
//! reads and to the state file it writes.
//!
//! `crates/skills` decides everything about an install and nothing about who runs it.
//! `crates/tools` holds the tool's shape and no way to reach a catalog. This module is the ground
//! between them, and it lives here for the reason [`crate::refresh`] does: this is the one crate
//! that depends on both sides, and the binary is the only place that knows where the home is.
//!
//! **Three decisions, and each is a way the obvious version is wrong.**
//!
//! - **The publisher gets a deadline.** `installer`'s own module doc says of the real publisher
//!   that whoever wires it in "owes it either a worker or a timeout". [`crate::refresh`] paid that
//!   with a worker thread, because a page must not wait on somebody else's program. A turn is the
//!   other case: it is waiting anyway, inside a request that has already asked a person a question,
//!   so the answer here is the other half of the debt — [`SpawnerPublisher`] runs the program
//!   through the same [`Spawner`] `bash` uses, with a deadline and a kill. A package that hangs
//!   costs this turn its deadline rather than the connection.
//! - **The fence is asked once, before the publisher runs, and about one path.** The install writes
//!   `<deepnest home>/skills/<name>/SKILL.md`, which is outside the app: at `workspace-write` the
//!   call is refused in the policy's own words, plus the mode that would reach it. The state file
//!   at `<deepnest home>/skills/.state.json` is deliberately **not** asked about — `Policy`
//!   protects it so that no tool can edit the record of what is managed, and writing it is not the
//!   caller's decision but the install's own last step, exactly as it is on the panel's path.
//! - **The file first, the record second, and a record that fails is not an install that failed.**
//!   The same order `installer` chose: an interrupted run leaves an unrecorded file, which the next
//!   install refuses to overwrite — the safe direction — rather than a managed entry pointing at
//!   nothing. So a state write that fails is reported as what it is, with the path the skill did
//!   land at, instead of pretending nothing was installed.
//!
//! **No version is recorded here.** `<program> skill` never says which version it publishes, so
//! `install` records none and keeps whatever the state already knew. A version is learned by
//! [`crate::refresh`]'s release check, which is where the panel asks the question; an agent
//! installing a skill is not that question.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use deepnest_skills::installer::{self, Outcome};
use deepnest_skills::{catalog, resolve_dest, Publisher, Report, State, PUBLISH_ARG};
use deepnest_tools::shell::{ShellResult, Spawner};
use deepnest_tools::{SkillInstallRequest, SkillInstaller, ToolContext, ToolResult};

/// How long the publish command may run before it is killed, in seconds.
///
/// Generous on purpose: the command is a package publishing a file it already has, but the package
/// is somebody else's program and some are `node` or `python` entry points that start slowly. Five
/// minutes is long enough for a slow one and short enough that a hung one gives the turn back —
/// which is the whole reason this module exists rather than using the bare publisher.
pub const PUBLISH_TIMEOUT_SECONDS: u64 = 300;

/// The real publisher with a deadline: `<program> skill`, run by the same spawner `bash` uses.
///
/// `Report` is what `crates/skills` reads, so a kill by the deadline arrives there as a run with no
/// exit status — which `Report::succeeded` already treats as a failure, and `describe_failure`
/// reports with the last line of stderr. The deadline note is therefore appended to stderr *last*,
/// because that is the line a person ends up reading.
pub struct SpawnerPublisher {
    spawner: Arc<dyn Spawner>,
    cwd: PathBuf,
    timeout: Duration,
}

impl SpawnerPublisher {
    pub fn new(spawner: Arc<dyn Spawner>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            spawner,
            cwd: cwd.into(),
            timeout: Duration::from_secs(PUBLISH_TIMEOUT_SECONDS),
        }
    }

    /// The same publisher with a different deadline, for a caller that wants one.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Publisher for SpawnerPublisher {
    fn publish(&self, program: &str) -> io::Result<Report> {
        let args = [PUBLISH_ARG.to_string()];
        let ShellResult {
            status,
            stdout,
            mut stderr,
            timed_out,
            ..
        } = self.spawner.run(program, &args, &self.cwd, self.timeout)?;
        if timed_out {
            // Last, because it is the line that explains the missing exit status.
            stderr = format!(
                "{}\n`{program} {PUBLISH_ARG}` did not finish within {} seconds and was killed",
                stderr.trim_end(),
                self.timeout.as_secs()
            );
        }
        Ok(Report {
            // A killed child has no exit code, and inventing one would let the installer read a
            // deadline as a program that ran and failed, which is a different fix.
            status: if timed_out { None } else { status },
            stdout,
            stderr,
        })
    }
}

/// `install_skill`, for the app dialog: the catalog under this home, this turn's policy as the
/// fence, and a publisher that cannot hang.
pub struct DialogInstaller {
    home: PathBuf,
    publisher: Arc<dyn Publisher>,
}

impl DialogInstaller {
    /// `home` is deepnest's own home (`~/.deepnest`), which is what the catalog's `~/skills/`
    /// resolves against and the directory the loader reads — the same value `crate::refresh` hands
    /// `install`, and for the same reason: the user's home would put the file where nothing loads.
    pub fn new(home: impl Into<PathBuf>, publisher: Arc<dyn Publisher>) -> Self {
        Self {
            home: home.into(),
            publisher,
        }
    }
}

impl SkillInstaller for DialogInstaller {
    fn install(&self, request: &SkillInstallRequest, ctx: &ToolContext) -> ToolResult {
        const TOOL: &str = "install_skill";

        let root = deepnest_skills::user_root(&self.home);
        let catalog = catalog::load(&root);
        let Some(entry) = catalog.get(&request.name) else {
            let known: Vec<&str> = catalog
                .installable()
                .map(|entry| entry.name.as_str())
                .collect();
            return ToolResult::refused(
                TOOL,
                format!(
                    "the catalog at {} names no skill called `{}`, so there is nothing to install. \
                     The ones it can install are: {}. Nothing was installed.",
                    catalog::path(&root).display(),
                    request.name,
                    if known.is_empty() {
                        "none — its entries name no way to fetch them".to_string()
                    } else {
                        known.join(", ")
                    }
                ),
            );
        };

        // The fence, and before the publisher runs: an install is somebody else's program, and a
        // directory this call may not write is a refusal that costs nobody a subprocess. An entry
        // whose `dest` does not resolve is left to `install` — it refuses that with better words
        // than this module would invent, and it refuses it before running anything either.
        if let Some(dest) = entry
            .dest
            .as_deref()
            .and_then(|dest| resolve_dest(dest, &self.home))
        {
            if let Some(reason) = ctx.policy.write_refusal(&dest) {
                return ToolResult::refused(
                    TOOL,
                    format!(
                        "{reason}. A skill lives in deepnest's own skills directory, which no \
                         narrower mode reaches: send this call again with `request_mode` \
                         \"danger-full-access\" and a `justification` the user can judge, and they \
                         are asked before anything runs. Nothing was installed."
                    ),
                );
            }
        }

        let state_path = deepnest_skills::state::path(&root);
        let mut state = State::read(&state_path);
        let at = deepnest_agent::session::now_iso();

        let placed =
            match installer::install(entry, &self.home, &mut state, self.publisher.as_ref(), &at) {
                Outcome::Installed(placed) => placed,
                Outcome::Refused(refused) => {
                    return ToolResult::refused(TOOL, format!("{refused}. Nothing was installed."))
                }
            };

        if let Err(error) = state.write(&state_path) {
            return ToolResult::error(
                TOOL,
                format!(
                    "the skill was written to {} but the record of it could not be saved \
                     ({error}), so a later refresh will not know deepnest put it there. The skill \
                     itself is on disk and loads normally.",
                    placed.path.display()
                ),
            );
        }

        let verb = if placed.refreshed {
            "Refreshed"
        } else {
            "Installed"
        };
        ToolResult::ok(
            TOOL,
            format!(
                "{verb} `{}` at {}. It is in your skill index from the next turn on — the loader \
                 reads the skills directory every turn, so nothing needs restarting.",
                placed.name,
                placed.path.display()
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_sandbox::{Mode, Policy};
    use deepnest_skills::state;
    use std::path::Path;
    use std::sync::Mutex;

    /// A skill file as a publishing package would print it.
    const PUBLISHED: &str = "---\nname: browser-harness\ndescription: \"Drive the user's own \
browser over CDP.\"\n---\n\nOpen a page and read it.\n";

    struct Scratch {
        base: PathBuf,
        home: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-install-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let home = base.join("deepnest");
            std::fs::create_dir_all(&home).unwrap();
            Self { base, home }
        }

        /// The real bundled catalog, so the names the tests use are the names this build ships.
        fn with_catalog(name: &str) -> Self {
            let scratch = Self::new(name);
            catalog::ensure(&deepnest_skills::user_root(&scratch.home)).unwrap();
            scratch
        }

        fn skills_root(&self) -> PathBuf {
            deepnest_skills::user_root(&self.home)
        }

        fn skill_file(&self, name: &str) -> PathBuf {
            self.skills_root().join(name).join("SKILL.md")
        }

        fn policy(&self, mode: Mode) -> Policy {
            let temp = self.base.join("temp");
            Policy::for_app(mode, &self.home, "notes", Some(&temp))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    struct StubPublisher {
        calls: Mutex<Vec<String>>,
        report: Report,
    }

    impl StubPublisher {
        fn succeeding() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                report: Report {
                    status: Some(0),
                    stdout: PUBLISHED.to_string(),
                    stderr: String::new(),
                },
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Publisher for StubPublisher {
        fn publish(&self, program: &str) -> io::Result<Report> {
            self.calls.lock().unwrap().push(program.to_string());
            Ok(self.report.clone())
        }
    }

    fn ctx(scratch: &Scratch, mode: Mode) -> ToolContext {
        ToolContext::new(
            scratch.policy(mode),
            scratch.home.join("apps").join("notes"),
        )
    }

    fn install(scratch: &Scratch, mode: Mode, name: &str) -> (ToolResult, Vec<String>) {
        let publisher = StubPublisher::succeeding();
        let installer =
            DialogInstaller::new(&scratch.home, Arc::clone(&publisher) as Arc<dyn Publisher>);
        let result = installer.install(
            &SkillInstallRequest {
                name: name.to_string(),
            },
            &ctx(scratch, mode),
        );
        (result, publisher.calls())
    }

    #[test]
    fn an_approved_call_installs_the_skill_where_the_loader_reads_it() {
        let scratch = Scratch::with_catalog("installed");
        let (result, calls) = install(&scratch, Mode::DangerFullAccess, "browser-harness");
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(calls, vec!["browser-harness".to_string()]);
        let file = scratch.skill_file("browser-harness");
        let text = std::fs::read_to_string(&file).expect("the skill file is on disk");
        assert!(
            text.contains("name: browser-harness"),
            "the published file is what landed: {text}"
        );
        // And the record that lets a later refresh replace it.
        let state = State::read(&state::path(&scratch.skills_root()));
        assert!(
            state.may_refresh("browser-harness"),
            "an installed skill is managed, not the user's: {state:?}"
        );
        // The loader finds it from the next turn on, which is what the result promises.
        let loaded = deepnest_skills::load(&scratch.home, None);
        assert!(
            loaded
                .skills
                .iter()
                .any(|skill| skill.name == "browser-harness"),
            "{:?}",
            loaded
                .skills
                .iter()
                .map(|skill| &skill.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_call_the_mode_does_not_reach_is_refused_before_the_package_runs() {
        let scratch = Scratch::with_catalog("refused");
        let (result, calls) = install(&scratch, Mode::WorkspaceWrite, "browser-harness");
        assert!(result.is_error);
        assert!(
            result.content.contains("outside the writable set"),
            "the policy's own words: {}",
            result.content
        );
        assert!(
            result.content.contains("request_mode"),
            "the refusal names the way forward: {}",
            result.content
        );
        assert!(
            calls.is_empty(),
            "no subprocess for a write that is refused"
        );
        assert!(
            !scratch.skill_file("browser-harness").exists(),
            "nothing was written"
        );
    }

    #[test]
    fn a_read_only_call_is_refused_too() {
        let scratch = Scratch::with_catalog("readonly");
        let (result, calls) = install(&scratch, Mode::ReadOnly, "browser-harness");
        assert!(result.is_error);
        assert!(result.content.contains("read-only"), "{}", result.content);
        assert!(calls.is_empty());
    }

    #[test]
    fn a_name_the_catalog_does_not_know_is_refused_with_the_ones_it_does() {
        let scratch = Scratch::with_catalog("unknown");
        let (result, calls) = install(&scratch, Mode::DangerFullAccess, "browser-harnes");
        assert!(result.is_error);
        assert!(
            result.content.contains("`browser-harnes`"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("browser-harness"),
            "the refusal lists what would have worked: {}",
            result.content
        );
        assert!(calls.is_empty());
    }

    #[test]
    fn an_entry_that_installs_nothing_is_refused_without_running_anything() {
        // `baoyu-design` is in the bundled catalog and is recommended only: it names a directory,
        // not a way to fetch the files.
        let scratch = Scratch::with_catalog("recommended");
        let (result, calls) = install(&scratch, Mode::DangerFullAccess, "baoyu-design");
        assert!(result.is_error);
        assert!(
            result.content.contains("recommended only"),
            "{}",
            result.content
        );
        assert!(calls.is_empty(), "there is no program to run");
    }

    #[test]
    fn a_second_install_refreshes_the_file_it_installed() {
        let scratch = Scratch::with_catalog("refreshed");
        let (first, _) = install(&scratch, Mode::DangerFullAccess, "browser-harness");
        assert!(!first.is_error, "{}", first.content);
        let (second, _) = install(&scratch, Mode::DangerFullAccess, "browser-harness");
        assert!(!second.is_error, "{}", second.content);
        assert!(
            second.content.starts_with("Refreshed"),
            "the second install replaced deepnest's own file: {}",
            second.content
        );
    }

    struct SlowSpawner;

    impl Spawner for SlowSpawner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _cwd: &Path,
            _timeout: Duration,
        ) -> io::Result<ShellResult> {
            assert_eq!(program, "browser-harness");
            assert_eq!(args, vec![PUBLISH_ARG.to_string()]);
            Ok(ShellResult {
                status: None,
                stdout: String::new(),
                stderr: "fetching".to_string(),
                timed_out: true,
                signal: None,
            })
        }
    }

    #[test]
    fn a_publisher_that_runs_out_of_time_is_a_failure_with_no_exit_code() {
        let publisher = SpawnerPublisher::new(Arc::new(SlowSpawner), std::env::temp_dir())
            .with_timeout(Duration::from_secs(7));
        let report = publisher.publish("browser-harness").expect("a report");
        assert_eq!(
            report.status, None,
            "a killed child has no exit code, and inventing one would read as a run that failed"
        );
        assert!(!report.succeeded());
        assert!(
            report.stderr.contains("did not finish within 7 seconds"),
            "{}",
            report.stderr
        );
        assert!(
            report
                .stderr
                .lines()
                .last()
                .is_some_and(|line| line.contains("killed")),
            "the deadline is the last line, which is the one report_failure shows: {}",
            report.stderr
        );
    }

    #[test]
    fn the_publisher_passes_the_program_through_a_spawner_that_runs_it() {
        // The shape, not a run: the real spawner is exercised by `crates/tools`' own tests, and a
        // test that ran a real `browser-harness` would measure whether this machine has one.
        struct EchoSpawner;
        impl Spawner for EchoSpawner {
            fn run(
                &self,
                _program: &str,
                _args: &[String],
                cwd: &Path,
                timeout: Duration,
            ) -> io::Result<ShellResult> {
                assert!(timeout >= Duration::from_secs(1));
                assert!(cwd.is_dir(), "the publisher runs somewhere real");
                Ok(ShellResult {
                    status: Some(0),
                    stdout: "---\nname: x\n---\n".to_string(),
                    stderr: String::new(),
                    timed_out: false,
                    signal: None,
                })
            }
        }
        let publisher = SpawnerPublisher::new(Arc::new(EchoSpawner), std::env::temp_dir());
        let report = publisher.publish("browser-harness").expect("a report");
        assert_eq!(report.status, Some(0));
        assert!(report.succeeded());
        assert_eq!(report.stderr, "");
    }

    #[test]
    fn the_installer_reads_the_catalog_under_the_home_it_was_given() {
        // A guard on the shape every test above relies on: an installer with the wrong home reads
        // an empty catalog and refuses every name, which reads as "no such skill" rather than as a
        // wiring mistake.
        let scratch = Scratch::with_catalog("home");
        let installer = DialogInstaller::new(&scratch.home, StubPublisher::succeeding());
        assert_eq!(installer.home, scratch.home);
        assert!(catalog::path(&scratch.skills_root()).exists());
    }
}
