//! A refresh the user asks for: rewrite a managed skill from its own package, and record the
//! version that was adopted with it.
//!
//! `crates/skills` decides everything about an install and everything about a release check, and
//! neither half is wired to anything: `installer` says of the real publisher "whoever wires it in
//! owes it either a worker or a timeout", and `release` says the same of its own seam. This module
//! is the caller that pays both debts, and it lives here because this is the one crate that depends
//! on both sides — the same reason `crate::updates` is here and not in `crates/skills`.
//!
//! **Four decisions, and each is a way the obvious version is wrong.**
//!
//! - **A refresh is not a request the page waits on.** The real publisher is the package, run with
//!   no timeout: `Command::output()` returns when the program decides to. A `POST` that waited for
//!   it would hold one HTTP connection for as long as somebody else's program takes, so the route
//!   **enqueues** and answers `202`, and a worker thread of its own does the work — the shape
//!   `crate::rebuild` already uses for the same reason, one step further from the page.
//! - **One refresh at a time, for the whole queue.** The state file is **one** file: two workers
//!   each read it, each record their own skill, and the second write loses the first one's record —
//!   a silent loss, because both installs really did happen. Nothing here is worth that, so a
//!   second request while one runs is refused by name (`409`) rather than queued behind it.
//! - **The file first, the version second.** `<program> skill` never says which version it is
//!   publishing, so `install` deliberately records no version and keeps whatever was there. The one
//!   place a version can be learned is the release check, and the verdict it returns —
//!   `Verdict::adoptable()` — is what a refresh records through `State::record_update`. That is also
//!   the path by which a skill installed before the check existed becomes comparable at all
//!   (`Verdict::Unknown`): its first refresh gives it a version to compare next time.
//! - **A check that cannot answer does not undo the refresh.** The forge may be unreachable, rate
//!   limited, or publishing tags this build cannot order. The file on disk is still refreshed from
//!   the local package, so the outcome is a refresh whose version could not be learned — reported as
//!   the version it kept, with `None` when it had none, and never as a number that was guessed.
//!
//! What it does **not** do: it does not decide whether a newer release exists (that is the panel's
//! question, answered by `GET /api/skills/updates`), and it cannot upgrade the package itself — a
//! refresh rewrites the skill from the copy that is installed. A package that is behind its own
//! release still publishes the older file, which is exactly why the version recorded is the one the
//! check returned and not one this module invented.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use deepnest_skills::installer::{self, Outcome};
use deepnest_skills::{catalog, release, Publisher, Releases, State};
use serde_json::{json, Value};

/// What is true about the last refresh this process ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// A worker is running the publisher now.
    Running {
        /// The skill being rewritten.
        name: String,
    },
    /// The file is on disk, and the state file records it.
    Refreshed {
        /// The skill that was rewritten.
        name: String,
        /// The version adopted from the check, or the one that was already recorded when the check
        /// could not answer. `None` when neither side knows one.
        version: Option<String>,
        /// Whether this replaced a skill deepnest had installed before, rather than arriving fresh.
        replaced: bool,
    },
    /// Nothing was written, and the detail is the sentence saying why.
    ///
    /// The refusal's own words travel here rather than a variant per cause: `crates/skills` already
    /// words every one of them as a next step ("move it aside, then install again"), and rewording
    /// them here would make two vocabularies for one refusal.
    Failed {
        /// The skill that was asked for.
        name: String,
        /// What went wrong, as the person reads it.
        detail: String,
    },
}

