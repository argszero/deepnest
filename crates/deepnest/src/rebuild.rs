//! Rebuilding an app after a turn wrote to it, without holding the turn's connection.
//!
//! The loop this closes: a person asks the agent for a change, the agent writes Rust, and the
//! browser keeps showing the old binary. `crate::chat` says exactly that — "a writing turn does not
//! rebuild the app" — and the reason it gave is the reason this module exists: a build inside the
//! chat request would hold one HTTP connection for a minute of `bun install`, longer than any
//! browser waits. So the turn **enqueues** and returns, and the work happens on a thread of its own.
//!
//! **Three rules, and each is a way the naive version fails:**
//!
//! - **One build per app at a time.** Two turns in a row would otherwise start two builds in the
//!   same directory, and the second would find the first's `target/` half-written. A request that
//!   arrives during a build does not start one: it bumps the generation, and the build already
//!   running will see that it is out of date and go round again.
//! - **A superseded build never restarts the app.** The build that finishes second is the one whose
//!   binary is on disk, so a stale one that restarted anyway would put the older binary back and
//!   the page would reload onto the change before last — a race that looks like "the agent's edits
//!   vanish sometimes".
//! - **A failure is a status, not a lost message.** The compiler's last lines travel back through
//!   [`Rebuilder::status`] to the page and through [`Rebuilder::note`] to the agent's next turn, so
//!   the person sees why nothing changed and the agent does not answer as if it had worked.
//!
//! What it does **not** do: it does not decide *whether* to build (that is the turn's `changed`
//! flag), and it does not start an app that was never running unless a build succeeded — a failed
//! build leaves whatever is serving alone, which is the whole point of not restarting on failure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use deepnest_apps::{build, build_log_path, log_path, tail, BuildError, Runner, Slug, Supervisor};
use serde_json::{json, Value};

/// The route this module answers, and the spelling the router routes on.
///
/// Named here rather than spelled as a literal in three places, because three do read it: the
/// router's parser, the cookie that lets a page watch its own build, and the page itself.
pub const ACTION: &str = "build";

/// How much of a failed build's log travels with the status the page polls.
///
/// A compiler's first line is the summary and its last lines are the detail; forty is enough for a
/// `rustc` diagnostic with its notes and small enough that a page can show it without scrolling.
pub const TAIL_LINES: usize = 40;

/// How much of that log is handed to the agent's next turn.
///
/// Smaller than the page's share on purpose: the agent gets the diagnosis it needs to fix the
/// source, and the person gets the whole thing.
pub const NOTE_LINES: usize = 20;

/// What is true about an app's last rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Nothing has asked for a build since this process started.
    Idle,
    /// A build is running now.
    Building { generation: u64 },
    /// The newest build finished, the app was restarted, and it is serving that binary.
    Ok { generation: u64 },
    /// The newest build failed. `detail` is the tail of the build log.
    Failed {
        generation: u64,
        step: Option<String>,
        detail: String,
        log: PathBuf,
    },
}

impl Status {
    /// The word the page switches on.
    pub fn state(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Building { .. } => "building",
            Self::Ok { .. } => "ok",
            Self::Failed { .. } => "failed",
        }
    }

    fn generation(&self) -> u64 {
        match self {
            Self::Idle => 0,
            Self::Building { generation } | Self::Ok { generation } => *generation,
            Self::Failed { generation, .. } => *generation,
        }
    }
}

/// The queue: what each app's rebuild is doing, and the generation that decides who wins.
pub struct Rebuilder {
    supervisor: Arc<Supervisor>,
    builder: Arc<dyn Runner>,
    apps_dir: PathBuf,
    home: PathBuf,
    jobs: Mutex<HashMap<String, Job>>,
}

/// One app's queue entry. Guarded by [`Rebuilder::jobs`], all of it at once: the generation, the
/// flag that says a worker owns this app, and the last status are read together or not at all.
#[derive(Default)]
struct Job {
    /// Bumped by every request. A worker compares it with the one it started on.
    generation: u64,
    /// Whether a worker thread is running for this app right now.
    running: bool,
    status: Option<Status>,
}

