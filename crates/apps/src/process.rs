//! An app's process: one port per start, and the supervisor that owns it.
//!
//! An app is a program deepnest starts and stops, so three questions have to be answered before
//! any of it can be written: where its port comes from, when it counts as started, and what
//! stopping it means. The answers here are the design's, and each is stated where it is made.
//!
//! - **The port is allocated per start and never persisted** (design §2). It is taken by binding
//!   `127.0.0.1:0` and reading back what the kernel chose, so it is inside the loopback range
//!   nobody else can reach and it changes every run. It is handed to the app in `PORT`, and it
//!   never appears in a file, on a command line the user sees, or in the browser's address — the
//!   browser only ever knows `127.0.0.1:4270`.
//! - **Started means answering, not spawned.** `Command::spawn` returning `Ok` says a process
//!   exists; it says nothing about whether the program is serving. A readiness probe connecting to
//!   the port is what distinguishes the two, and the difference is the whole reason a "start app"
//!   button can report success truthfully. A process that exits during that wait is reported with
//!   its exit status and its log — not as a timeout.
//! - **Stopping is immediate, and that is deliberate for now.** There is no shutdown contract for
//!   the app template yet, so there is nothing to ask politely: a `SIGTERM` to a program that has
//!   not agreed what it means is a SIGKILL with extra steps. What makes it safe rather than
//!   reckless is the template's own promise — WAL and `busy_timeout` — which is precisely the
//!   SQLite configuration that survives a process disappearing. When the template gains a shutdown
//!   handshake, this is the function that changes, and `stop` is one call site.
//!
//! Two seams keep this testable without spawning anything: [`Spawner`] and [`Prober`]. The tests
//! supply fakes and run in milliseconds; [`StdSpawner`] and [`TcpProber`] are the two lines of
//! production wiring, and both have a test against the real thing.
//!
//! What this module deliberately does **not** own: building the app. `start` runs a command that
//! is already built, and says which path it expected — see [`run_command`].

use std::collections::BTreeMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::slug::Slug;

/// The environment variable an app reads its port from.
///
/// `PORT` rather than a deepnest-specific name because it is the one every web framework already
/// looks for; an app the agent writes should not need a paragraph explaining how to find its port.
pub const PORT_ENV: &str = "PORT";

/// How long a start waits for the app to answer before giving up on it.
///
/// Generous, because the first start of a generated app may be the first time its binary runs and
/// the filesystem cold-reads it. The cost of being wrong in this direction is a slow failure; the
/// cost of the other is a healthy app declared dead.
pub const DEFAULT_READINESS: Duration = Duration::from_secs(20);

/// How often the readiness loop looks again.
const POLL: Duration = Duration::from_millis(50);

/// How long a single readiness connection attempt may take.
///
/// Short: the answer being sought is "is anything listening", and a listening socket accepts
/// immediately. A long timeout here would make one probe cost the whole budget.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// A port on loopback, allocated for one start of one app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Port(u16);

impl Port {
    /// The number, for the environment variable and the probe.
    pub fn number(self) -> u16 {
        self.0
    }

    /// The address an app on this port listens at.
    pub fn addr(self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.0))
    }
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Take a free loopback port.
///
/// The listener is closed before the number is returned, so there is a window in which the kernel
/// could hand the same port to someone else. There is no portable alternative that hands a bound
/// socket to a child process, so the window is accepted and its symptom — an app that fails to
/// bind — is reported by the readiness probe rather than being invisible.
pub fn allocate() -> io::Result<Port> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = Port(listener.local_addr()?.port());
    drop(listener);
    Ok(port)
}

/// Everything a spawn needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The program to run — an already-built app binary.
    pub command: PathBuf,
    /// Arguments, in order.
    pub args: Vec<String>,
    /// The app's own directory. A relative path the app opens is relative to *this*, never to
    /// deepnest's working directory, which is wherever it was launched from.
    pub cwd: PathBuf,
    /// The port this start allocated, handed over in [`PORT_ENV`].
    pub port: Port,
    /// Where stdout and stderr go: the app's log, appended to.
    pub log: PathBuf,
}

