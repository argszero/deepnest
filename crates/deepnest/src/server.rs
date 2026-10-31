//! The one listening port — and the fact that taking it **is** the admission.
//!
//! Two deepnest instances must not both serve `127.0.0.1:4270`, and there are two usual ways to
//! enforce that: a PID file, or a lock check followed by a bind. Both have a window between the
//! question and the answer. This one has neither, because the kernel is the arbiter: the second
//! `bind` on the same address fails, and it fails whether or not this process looked first.
//!
//! Two platform details are load-bearing and are stated where they are set:
//!
//! - **`SO_REUSEADDR` on unix, never `SO_REUSEPORT`.** A previous run's connections can hold the
//!   port in `TIME_WAIT`, so a restart a second after a clean exit would fail without it —
//!   `SO_REUSEADDR` skips sockets that are not listening. `SO_REUSEPORT` is a different thing
//!   that would let a second instance *share* the port, which is exactly what this module exists
//!   to prevent.
//! - **Nothing on Windows.** `SO_REUSEADDR` there means "let another process take this port too",
//!   and the default — neither option set — already refuses a second bind with `WSAEADDRINUSE`.
//!   Setting `SO_EXCLUSIVEADDRUSE` explicitly would harden it against a process that sets
//!   `SO_REUSEADDR` first, and that needs a raw winsock call; it is deferred, and this comment is
//!   the record rather than a claim that it is done.
//!
//! The loop is one thread, one connection at a time, non-blocking with a short poll so that a
//! shutdown request and the token keepalive are both noticed promptly. That is the right size for
//! a control panel that serves one browser tab; `crates/gateway` will need a real server, and
//! replacing this loop is its decision to make.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::http::{self, Method, ParseError, ReadError, Response};
use crate::router::Router;
use crate::token::{self, Token};

/// The port the design fixes. It is the only port anyone — user, browser, README — knows.
pub const DEFAULT_PORT: u16 = 4270;

/// The address deepnest listens on. Loopback only: nothing here is reachable from the network.
pub const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// How long a client may take to send its request head, and to accept a response.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the accept loop looks for work when there is none.
///
/// Small enough that a request is answered without a perceptible delay, large enough that an
/// idle deepnest costs no measurable CPU.
const POLL: Duration = Duration::from_millis(10);

/// How hard to try before concluding the port belongs to someone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindAttempts {
    pub attempts: u32,
    pub delay: Duration,
}

impl Default for BindAttempts {
    fn default() -> Self {
        // Two seconds in total. Long enough to outlast a transient `TIME_WAIT` collision,
        // short enough that a real "already running" is reported while the user still expects an
        // answer.
        Self {
            attempts: 10,
            delay: Duration::from_millis(200),
        }
    }
}

/// Why the port could not be taken.
#[derive(Debug)]
pub enum BindError {
    /// The address would not come free. Either another deepnest is already serving, or a socket
    /// this process cannot see is holding it.
    InUse { addr: SocketAddr, attempts: u32 },
    /// The socket could not be created, configured or bound for some other reason.
    Socket { addr: SocketAddr, source: io::Error },
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InUse { addr, attempts } => write!(
                f,
                "{addr} is already taken (tried {attempts}×) — another deepnest is probably running"
            ),
            Self::Socket { addr, source } => write!(f, "cannot listen on {addr}: {source}"),
        }
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InUse { .. } => None,
            Self::Socket { source, .. } => Some(source),
        }
    }
}

/// Why a server did not start.
#[derive(Debug)]
pub enum StartError {
    Bind(BindError),
    /// The listener thread could not be created — an OS limit, not a bug in the caller.
    Spawn(io::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind(error) => write!(f, "{error}"),
            Self::Spawn(error) => write!(f, "cannot start the listener thread: {error}"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind(error) => Some(error),
            Self::Spawn(error) => Some(error),
        }
    }
}