impl Rebuilder {
    /// The queue for the apps in `apps_dir`, restarted through `supervisor`.
    pub fn new(
        supervisor: Arc<Supervisor>,
        builder: Arc<dyn Runner>,
        apps_dir: PathBuf,
        home: PathBuf,
    ) -> Self {
        Self {
            supervisor,
            builder,
            apps_dir,
            home,
            jobs: Mutex::new(HashMap::new()),
        }
    }

    /// Ask for a rebuild of `slug`, and return at once.
    ///
    /// The return value is the generation this request became. A request that arrives while a build
    /// is running starts nothing: it bumps the generation, which is what tells the running worker
    /// that its result is already out of date.
    pub fn enqueue(self: &Arc<Self>, slug: &Slug) -> u64 {
        let (generation, spawn) = {
            let mut jobs = self.jobs();
            let job = jobs.entry(slug.as_str().to_string()).or_default();
            job.generation += 1;
            let generation = job.generation;
            if job.running {
                (generation, false)
            } else {
                job.running = true;
                job.status = Some(Status::Building { generation });
                (generation, true)
            }
        };

        if spawn {
            let queue = Arc::clone(self);
            let slug = slug.clone();
            // A thread per app, and only one at a time per app: the guard above is what makes the
            // second request a generation bump rather than a second build. Its handle is dropped
            // deliberately — nothing waits for a build, because waiting is what this exists to
            // avoid; the status is how its outcome is read.
            std::thread::spawn(move || queue.run(slug));
        }
        generation
    }

    /// What the last (or running) rebuild is doing.
    pub fn status(&self, slug: &Slug) -> Status {
        self.jobs()
            .get(slug.as_str())
            .and_then(|job| job.status.clone())
            .unwrap_or(Status::Idle)
    }

    /// The failure the agent should know about before its next turn, if the last build failed.
    ///
    /// This is how a broken build reaches the model: the turn after a failed rebuild is told what
    /// the compiler said, so "the app does not build because `x` moved" is answered by fixing `x`
    /// rather than by writing more code on top of a tree that does not compile.
    pub fn note(&self, slug: &Slug) -> Option<String> {
        let Status::Failed { step, detail, .. } = self.status(slug) else {
            return None;
        };
        let named = match &step {
            Some(step) => format!("the `{step}` step"),
            None => "a build step".to_string(),
        };
        Some(format!(
            "# The last build of this app failed\n\n{named} failed, so the app is still running the \
             binary from the build before it. What the build said:\n\n```\n{detail}\n```\n\nFix the \
             source before adding anything new; the page reloads by itself once a build succeeds."
        ))
    }

    fn jobs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Job>> {
        // A poisoned lock here means a worker panicked while holding it, which is not a reason to
        // lose the queue: the data is a generation and a status, and the next write replaces both.
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Build until the newest request has been built, then restart the app.
    ///
    /// The loop is what makes "one build at a time" and "the newest one wins" true together: a
    /// worker that finds the generation moved on does not restart the app with its own (older)
    /// binary, it builds again.
    fn run(self: Arc<Self>, slug: Slug) {
        let dir = self.apps_dir.join(slug.as_str());
        loop {
            let generation = {
                let mut jobs = self.jobs();
                let job = jobs.entry(slug.as_str().to_string()).or_default();
                job.status = Some(Status::Building {
                    generation: job.generation,
                });
                job.generation
            };

            let status = self.build_once(&slug, &dir, generation);

            let mut jobs = self.jobs();
            let job = jobs.entry(slug.as_str().to_string()).or_default();
            if job.generation != generation {
                // A newer request arrived while this build ran. Restarting now would put this
                // binary — built from older source — in front of the page, so go round again
                // instead. `running` stays true, so nothing else starts a worker meanwhile.
                continue;
            }
            job.status = Some(status);
            job.running = false;
            return;
        }
    }

    /// One build, and the restart that follows a successful one.
    fn build_once(&self, slug: &Slug, dir: &Path, generation: u64) -> Status {
        let log = build_log_path(&self.home, slug);
        match build(dir, log.clone(), self.builder.as_ref()) {
            Ok(_built) => {
                // The build only counts once the app is serving it: a binary the supervisor cannot
                // start is not a successful rebuild, and reporting `ok` would reload the page onto
                // a stopped app.
                let app_log = log_path(&self.home, slug);
                match self.supervisor.restart(slug, dir, app_log) {
                    Ok(_state) => Status::Ok { generation },
                    Err(error) => Status::Failed {
                        generation,
                        step: Some("restart".to_string()),
                        detail: format!(
                            "the build succeeded but the app could not be restarted: {error}"
                        ),
                        log,
                    },
                }
            }
            Err(error) => {
                let step = match &error {
                    BuildError::Failed { step, .. } => Some(step.to_string()),
                    _ => None,
                };
                Status::Failed {
                    generation,
                    step,
                    // The log's own tail rather than the error's one-line summary: what a person
                    // needs is the compiler's sentence about their code.
                    detail: tail(&log, TAIL_LINES)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| error.to_string()),
                    log,
                }
            }
        }
    }
}