impl Status {
    /// The word the page switches on.
    pub fn state(&self) -> &'static str {
        match self {
            Self::Running { .. } => "refreshing",
            Self::Refreshed { .. } => "refreshed",
            Self::Failed { .. } => "failed",
        }
    }

    /// The skill this is about, so a page polling can tell its own answer from another skill's.
    pub fn name(&self) -> &str {
        match self {
            Self::Running { name } | Self::Refreshed { name, .. } | Self::Failed { name, .. } => {
                name
            }
        }
    }

    /// The status as the route returns it.
    ///
    /// Every key is present in every state, `null` where it does not apply: the page reads one
    /// field and never has to decide whether a missing key means "fine" or "not asked".
    pub fn body(&self) -> Value {
        json!({
            "name": self.name(),
            "state": self.state(),
            "version": match self {
                Self::Refreshed { version, .. } => version.clone(),
                _ => None,
            },
            "replaced": match self {
                Self::Refreshed { replaced, .. } => Some(*replaced),
                _ => None,
            },
            "detail": match self {
                Self::Failed { detail, .. } => Some(detail.clone()),
                _ => None,
            },
        })
    }
}

/// What came of asking for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// A worker took it.
    Started,
    /// One is already running, and that one is named: the state file is one file.
    Busy {
        /// The skill in flight.
        name: String,
    },
}

/// The queue: what the panel asked for, and what came of it.
pub struct Refresher {
    home: PathBuf,
    publisher: Arc<dyn Publisher>,
    releases: Arc<dyn Releases>,
    queue: Mutex<Queue>,
}

/// The queue's own state, guarded all at once: who is running and what the last one came to are
/// read together or not at all, because a page that saw "nothing running" and then the previous
/// outcome would report somebody else's refresh as its own.
#[derive(Default)]
struct Queue {
    running: Option<String>,
    last: Option<Status>,
}

impl Refresher {
    /// The queue for the skills under `home`, published by `publisher`, checked against `releases`.
    ///
    /// Both are seams for the reason every other seam in this workspace is: the interesting
    /// behaviour is what happens *after* a program has run and *after* a network answered, and a
    /// test that ran either would measure this machine's network and its installed packages.
    pub fn new(home: PathBuf, publisher: Arc<dyn Publisher>, releases: Arc<dyn Releases>) -> Self {
        Self {
            home,
            publisher,
            releases,
            queue: Mutex::new(Queue::default()),
        }
    }

    /// Ask for `name` to be rewritten, and return at once.
    pub fn enqueue(self: &Arc<Self>, name: &str) -> Started {
        {
            let mut queue = self.queue();
            if let Some(running) = &queue.running {
                return Started::Busy {
                    name: running.clone(),
                };
            }
            queue.running = Some(name.to_string());
            queue.last = Some(Status::Running {
                name: name.to_string(),
            });
        }

        let queue = Arc::clone(self);
        let name = name.to_string();
        // A thread per refresh, and the guard above is what makes a second request a refusal
        // rather than a second writer. Its handle is dropped deliberately: nothing waits for a
        // refresh, because waiting is what this exists to avoid — the status is how its outcome is
        // read.
        std::thread::spawn(move || queue.run(name));
        Started::Started
    }