/// What to serve, and where the token lives.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// `0` asks the OS for a free port — how a test gets one without racing a real deepnest.
    pub port: u16,
    /// The control token. The binary always passes one; `None` disables the `/api/` check, which
    /// exists only so tests can exercise routing without writing a file.
    pub token: Option<Token>,
    /// Where to re-assert the token on the keepalive. Both this and `token` must be set for the
    /// keepalive to run.
    pub token_path: Option<PathBuf>,
    /// The model name from the config, for the panel.
    pub model: Option<String>,
    /// `~/.deepnest/apps`, for the panel and `/api/apps`. `None` reports that the directory is
    /// unknown rather than answering that there are no apps.
    pub apps_dir: Option<PathBuf>,
    /// How often the keepalive re-asserts the token file.
    pub keepalive: Duration,
    pub bind: BindAttempts,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            token: None,
            token_path: None,
            model: None,
            apps_dir: None,
            keepalive: Duration::from_secs(30),
            bind: BindAttempts::default(),
        }
    }
}

/// Take `port` on loopback, retrying a bounded number of times.
///
/// A failure that is not `AddrInUse` is reported at once: retrying an address this machine does
/// not have would only delay the same answer.
pub fn bind(port: u16, policy: BindAttempts) -> Result<TcpListener, BindError> {
    let addr = SocketAddr::new(LOOPBACK, port);
    let attempts = policy.attempts.max(1);

    for attempt in 1..=attempts {
        match listen(addr) {
            Ok(listener) => return Ok(listener),
            Err(source) if source.kind() == io::ErrorKind::AddrInUse => {
                if attempt < attempts {
                    thread::sleep(policy.delay);
                }
            }
            Err(source) => return Err(BindError::Socket { addr, source }),
        }
    }
    Err(BindError::InUse { addr, attempts })
}

fn listen(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    #[cfg(unix)]
    {
        // Never `set_reuse_port`: see the module docs.
        socket.set_reuse_address(true)?;
    }
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    Ok(socket.into())
}

/// A running server. Dropping it stops the listener, so a test cannot leak one.
#[derive(Debug)]
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    /// Where it is listening.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The port it is listening on — resolved, so a `0` request reads back as the real one.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The address a browser would open.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.addr.port())
    }

    /// Stop listening and wait for the listener thread to finish.
    ///
    /// Waiting matters: it is what makes "the port is free afterwards" a fact rather than a hope,
    /// and a test that asserted the restart path without it would be flaky on a slow machine.
    pub fn shutdown(mut self) {
        self.stop_listener();
    }

    fn stop_listener(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop_listener();
    }
}

/// Bind and start serving, or say why not.
pub fn start(options: ServerOptions) -> Result<Server, StartError> {
    let listener = bind(options.port, options.bind).map_err(StartError::Bind)?;
    let requested = SocketAddr::new(LOOPBACK, options.port);
    let addr = listener.local_addr().map_err(|source| {
        StartError::Bind(BindError::Socket {
            addr: requested,
            source,
        })
    })?;

    let mut router = Router::new(addr.port(), options.model.clone(), options.token.clone());
    if let Some(dir) = options.apps_dir.clone() {
        router = router.with_apps_dir(dir);
    }
    let router = Arc::new(router);
    let keepalive = match (options.token_path.clone(), options.token) {
        (Some(path), Some(token)) => Some((path, token)),
        _ => None,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let interval = options.keepalive;

    let worker = thread::Builder::new()
        .name("deepnest-http".to_string())
        .spawn(move || serve(listener, router, worker_stop, keepalive, interval))
        .map_err(StartError::Spawn)?;

    Ok(Server {
        addr,
        stop,
        worker: Some(worker),
    })
}

fn serve(
    listener: TcpListener,
    router: Arc<Router>,
    stop: Arc<AtomicBool>,
    keepalive: Option<(PathBuf, Token)>,
    interval: Duration,
) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    let mut next_keepalive = Instant::now() + interval;

    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // One connection, one request, one answer, closed. A client that stalls is held
                // to the socket timeouts rather than to the loop's patience.
                let _ = answer(stream, &router);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= next_keepalive {
                    if let Some((path, token)) = &keepalive {
                        // Best effort: a tick that cannot write is retried on the next one.
                        let _ = token::reassert(path, token);
                    }
                    next_keepalive = Instant::now() + interval;
                }
                thread::sleep(POLL);
            }
            // A transient accept error (a client that vanished between SYN and accept) is not a
            // reason to stop serving.
            Err(_) => thread::sleep(POLL),
        }
    }
}