impl Status {
    /// The status as the route returns it.
    pub fn body(&self, slug: &Slug) -> Value {
        match self {
            Self::Idle => json!({ "slug": slug.as_str(), "state": self.state() }),
            Self::Building { .. } | Self::Ok { .. } => json!({
                "slug": slug.as_str(),
                "state": self.state(),
                "generation": self.generation(),
            }),
            Self::Failed {
                step, detail, log, ..
            } => json!({
                "slug": slug.as_str(),
                "state": self.state(),
                "generation": self.generation(),
                "step": step,
                "detail": detail,
                "log": log.display().to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use deepnest_apps::{Handle, Outcome, Port, Prober, Spawner, Spec, Step};

    /// A runner that answers the same way every time and counts how often it was asked.
    struct Scripted {
        fails: bool,
        runs: AtomicUsize,
    }

    impl Scripted {
        fn succeeding() -> Arc<Self> {
            Arc::new(Self {
                fails: false,
                runs: AtomicUsize::new(0),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                fails: true,
                runs: AtomicUsize::new(0),
            })
        }
    }

    impl Runner for Scripted {
        fn run(&self, _step: &Step, log: &std::path::Path) -> std::io::Result<Outcome> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            if self.fails {
                // The log is what the status reports, so a fake that wrote nothing would let a
                // status carrying no tail pass as one that carried the compiler's sentence.
                let _ = std::fs::write(log, "error[E0425]: cannot find value `x` in this scope\n");
                return Ok(Outcome {
                    code: Some(1),
                    detail: "exited with 1".to_string(),
                });
            }
            Ok(Outcome {
                code: Some(0),
                detail: "exited with 0".to_string(),
            })
        }
    }

    struct FakeSpawner;

    impl Spawner for FakeSpawner {
        fn spawn(&self, _spec: &Spec) -> std::io::Result<Box<dyn Handle>> {
            Ok(Box::new(NeverExits))
        }
    }

    struct NeverExits;

    impl Handle for NeverExits {
        fn id(&self) -> u32 {
            4242
        }

        fn poll(&mut self) -> std::io::Result<Option<deepnest_apps::Exit>> {
            Ok(None)
        }

        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct AlwaysReady;

    impl Prober for AlwaysReady {
        fn ready(&self, _port: Port) -> bool {
            true
        }
    }

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-rebuild-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let app = base.join("apps").join("notes");
            std::fs::create_dir_all(&app).unwrap();
            // The build refuses a directory that is not an app before it runs a step, and it
            // insists a successful build leaves a binary behind, so both are here: without them
            // these tests would measure the refusals rather than the queue.
            std::fs::write(
                app.join("deepnest.toml"),
                "[app]\nname = \"Notes\"\ntemplate = 1\n",
            )
            .unwrap();
            let binary = app.join(deepnest_apps::EXPECTED_BINARY);
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, b"not really a program").unwrap();
            Self { base }
        }

        fn apps(&self) -> PathBuf {
            self.base.join("apps")
        }

        fn slug(&self) -> Slug {
            Slug::parse("notes").unwrap()
        }
    }

    fn queue(scratch: &Scratch, runner: Arc<dyn Runner>) -> Arc<Rebuilder> {
        let supervisor = Arc::new(Supervisor::with_parts(
            Box::new(FakeSpawner),
            Box::new(AlwaysReady),
        ));
        Arc::new(Rebuilder::new(
            supervisor,
            runner,
            scratch.apps(),
            scratch.base.clone(),
        ))
    }

    /// Wait for the status to stop saying `building`.
    fn settle(queue: &Rebuilder, slug: &Slug) -> Status {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let status = queue.status(slug);
            if !matches!(status, Status::Building { .. }) {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the build never finished: {status:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn nothing_asked_for_is_idle_rather_than_a_guess() {
        let scratch = Scratch::new("idle");
        let queue = queue(&scratch, Scripted::succeeding());
        assert_eq!(queue.status(&scratch.slug()), Status::Idle);
        assert_eq!(queue.note(&scratch.slug()), None);
    }

    #[test]
    fn a_successful_build_restarts_the_app_and_says_so() {
        let scratch = Scratch::new("ok");
        let queue = queue(&scratch, Scripted::succeeding());
        let slug = scratch.slug();

        assert_eq!(queue.enqueue(&slug), 1);
        assert_eq!(settle(&queue, &slug), Status::Ok { generation: 1 });
        assert_eq!(queue.note(&slug), None, "a good build is not news");
    }

    #[test]
    fn a_failed_build_reports_the_log_and_hands_it_to_the_next_turn() {
        let scratch = Scratch::new("failed");
        let queue = queue(&scratch, Scripted::failing());
        let slug = scratch.slug();

        queue.enqueue(&slug);
        let status = settle(&queue, &slug);
        let Status::Failed { step, detail, .. } = status else {
            panic!("expected a failure, got {status:?}");
        };
        assert_eq!(step.as_deref(), Some("bun install"));
        // The log's own last lines, not the error's summary: this is what the page shows.
        assert!(detail.contains("cannot find value `x`"), "{detail}");

        let note = queue.note(&slug).expect("a failure is news");
        assert!(note.contains("cannot find value `x`"), "{note}");
        assert!(note.contains("still running the binary"), "{note}");
    }

    #[test]
    fn asking_twice_during_a_build_starts_one_build_and_supersedes_with_the_newest() {
        let scratch = Scratch::new("supersede");
        // A runner that holds its first step until the test releases it, so the second request
        // provably arrives while the first build is still running.
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let runs = Arc::new(AtomicUsize::new(0));

        struct Gated {
            gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
            runs: Arc<AtomicUsize>,
        }

        impl Runner for Gated {
            fn run(&self, _step: &Step, log: &std::path::Path) -> std::io::Result<Outcome> {
                let n = self.runs.fetch_add(1, Ordering::SeqCst);
                let _ = std::fs::write(log, format!("step {n}\n"));
                if n == 0 {
                    let (lock, cvar) = &*self.gate;
                    let mut open = lock.lock().unwrap();
                    while !*open {
                        open = cvar.wait(open).unwrap();
                    }
                }
                Ok(Outcome {
                    code: Some(0),
                    detail: "exited with 0".to_string(),
                })
            }
        }

        let queue = queue(
            &scratch,
            Arc::new(Gated {
                gate: Arc::clone(&gate),
                runs: Arc::clone(&runs),
            }),
        );
        let slug = scratch.slug();

        let first_asked = queue.enqueue(&slug);
        assert_eq!(first_asked, 1);
        // The worker is a thread, so wait until it is provably inside the first step: a second
        // request sent before the thread was scheduled would prove nothing about a request that
        // arrives during a build.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while runs.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the worker never reached its first step"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let second = queue.enqueue(&slug);
        assert_eq!(second, 2, "each request takes a generation");
        // The second request must not have started a build of its own: one worker owns the app, and
        // it is still inside the first step.
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Release the first build. Its generation is now stale, so it must not announce `ok`; the
        // worker has to go round again on the newer request.
        let (lock, cvar) = &*gate;
        *lock.lock().unwrap() = true;
        cvar.notify_all();

        assert_eq!(
            settle(&queue, &slug),
            Status::Ok { generation: 2 },
            "the newest request is the one that reports"
        );
        assert!(
            runs.load(Ordering::SeqCst) > 1,
            "a superseded build must be built again, not announced"
        );
    }
}