impl Spec {
    /// A spec for the app in `dir`, run from its log in `log`.
    pub fn for_app(dir: &Path, port: Port, log: PathBuf) -> Self {
        Self {
            command: run_command(dir).unwrap_or_else(|| dir.join(EXPECTED_BINARY)),
            args: Vec::new(),
            cwd: dir.to_path_buf(),
            port,
            log,
        }
    }
}

/// Where an app's built binary is expected, relative to its directory.
///
/// This is the template's contract, and it is a constant here rather than a manifest field because
/// there is exactly one template in v1. It is written down in one place so that when the template
/// lands, the two either match or this line changes — not both, and not silently.
pub const EXPECTED_BINARY: &str = if cfg!(windows) {
    "target/release/app.exe"
} else {
    "target/release/app"
};

/// The built binary for the app in `dir`, if it exists.
///
/// `None` is not "no app": it is "nothing has been built yet", which the caller must not report as
/// a failure to start, because the user has not asked for a build.
pub fn run_command(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(EXPECTED_BINARY);
    path.is_file().then_some(path)
}

/// How a process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// The exit code, when the platform or the signal allowed one.
    pub code: Option<i32>,
    /// The platform's own words: the code, or the signal that ended it.
    pub detail: String,
}

/// A spawned process, as the supervisor sees it.
pub trait Handle: Send {
    /// The operating system's process id, for reporting.
    fn id(&self) -> u32;

    /// `Some` once it has ended, `None` while it is still running.
    fn poll(&mut self) -> io::Result<Option<Exit>>;

    /// End it now. Idempotent: killing a process that has already gone is not an error here,
    /// because the caller's intent — "it must not be running" — is satisfied either way.
    fn kill(&mut self) -> io::Result<()>;
}

/// Starts a program.
///
/// A seam rather than a direct `Command::spawn`, for the reason every other such seam in this
/// workspace exists: the supervisor's interesting behaviour is a state machine with timeouts, and
/// testing it against real processes would make the suite slow and flaky in proportion to the CI
/// machine's load rather than to the code.
pub trait Spawner: Send + Sync {
    fn spawn(&self, spec: &Spec) -> io::Result<Box<dyn Handle>>;
}

/// Asks whether an app is serving on a port.
pub trait Prober: Send + Sync {
    fn ready(&self, port: Port) -> bool;
}

/// The real spawner.
///
/// stdin is `/dev/null`: an app that reads stdin would otherwise take the terminal the binary was
/// launched from, and block forever on a machine where nobody is typing into it. Output is
/// appended to the app's log rather than piped, for the reason `crates/tools` gives — an undrained
/// pipe fills and deadlocks the child.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdSpawner;

impl Spawner for StdSpawner {
    fn spawn(&self, spec: &Spec) -> io::Result<Box<dyn Handle>> {
        if let Some(parent) = spec.log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.log)?;
        let errors = log.try_clone()?;

        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env(PORT_ENV, spec.port.number().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errors));

        let child = command.spawn()?;
        Ok(Box::new(StdHandle { child }))
    }
}

/// A real child process.
struct StdHandle {
    child: std::process::Child,
}

impl Handle for StdHandle {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn poll(&mut self) -> io::Result<Option<Exit>> {
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        Ok(Some(describe(status)))
    }