fn answer(mut stream: TcpStream, router: &Router) -> io::Result<()> {
    // Blocking, explicitly. BSD — and therefore macOS — hands the accepted socket the listening
    // socket's non-blocking state, while Linux does not. Inheriting it turns "the head has not
    // arrived in one packet yet" into a client error, and the half-written answer that follows
    // is a connection reset at the other end. This cost two of eight integration tests on the
    // macOS CI job and none on Linux: exactly the difference this matrix exists to find.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));

    let (response, head_only) = match http::read(&mut stream) {
        Ok(request) => {
            let head_only = request.method == Method::Head;
            (router.respond(&request), head_only)
        }
        Err(ReadError::Parse(ParseError::UnsupportedMethod)) => (
            Response::text(405, "this server answers GET and HEAD\n").header("Allow", "GET, HEAD"),
            false,
        ),
        Err(ReadError::Parse(ParseError::Oversized)) => {
            (Response::text(413, "request head too large\n"), false)
        }
        Err(ReadError::Parse(ParseError::Malformed)) => {
            (Response::text(400, "malformed request\n"), false)
        }
        Err(ReadError::Io(error)) => {
            // Nothing arrived, or the client left. Answering is still the polite thing to do —
            // the write fails harmlessly on a closed socket.
            let _ = error;
            (Response::text(400, "incomplete request\n"), false)
        }
    };

    let result = response.write_to(&mut stream, head_only);
    let _ = stream.shutdown(Shutdown::Write);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy that gives up almost at once: a real "already in use" must not cost a test two
    /// seconds per assertion.
    fn brisk() -> BindAttempts {
        BindAttempts {
            attempts: 2,
            delay: Duration::from_millis(10),
        }
    }

    #[test]
    fn a_second_bind_on_a_held_port_is_refused() {
        // This is the single-instance rule, tested where it actually lives: the kernel.
        let first = bind(0, brisk()).unwrap();
        let port = first.local_addr().unwrap().port();

        match bind(port, brisk()) {
            Err(BindError::InUse { attempts, .. }) => assert_eq!(attempts, 2),
            other => panic!("expected InUse, got {other:?}"),
        }
    }

    #[test]
    fn an_ephemeral_request_reports_the_port_it_got() {
        let options = ServerOptions {
            port: 0,
            ..Default::default()
        };
        let server = start(options).unwrap();
        assert_ne!(server.port(), 0);
        assert_eq!(server.addr().ip(), LOOPBACK);
        assert_eq!(server.url(), format!("http://127.0.0.1:{}/", server.port()));
        server.shutdown();
    }

    #[test]
    fn shutting_down_frees_the_port() {
        let options = ServerOptions {
            port: 0,
            ..Default::default()
        };
        let server = start(options).unwrap();
        let port = server.port();
        server.shutdown();

        // If the listener thread outlived `shutdown`, this bind would fail and a restart would
        // be broken in the one case it matters most: the run that just ended.
        let again = bind(port, brisk()).unwrap();
        assert_eq!(again.local_addr().unwrap().port(), port);
    }

    #[test]
    fn dropping_a_server_stops_it_too() {
        let port = {
            let server = start(ServerOptions {
                port: 0,
                ..Default::default()
            })
            .unwrap();
            server.port()
        };
        assert!(bind(port, brisk()).is_ok());
    }

    #[test]
    fn the_keepalive_puts_a_deleted_token_back() {
        let base = std::env::temp_dir().join(format!("deepnest-serve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join(token::FILE_NAME);
        let token = token::ensure(&path).unwrap();

        let server = start(ServerOptions {
            port: 0,
            token: Some(token.clone()),
            token_path: Some(path.clone()),
            keepalive: Duration::from_millis(20),
            ..Default::default()
        })
        .unwrap();

        std::fs::remove_file(&path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let restored = std::fs::read_to_string(&path)
                .map(|text| text.trim() == token.as_str())
                .unwrap_or(false);
            if restored {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        // The same token, not a fresh one: a new token would refuse the panel that holds the old
        // one until the next restart.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            token.as_str()
        );

        server.shutdown();
        let _ = std::fs::remove_dir_all(&base);
    }
}