    /// What the last (or running) refresh came to, or `None` when nothing has been asked for.
    pub fn status(&self) -> Option<Status> {
        self.queue().last.clone()
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        // A poisoned lock here means a worker panicked while holding it, which is not a reason to
        // lose the queue: the data is a name and a status, and the next write replaces both.
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn run(self: Arc<Self>, name: String) {
        let status = self.refresh_once(&name);
        let mut queue = self.queue();
        queue.running = None;
        queue.last = Some(status);
    }

    /// One refresh: the file, then the version, then the state file that records both.
    fn refresh_once(&self, name: &str) -> Status {
        let root = deepnest_skills::user_root(&self.home);
        let catalog = catalog::load(&root);
        let Some(entry) = catalog.get(name) else {
            return Status::Failed {
                name: name.to_string(),
                detail: format!(
                    "the catalog names no skill called `{name}`, so there is nothing to publish it"
                ),
            };
        };

        let state_path = deepnest_skills::state::path(&root);
        let mut state = State::read(&state_path);
        let at = deepnest_agent::session::now_iso();

        // The file first. `install` refuses a skill deepnest did not put there *before* it runs the
        // publisher, so a file the user wrote costs no child process; and a refusal leaves the state
        // untouched, because nothing was written to record.
        //
        // The home handed over is deepnest's own, which is what the catalog's `~/skills/` resolves
        // against and the reason the result lands where `user_root` above reads it. The user's home
        // would put the file in a directory nothing loads, and under a relocated `DEEPNEST_HOME` it
        // would reach outside the throwaway home entirely.
        let placed =
            match installer::install(entry, &self.home, &mut state, self.publisher.as_ref(), &at) {
                Outcome::Installed(placed) => placed,
                refused => {
                    return Status::Failed {
                        name: name.to_string(),
                        detail: refused.to_string(),
                    }
                }
            };

        // The version, which only the check can learn. It is asked after the file has been rewritten
        // because the refresh is what the user asked for and a forge that cannot answer must not be
        // able to cancel it — the version is the part that can fail, not the file.
        let adopted = release::check(entry, state.get(name), self.releases.as_ref())
            .adoptable()
            .map(ToString::to_string);
        if let Some(version) = &adopted {
            // Cannot refuse: the install above has just recorded this name as managed, which is the
            // one condition `record_update` checks.
            state.record_update(name, Some(version.clone()), &at);
        }

        if let Err(error) = state.write(&state_path) {
            // The file is on disk and the record is not, which is the safe direction `install`
            // already chose for an interrupted run; said out loud rather than reported as a refresh.
            return Status::Failed {
                name: name.to_string(),
                detail: format!(
                    "the skill was rewritten at {} but the state file could not be updated: {error}",
                    placed.path.display()
                ),
            };
        }

        Status::Refreshed {
            name: name.to_string(),
            version: adopted.or_else(|| state.get(name).and_then(|i| i.version.clone())),
            replaced: placed.refreshed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use deepnest_skills::installer::Report;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-refresh-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let root = deepnest_skills::user_root(&base);
            std::fs::create_dir_all(&root).unwrap();
            // The catalog the binary actually ships, so the entry being refreshed is the real one
            // rather than a fixture that could drift from it.
            std::fs::write(
                deepnest_skills::catalog::path(&root),
                deepnest_skills::BUNDLED,
            )
            .unwrap();
            Self { base }
        }

        fn root(&self) -> PathBuf {
            deepnest_skills::user_root(&self.base)
        }

        fn state_path(&self) -> PathBuf {
            deepnest_skills::state::path(&self.root())
        }

        /// The state file as it is on disk now — the reading the next check gets, not this
        /// process's copy.
        fn state(&self) -> State {
            State::read(&self.state_path())
        }

        /// A skill deepnest installed, at `version` when the test says one is known.
        fn installed(&self, name: &str, version: Option<&str>) {
            let dir = self.root().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(deepnest_skills::SKILL_FILE),
                skill_text(name, "drives a browser"),
            )
            .unwrap();
            let mut state = State::new();
            state.record_install(name, version.map(str::to_string), "2026-10-31T00:00:00Z");
            state.write(&self.state_path()).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn skill_text(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\nDrive the browser.\n")
    }

    /// A publisher that prints a usable skill and counts how often it ran.
    struct Fake {
        calls: AtomicUsize,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Publisher for Fake {
        fn publish(&self, program: &str) -> io::Result<Report> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Report {
                status: Some(0),
                // A description the seeded file does not have, so a test can tell a rewritten file
                // from one that was left alone.
                stdout: skill_text(program, "published again"),
                stderr: String::new(),
            })
        }
    }

    struct Answers(&'static str);

    impl Releases for Answers {
        fn latest(&self, _repo: &str) -> io::Result<String> {
            Ok(self.0.to_string())
        }
    }

    fn refresher(
        scratch: &Scratch,
        publisher: Arc<dyn Publisher>,
        releases: Arc<dyn Releases>,
    ) -> Arc<Refresher> {
        Arc::new(Refresher::new(scratch.base.clone(), publisher, releases))
    }

    /// Wait for the queue to stop saying `refreshing`.
    fn settle(refresher: &Refresher) -> Status {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match refresher.status() {
                Some(status) if !matches!(status, Status::Running { .. }) => return status,
                other => {
                    assert!(
                        Instant::now() < deadline,
                        "the refresh never reported: {other:?}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    #[test]
    fn a_refresh_records_the_version_the_check_adopted() {
        let scratch = Scratch::new("adopt");
        scratch.installed("browser-harness", Some("0.1.0"));
        let refresher = refresher(
            &scratch,
            Fake::new(),
            Arc::new(Answers(r#"{"tag_name":"v0.4.0"}"#)),
        );

        assert_eq!(refresher.enqueue("browser-harness"), Started::Started);
        let status = settle(&refresher);
        assert_eq!(status.state(), "refreshed", "{status:?}");
        assert_eq!(status.name(), "browser-harness");

        // The reading that matters: the file the next check reads, not the copy this process held.
        let recorded = scratch.state();
        let entry = recorded.get("browser-harness").unwrap();
        assert_eq!(
            entry.version.as_deref(),
            Some("0.4.0"),
            "the adopted version never reached the state file"
        );
        assert_eq!(
            entry.installed_at.as_deref(),
            Some("2026-10-31T00:00:00Z"),
            "a refresh is not a new arrival"
        );
        assert!(entry.updated_at.is_some(), "no refresh was recorded");
        assert!(entry.managed, "a refresh must not lose provenance");
    }

    #[test]
    fn the_file_is_rewritten_from_the_package_that_publishes_it() {
        let scratch = Scratch::new("rewritten");
        scratch.installed("browser-harness", Some("0.1.0"));
        let refresher = refresher(
            &scratch,
            Fake::new(),
            Arc::new(Answers(r#"{"tag_name":"v0.4.0"}"#)),
        );

        refresher.enqueue("browser-harness");
        settle(&refresher);

        let text = std::fs::read_to_string(
            scratch
                .root()
                .join("browser-harness")
                .join(deepnest_skills::SKILL_FILE),
        )
        .unwrap();
        assert!(
            text.contains("published again"),
            "the file on disk is still the old one: {text}"
        );
    }

    #[test]
    fn a_skill_installed_without_a_version_becomes_comparable_on_its_first_refresh() {
        // The verdict here is `Unknown`, not `Newer`: nothing is recorded about what is on disk.
        // Adopting the published version is what makes the next check answerable at all, and it is
        // the reason a refresh is offered for that state rather than only for a newer release.
        let scratch = Scratch::new("comparable");
        scratch.installed("browser-harness", None);
        let refresher = refresher(
            &scratch,
            Fake::new(),
            Arc::new(Answers(r#"{"tag_name":"v0.4.0"}"#)),
        );

        refresher.enqueue("browser-harness");
        let status = settle(&refresher);
        assert_eq!(status.state(), "refreshed", "{status:?}");
        assert_eq!(
            scratch
                .state()
                .get("browser-harness")
                .unwrap()
                .version
                .as_deref(),
            Some("0.4.0")
        );
    }

    #[test]
    fn an_answer_the_forge_could_not_give_keeps_the_version_it_had() {
        // The file is still refreshed — that is what was asked for — and the version is neither
        // invented nor cleared, which is the state model's answer to "the source did not say".
        let scratch = Scratch::new("unreachable");
        scratch.installed("browser-harness", Some("0.1.0"));
        let refresher = refresher(
            &scratch,
            Fake::new(),
            Arc::new(Answers("<html>a proxy's page</html>")),
        );

        refresher.enqueue("browser-harness");
        let status = settle(&refresher);
        assert_eq!(status.state(), "refreshed", "{status:?}");
        assert!(
            matches!(&status, Status::Refreshed { version, .. } if version.as_deref() == Some("0.1.0")),
            "the refresh reported a version it could not have learned: {status:?}"
        );
        assert_eq!(
            scratch
                .state()
                .get("browser-harness")
                .unwrap()
                .version
                .as_deref(),
            Some("0.1.0"),
            "a check that could not answer cleared the version"
        );
    }

    #[test]
    fn a_skill_the_user_wrote_is_refused_before_any_program_runs() {
        let scratch = Scratch::new("user-skill");
        let dir = scratch.root().join("browser-harness");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(deepnest_skills::SKILL_FILE),
            skill_text("browser-harness", "mine"),
        )
        .unwrap();
        let mut state = State::new();
        state.record_user_skill("browser-harness");
        state.write(&scratch.state_path()).unwrap();

        let publisher = Fake::new();
        let refresher = refresher(
            &scratch,
            Arc::clone(&publisher) as Arc<dyn Publisher>,
            Arc::new(Answers(r#"{"tag_name":"v0.4.0"}"#)),
        );

        refresher.enqueue("browser-harness");
        let status = settle(&refresher);
        assert_eq!(status.state(), "failed", "{status:?}");
        assert!(
            matches!(&status, Status::Failed { detail, .. }
                if detail.contains("did not put it there")),
            "the refusal does not say why: {status:?}"
        );
        assert_eq!(
            publisher.calls(),
            0,
            "the publisher ran for a file that is not ours"
        );
        // And the user's file is exactly as it was.
        let text = std::fs::read_to_string(
            scratch
                .root()
                .join("browser-harness")
                .join(deepnest_skills::SKILL_FILE),
        )
        .unwrap();
        assert!(text.contains("mine"), "{text}");
    }

    #[test]
    fn a_name_the_catalog_does_not_hold_is_refused_by_name() {
        let scratch = Scratch::new("unknown-name");
        let publisher = Fake::new();
        let refresher = refresher(
            &scratch,
            Arc::clone(&publisher) as Arc<dyn Publisher>,
            Arc::new(Answers(r#"{"tag_name":"v0.4.0"}"#)),
        );

        refresher.enqueue("no-such-skill");
        let status = settle(&refresher);
        assert!(
            matches!(&status, Status::Failed { detail, .. } if detail.contains("no-such-skill")),
            "{status:?}"
        );
        assert_eq!(publisher.calls(), 0);
    }

    #[test]
    fn a_second_ask_while_a_refresh_runs_is_refused_rather_than_run_beside_it() {
        // The reason this is a refusal and not a queue: the state file is one file, and two workers
        // would each write their own record over the other's.
        let scratch = Scratch::new("busy");
        scratch.installed("browser-harness", Some("0.1.0"));

        struct Gated {
            gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
            calls: AtomicUsize,
        }

        impl Publisher for Gated {
            fn publish(&self, program: &str) -> io::Result<Report> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let (lock, cvar) = &*self.gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = cvar.wait(open).unwrap();
                }
                Ok(Report {
                    status: Some(0),
                    stdout: skill_text(program, "published again"),
                    stderr: String::new(),
                })
            }
        }

        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let publisher = Arc::new(Gated {
            gate: Arc::clone(&gate),
            calls: AtomicUsize::new(0),
        });
        let refresher = refresher(
            &scratch,
            Arc::clone(&publisher) as Arc<dyn Publisher>,
            Arc::new(Answers(r#"{"tag_name":"v0.4.0"}"#)),
        );

        assert_eq!(refresher.enqueue("browser-harness"), Started::Started);
        // Wait until the worker is provably inside the publisher: a second ask sent before the
        // thread was scheduled would prove nothing about one that arrives during a refresh.
        let deadline = Instant::now() + Duration::from_secs(20);
        while publisher.calls.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "the worker never reached the publisher"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(
            refresher.enqueue("browser-harness"),
            Started::Busy {
                name: "browser-harness".to_string()
            }
        );
        assert_eq!(
            publisher.calls.load(Ordering::SeqCst),
            1,
            "a second publish ran beside the first"
        );

        let (lock, cvar) = &*gate;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
        assert_eq!(settle(&refresher).state(), "refreshed");
    }

    #[test]
    fn nothing_asked_for_has_no_status_to_report() {
        let scratch = Scratch::new("idle");
        let refresher = refresher(
            &scratch,
            Fake::new(),
            Arc::new(Answers(r#"{"tag_name":"v0.4.0"}"#)),
        );
        assert_eq!(refresher.status(), None);
    }
}