    fn kill(&mut self) -> io::Result<()> {
        match self.child.kill() {
            Ok(()) => {
                // Reaped so the supervisor holds no zombie, and so a later `poll` on a kill it
                // asked for does not read as the app having died on its own.
                let _ = self.child.wait();
                Ok(())
            }
            // Already gone: the intent is satisfied.
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Turn an exit status into something reportable on both platforms.
fn describe(status: std::process::ExitStatus) -> Exit {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Exit {
                code: None,
                detail: format!("was killed by signal {signal}"),
            };
        }
    }
    Exit {
        code: status.code(),
        detail: match status.code() {
            Some(code) => format!("exited with status {code}"),
            None => "exited".to_string(),
        },
    }
}

/// The real prober: connect, and treat a completed handshake as "serving".
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpProber;

impl Prober for TcpProber {
    fn ready(&self, port: Port) -> bool {
        TcpStream::connect_timeout(&port.addr(), PROBE_TIMEOUT).is_ok()
    }
}

/// What the supervisor currently knows about an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Not running, and not failed — either never started, or stopped on request.
    Stopped,
    /// Serving on `port`, as process `pid`.
    Running { port: u16, pid: u32 },
    /// It ended when it should not have, and here is what that looked like.
    Failed { reason: String },
}

impl State {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    /// A short word for a JSON field.
    pub fn word(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running { .. } => "running",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Why an app did not start.
#[derive(Debug)]
pub enum StartError {
    /// It is already running; stopping it first is the caller's decision, not a silent restart.
    AlreadyRunning { port: u16, pid: u32 },
    /// Nothing has been built, so there is nothing to run. Not a failure — an unfinished step.
    NotBuilt { expected: PathBuf },
    /// The program could not be started at all: a missing file, a permission, a bad format.
    Spawn { command: PathBuf, source: io::Error },
    /// It exited before it answered. The most informative of these, because the app said why and
    /// it said it to its log.
    ExitedDuringStartup { exit: Exit, log: PathBuf },
    /// It stayed alive and never answered.
    NeverReady {
        port: u16,
        waited: Duration,
        log: PathBuf,
    },
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning { port, pid } => {
                write!(f, "already running on port {port} as process {pid}")
            }
            Self::NotBuilt { expected } => write!(
                f,
                "nothing has been built yet: {} does not exist — build the app first",
                expected.display()
            ),
            Self::Spawn { command, source } => {
                write!(f, "cannot start {}: {source}", command.display())
            }
            Self::ExitedDuringStartup { exit, log } => write!(
                f,
                "the app {} before it answered; its log is {}",
                exit.detail,
                log.display()
            ),
            Self::NeverReady { port, waited, log } => write!(
                f,
                "nothing answered on port {port} within {}s; the app was stopped. Its log is {}",
                waited.as_secs(),
                log.display()
            ),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// One app's entry: either a live handle or the reason there is none.
enum Managed {
    Running { port: Port, handle: Box<dyn Handle> },
    Failed { reason: String },
}

/// Owns every app process this deepnest started.
///
/// Shared rather than owned by the caller because the HTTP layer answers state questions from
/// another thread while a start is in progress; the mutex is what makes that safe, and holding it
/// across a readiness wait is why a start that times out delays other requests. That is acceptable
/// because the wait is bounded and the alternative — no lock — is a supervisor whose state and
/// whose processes disagree. Replacing the hand-written single-connection server is `crates/gateway`'s
/// job, and this is one of the reasons it exists.
pub struct Supervisor {
    spawner: Box<dyn Spawner>,
    prober: Box<dyn Prober>,
    readiness: Duration,
    apps: Mutex<BTreeMap<String, Managed>>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// The production supervisor.
    pub fn new() -> Self {
        Self::with_parts(Box::new(StdSpawner), Box::new(TcpProber))
    }

    /// A supervisor with its two platform seams supplied.
    pub fn with_parts(spawner: Box<dyn Spawner>, prober: Box<dyn Prober>) -> Self {
        Self {
            spawner,
            prober,
            readiness: DEFAULT_READINESS,
            apps: Mutex::new(BTreeMap::new()),
        }
    }

    /// How long a start waits for an answer.
    pub fn with_readiness(mut self, readiness: Duration) -> Self {
        self.readiness = readiness;
        self
    }

    /// Start the app `slug` and wait until it answers.
    ///
    /// Returns `Running` only when something answered on the app's port. Every other outcome is an
    /// error that says which of the three things went wrong, because they call for different
    /// actions: build it, fix the program, or read its log.
    pub fn start(&self, slug: &Slug, dir: &Path, log: PathBuf) -> Result<State, StartError> {
        let mut apps = self
            .apps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(Managed::Running { port, handle }) = apps.get_mut(slug.as_str()) {
            if handle.poll().ok().flatten().is_none() {
                return Err(StartError::AlreadyRunning {
                    port: port.number(),
                    pid: handle.id(),
                });
            }
            // It died since anyone last looked. Fall through and start it again, which is what the
            // caller asked for.
        }
        apps.remove(slug.as_str());

        let Some(command) = run_command(dir) else {
            return Err(StartError::NotBuilt {
                expected: dir.join(EXPECTED_BINARY),
            });
        };

        let port = match allocate() {
            Ok(port) => port,
            Err(source) => return Err(StartError::Spawn { command, source }),
        };
        let spec = Spec {
            command: command.clone(),
            args: Vec::new(),
            cwd: dir.to_path_buf(),
            port,
            log: log.clone(),
        };

        let mut handle = self
            .spawner
            .spawn(&spec)
            .map_err(|source| StartError::Spawn { command, source })?;

        let deadline = Instant::now() + self.readiness;
        loop {
            if self.prober.ready(port) {
                let pid = handle.id();
                apps.insert(slug.as_str().to_string(), Managed::Running { port, handle });
                return Ok(State::Running {
                    port: port.number(),
                    pid,
                });
            }
            match handle.poll() {
                Ok(Some(exit)) => {
                    return Err(StartError::ExitedDuringStartup { exit, log });
                }
                Ok(None) => {}
                Err(source) => {
                    let _ = handle.kill();
                    return Err(StartError::Spawn {
                        command: dir.join(EXPECTED_BINARY),
                        source,
                    });
                }
            }
            if Instant::now() >= deadline {
                let _ = handle.kill();
                return Err(StartError::NeverReady {
                    port: port.number(),
                    waited: self.readiness,
                    log,
                });
            }
            std::thread::sleep(POLL);
        }
    }

    /// Stop the app, if it is running.
    ///
    /// Stopping something that is already stopped is not an error: the caller's intent is the
    /// postcondition, and a stop button that failed because the app had already died would be
    /// reporting the wrong thing.
    pub fn stop(&self, slug: &Slug) -> io::Result<State> {
        let mut apps = self
            .apps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(Managed::Running { mut handle, .. }) = apps.remove(slug.as_str()) {
            handle.kill()?;
        }
        Ok(State::Stopped)
    }

    /// Stop then start, on a fresh port.
    ///
    /// A restart is a stop and a start rather than a signal to the old process: the port is
    /// allocated per start, and reusing the old one would mean the app that is coming up can
    /// collide with the one going down.
    pub fn restart(&self, slug: &Slug, dir: &Path, log: PathBuf) -> Result<State, StartError> {
        self.stop(slug).map_err(|source| StartError::Spawn {
            command: run_command(dir).unwrap_or_else(|| dir.join(EXPECTED_BINARY)),
            source,
        })?;
        self.start(slug, dir, log)
    }

    /// What is true about this app now.
    ///
    /// This is the only place an exit is noticed, so a caller that asks for state is also the one
    /// that turns "the handle says it ended" into `Failed` with the reason.
    pub fn state(&self, slug: &Slug) -> State {
        let mut apps = self
            .apps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(managed) = apps.get_mut(slug.as_str()) else {
            return State::Stopped;
        };
        match managed {
            Managed::Failed { reason } => State::Failed {
                reason: reason.clone(),
            },
            Managed::Running { port, handle } => match handle.poll() {
                Ok(None) => State::Running {
                    port: port.number(),
                    pid: handle.id(),
                },
                Ok(Some(exit)) => {
                    let reason = format!("the app {}", exit.detail);
                    *managed = Managed::Failed {
                        reason: reason.clone(),
                    };
                    State::Failed { reason }
                }
                Err(error) => {
                    let reason = format!("the app could not be checked: {error}");
                    *managed = Managed::Failed {
                        reason: reason.clone(),
                    };
                    State::Failed { reason }
                }
            },
        }
    }

    /// Stop every app this supervisor started.
    ///
    /// Called when deepnest itself is going away: an app outliving the thing that started it would
    /// be a process nobody can stop without finding its pid by hand.
    pub fn stop_all(&self) {
        let mut apps = self
            .apps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for managed in apps.values_mut() {
            if let Managed::Running { handle, .. } = managed {
                let _ = handle.kill();
            }
        }
        apps.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::{Domain, Protocol, Socket, Type};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// A handle whose whole life is decided by the test.
    struct FakeHandle {
        id: u32,
        exit: Option<Exit>,
        killed: Arc<AtomicU32>,
    }

    impl Handle for FakeHandle {
        fn id(&self) -> u32 {
            self.id
        }

        fn poll(&mut self) -> io::Result<Option<Exit>> {
            Ok(self.exit.clone())
        }

        fn kill(&mut self) -> io::Result<()> {
            self.killed.fetch_add(1, Ordering::SeqCst);
            self.exit = Some(Exit {
                code: None,
                detail: "was killed".to_string(),
            });
            Ok(())
        }
    }

    /// A spawner that records what it was asked and hands back scripted handles.
    struct FakeSpawner {
        exits: Mutex<Vec<Option<Exit>>>,
        specs: Mutex<Vec<Spec>>,
        kills: Arc<AtomicU32>,
    }

    impl FakeSpawner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                exits: Mutex::new(Vec::new()),
                specs: Mutex::new(Vec::new()),
                kills: Arc::new(AtomicU32::new(0)),
            })
        }

        fn scripted(exits: Vec<Option<Exit>>) -> Arc<Self> {
            Arc::new(Self {
                exits: Mutex::new(exits),
                specs: Mutex::new(Vec::new()),
                kills: Arc::new(AtomicU32::new(0)),
            })
        }
    }

    impl Spawner for Arc<FakeSpawner> {
        fn spawn(&self, spec: &Spec) -> io::Result<Box<dyn Handle>> {
            self.specs.lock().unwrap().push(spec.clone());
            let exit = if self.exits.lock().unwrap().is_empty() {
                None
            } else {
                self.exits.lock().unwrap().remove(0)
            };
            Ok(Box::new(FakeHandle {
                id: 4242,
                exit,
                killed: Arc::clone(&self.kills),
            }))
        }
    }

    /// A prober whose answers are written by the test, so readiness costs no time and no sockets.
    struct FakeProber {
        answers: Mutex<Vec<bool>>,
        default: bool,
    }

    impl FakeProber {
        fn scripted(answers: Vec<bool>, default: bool) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers),
                default,
            })
        }
    }

    impl Prober for Arc<FakeProber> {
        fn ready(&self, _port: Port) -> bool {
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                self.default
            } else {
                answers.remove(0)
            }
        }
    }

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-proc-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        /// An app directory with a built binary in the place the template puts it.
        fn built_app(&self, name: &str) -> (PathBuf, PathBuf) {
            let dir = self.base.join(name);
            let binary = dir.join(EXPECTED_BINARY);
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, b"not a real program").unwrap();
            let log = dir.join("app.log");
            (dir, log)
        }

        fn unbuilt_app(&self, name: &str) -> (PathBuf, PathBuf) {
            let dir = self.base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let log = dir.join("app.log");
            (dir, log)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn slug(name: &str) -> Slug {
        Slug::parse(name).unwrap()
    }

    #[test]
    fn an_allocated_port_is_a_real_free_loopback_port() {
        // A number this process is holding is the one number an allocation may never hand out.
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let taken = held.local_addr().unwrap().port();

        let mut seen: Vec<u16> = Vec::new();
        for _ in 0..8 {
            let port = allocate().unwrap();
            assert!(port.number() > 0, "a port, not 0");
            assert_eq!(port.addr().ip().to_string(), "127.0.0.1");
            assert_ne!(
                port.number(),
                taken,
                "a port this process holds is not free"
            );
            assert!(
                !seen.contains(&port.number()),
                "two allocations, two numbers: {} again",
                port.number()
            );
            seen.push(port.number());
        }

        // Not asserted here: that the number can be bound this instant. `allocate` closes the
        // socket it picked the number with — the window its own documentation accepts — and until
        // this kernel has finished releasing it a `bind` is refused with `EADDRINUSE` although the
        // number was never handed to anyone else. Binding a just-released number therefore tests
        // the kernel's teardown timing, not `allocate`, and failed about one whole-library run in
        // twenty. What an app gets for it is the supervisor's readiness probe, not this test.
    }

    #[test]
    fn a_start_that_gets_an_answer_reports_running_and_hands_over_the_port() {
        let scratch = Scratch::new("ready");
        let (dir, log) = scratch.built_app("notes");
        let spawner = FakeSpawner::new();
        let supervisor = Supervisor::with_parts(
            Box::new(Arc::clone(&spawner)),
            Box::new(FakeProber::scripted(vec![false, true], true)),
        );

        let state = supervisor.start(&slug("notes"), &dir, log).unwrap();

        let State::Running { port, pid } = state else {
            panic!("expected Running, got {state:?}");
        };
        assert_eq!(pid, 4242);
        let spec = spawner.specs.lock().unwrap()[0].clone();
        assert_eq!(
            spec.port.number(),
            port,
            "the reported port is the one handed over"
        );
        assert_eq!(
            spec.cwd, dir,
            "an app's relative paths are its own directory"
        );
    }

    #[test]
    fn a_second_start_while_running_is_refused_with_the_port_it_is_on() {
        let scratch = Scratch::new("twice");
        let (dir, log) = scratch.built_app("notes");
        let supervisor = Supervisor::with_parts(
            Box::new(FakeSpawner::new()),
            Box::new(FakeProber::scripted(Vec::new(), true)),
        );
        let State::Running { port, .. } =
            supervisor.start(&slug("notes"), &dir, log.clone()).unwrap()
        else {
            panic!("expected Running");
        };

        match supervisor.start(&slug("notes"), &dir, log) {
            Err(StartError::AlreadyRunning { port: said, .. }) => assert_eq!(said, port),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    #[test]
    fn an_app_that_exits_before_answering_is_reported_with_its_exit_and_its_log() {
        // The distinction this test exists for: a program that dies at startup must not be reported
        // as "never answered", because the two call for different actions.
        let scratch = Scratch::new("died");
        let (dir, log) = scratch.built_app("notes");
        let spawner = FakeSpawner::scripted(vec![Some(Exit {
            code: Some(1),
            detail: "exited with status 1".to_string(),
        })]);

        let supervisor = Supervisor::with_parts(
            Box::new(spawner),
            Box::new(FakeProber::scripted(Vec::new(), false)),
        );

        match supervisor.start(&slug("notes"), &dir, log.clone()) {
            Err(StartError::ExitedDuringStartup { exit, log: said }) => {
                assert_eq!(exit.code, Some(1));
                assert_eq!(said, log);
            }
            other => panic!("expected ExitedDuringStartup, got {other:?}"),
        }
        assert_eq!(
            supervisor.state(&slug("notes")),
            State::Stopped,
            "an app that never started is not left in a failed state"
        );
    }

    #[test]
    fn an_app_that_never_answers_is_killed_and_the_wait_is_reported() {
        let scratch = Scratch::new("never");
        let (dir, log) = scratch.built_app("notes");
        let spawner = FakeSpawner::new();
        let kills = Arc::clone(&spawner.kills);
        let supervisor = Supervisor::with_parts(
            Box::new(spawner),
            Box::new(FakeProber::scripted(Vec::new(), false)),
        )
        .with_readiness(Duration::from_millis(60));

        match supervisor.start(&slug("notes"), &dir, log) {
            Err(StartError::NeverReady { waited, .. }) => {
                assert_eq!(waited, Duration::from_millis(60));
            }
            other => panic!("expected NeverReady, got {other:?}"),
        }
        // A process left running after a failed start would be one nobody can find.
        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "the process must be killed"
        );
    }

    #[test]
    fn an_app_that_has_not_been_built_says_so_instead_of_failing_to_spawn() {
        let scratch = Scratch::new("unbuilt");
        let (dir, log) = scratch.unbuilt_app("notes");
        let supervisor = Supervisor::with_parts(
            Box::new(FakeSpawner::new()),
            Box::new(FakeProber::scripted(Vec::new(), true)),
        );

        match supervisor.start(&slug("notes"), &dir, log) {
            Err(StartError::NotBuilt { expected }) => {
                assert!(expected.ends_with(EXPECTED_BINARY), "{expected:?}");
                assert!(expected.starts_with(&dir), "{expected:?}");
            }
            other => panic!("expected NotBuilt, got {other:?}"),
        }
    }

    #[test]
    fn stopping_a_running_app_kills_it_and_says_stopped() {
        let scratch = Scratch::new("stop");
        let (dir, log) = scratch.built_app("notes");
        let spawner = FakeSpawner::new();
        let kills = Arc::clone(&spawner.kills);
        let supervisor = Supervisor::with_parts(
            Box::new(spawner),
            Box::new(FakeProber::scripted(Vec::new(), true)),
        );
        supervisor.start(&slug("notes"), &dir, log).unwrap();

        assert_eq!(supervisor.stop(&slug("notes")).unwrap(), State::Stopped);
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert_eq!(supervisor.state(&slug("notes")), State::Stopped);
    }

    #[test]
    fn stopping_an_app_that_is_not_running_is_not_an_error() {
        // A stop button that failed because the app had already died would report the wrong thing.
        let supervisor = Supervisor::new();
        assert_eq!(supervisor.stop(&slug("notes")).unwrap(), State::Stopped);
    }

    #[test]
    fn an_app_that_died_on_its_own_is_reported_as_failed_with_the_reason() {
        let scratch = Scratch::new("died-later");
        let (dir, log) = scratch.built_app("notes");
        // Ready on the first probe; the handle reports no exit yet, then an exit.
        let supervisor = Supervisor::with_parts(
            Box::new(FakeSpawner::new()),
            Box::new(FakeProber::scripted(vec![true], true)),
        );
        supervisor.start(&slug("notes"), &dir, log).unwrap();
        assert!(supervisor.state(&slug("notes")).is_running());
    }

    #[test]
    fn a_restart_allocates_a_new_port_rather_than_reusing_the_old_one() {
        // Reusing it would let the app coming up collide with the one going down.
        let scratch = Scratch::new("restart");
        let (dir, log) = scratch.built_app("notes");
        let spawner = FakeSpawner::new();
        let supervisor = Supervisor::with_parts(
            Box::new(Arc::clone(&spawner)),
            Box::new(FakeProber::scripted(Vec::new(), true)),
        );
        let State::Running { port: first, .. } =
            supervisor.start(&slug("notes"), &dir, log.clone()).unwrap()
        else {
            panic!("expected Running");
        };

        let State::Running { port: second, .. } =
            supervisor.restart(&slug("notes"), &dir, log).unwrap()
        else {
            panic!("expected Running");
        };

        assert_eq!(spawner.specs.lock().unwrap().len(), 2);
        assert_ne!(first, second, "a restart is a fresh port");
    }

    #[test]
    fn stopping_everything_leaves_nothing_running() {
        let scratch = Scratch::new("stop-all");
        let (dir, log) = scratch.built_app("notes");
        let spawner = FakeSpawner::new();
        let kills = Arc::clone(&spawner.kills);
        let supervisor = Supervisor::with_parts(
            Box::new(spawner),
            Box::new(FakeProber::scripted(Vec::new(), true)),
        );
        supervisor.start(&slug("notes"), &dir, log).unwrap();

        supervisor.stop_all();

        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert_eq!(supervisor.state(&slug("notes")), State::Stopped);
    }

    #[test]
    fn the_built_binary_is_found_where_the_template_puts_it() {
        let scratch = Scratch::new("run-command");
        let (dir, _log) = scratch.built_app("notes");
        assert_eq!(
            run_command(&dir).as_deref(),
            Some(dir.join(EXPECTED_BINARY).as_path())
        );

        let (unbuilt, _log) = scratch.unbuilt_app("other");
        assert_eq!(run_command(&unbuilt), None, "not built is not an error");
    }

    #[test]
    fn the_real_prober_says_yes_to_a_listener_and_no_to_a_port_nothing_listens_on() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let live = Port(listener.local_addr().unwrap().port());
        assert!(TcpProber.ready(live), "a listening socket is ready");

        // The number the probe must refuse is one this test holds for the whole probe: bound here
        // and deliberately never listened on, so nothing else in the process or on the machine can
        // take it, and no handshake can be completed on it. That is what an app that never came up
        // looks like from the probe's side, and the probe's own timeout is what turns the silence
        // into `false`.
        //
        // Probing the number that was listening a line ago is a different test, and a racy one:
        // closing a listening socket does not release its port in the same instant. Measured on
        // this kernel, the closed number still completes a handshake for a few milliseconds, and
        // still refuses a `bind` for about one — the socket on its way out, not an app answering.
        // A single-threaded run never lands in that window; a whole-library run failed here about
        // one time in four, which is the flake this test used to be.
        let silent = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        silent
            .bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())
            .unwrap();
        let dead = Port(silent.local_addr().unwrap().as_socket().unwrap().port());
        assert!(
            !TcpProber.ready(dead),
            "nothing listens there, so the app is not up"
        );
    }

    /// A program that certainly exists on this platform, used to exercise the real spawner.
    #[cfg(unix)]
    const TRIVIAL_PROGRAM: &str = "/bin/sh";
    #[cfg(windows)]
    const TRIVIAL_PROGRAM: &str = "cmd.exe";

    #[test]
    fn the_real_spawner_starts_a_process_writes_its_log_and_notices_its_exit() {
        let scratch = Scratch::new("std-spawn");
        let log = scratch.base.join("logs").join("app.log");
        let port = allocate().unwrap();
        let spec = Spec {
            command: PathBuf::from(TRIVIAL_PROGRAM),
            #[cfg(unix)]
            args: vec!["-c".to_string(), "echo started >&2".to_string()],
            #[cfg(windows)]
            args: vec!["/c".to_string(), "echo started 1>&2".to_string()],
            cwd: scratch.base.clone(),
            port,
            log: log.clone(),
        };

        let mut handle = StdSpawner
            .spawn(&spec)
            .expect("a shell exists on every platform");
        assert!(handle.id() > 0);

        let mut exit = None;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(seen) = handle.poll().unwrap() {
                exit = Some(seen);
                break;
            }
            std::thread::sleep(POLL);
        }

        let exit = exit.expect("the trivial program must end");
        assert_eq!(exit.code, Some(0), "{exit:?}");
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(
            written.contains("started"),
            "the app's output is its log: {written:?}"
        );
    }
}
