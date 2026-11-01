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
//! **The accept loop stays one thread, and each connection gets its own.** That is the shape this
//! changed into when the gateway learned to hand a connection over (`decided in implementation`,
//! 2026-10-31): a WebSocket or an event stream is a connection that outlives the request that
//! opened it, and answering it inline would stop the loop — and the token keepalive with it — for
//! as long as the user kept the tab open. A request/response pair still finishes in microseconds;
//! what changed is that nothing else has to wait for it.
//!
//! Concurrency here is bounded ([`MAX_CONNECTIONS`]) and so is shutdown: a stream never ends on its
//! own, so `shutdown` closes the sockets rather than waiting for the user to leave.

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use deepnest_gateway::Answer;
use socket2::{Domain, Protocol, Socket, Type};

use crate::http::{self, Method, ParseError, ReadError, Response};
use crate::router::{Lifecycle, Router};
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

/// The most connections served at once.
///
/// A panel, an app and its socket are a handful. This is a ceiling for the case nobody plans for —
/// a loop on this machine opening connections until the process runs out of threads — and past it
/// a connection is refused by name instead of queued, because a queue in front of a full server is
/// a wait with no end.
const MAX_CONNECTIONS: usize = 64;

/// How long `shutdown` waits for the connection threads to notice their sockets are gone.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

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
    /// The process supervisor, when this process may start and stop apps. `None` leaves the
    /// lifecycle routes answering `503`: a server that read apps but could not run them would
    /// otherwise have to pretend the button worked.
    pub lifecycle: Option<Lifecycle>,
    /// `~/.deepnest/config.toml`, for the panel's first-run form. `None` leaves `POST /api/config`
    /// answering `503`: a process with no config path has nowhere to save one, and answering "saved"
    /// to a write that went nowhere is the one lie this server must not tell.
    pub config_path: Option<PathBuf>,
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
            lifecycle: None,
            config_path: None,
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

/// The connections being served right now.
///
/// It exists for one reason: a stream outlives the request that opened it, so `shutdown` cannot
/// wait for connections to end on their own — it has to end them. A handle on each socket is what
/// makes that a shutdown rather than a hope.
#[derive(Debug, Default)]
struct Live {
    members: Mutex<BTreeMap<u64, TcpStream>>,
    next: AtomicU64,
}

impl Live {
    fn count(&self) -> usize {
        self.members
            .lock()
            .map(|members| members.len())
            .unwrap_or(0)
    }

    /// Take a seat for `stream`, keeping a handle on the same socket.
    fn admit(self: &Arc<Self>, stream: &TcpStream) -> Option<Seat> {
        let handle = stream.try_clone().ok()?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.members.lock().ok()?.insert(id, handle);
        Some(Seat {
            id,
            live: Arc::clone(self),
        })
    }

    /// Close every connection being served, and wait for their threads to leave.
    fn close_all(&self) {
        // The sockets are cloned out before anything is shut down, so no thread is holding the
        // lock while its own socket is being closed.
        let sockets: Vec<TcpStream> = match self.members.lock() {
            Ok(members) => members
                .values()
                .filter_map(|socket| socket.try_clone().ok())
                .collect(),
            Err(_) => return,
        };
        for socket in &sockets {
            // Wakes a reader parked on this socket in either direction, which is the only thing
            // that ends a stream nobody closed.
            let _ = socket.shutdown(Shutdown::Both);
        }
        let deadline = Instant::now() + CLOSE_WAIT;
        while self.count() > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A connection's place in [`Live`], given back when the connection is done.
#[derive(Debug)]
struct Seat {
    id: u64,
    live: Arc<Live>,
}

impl Drop for Seat {
    fn drop(&mut self) {
        if let Ok(mut members) = self.live.members.lock() {
            members.remove(&self.id);
        }
    }
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
    if let Some(path) = options.config_path.clone() {
        router = router.with_config_path(path);
    }
    match (options.lifecycle.clone(), options.apps_dir.clone()) {
        // The supervisor is what makes the lifecycle routes real, and a home always implies
        // `home/apps` — so a caller that gave the home need not spell the directory twice. The
        // lifecycle is taken whole, so a builder or forwarder its caller chose arrives with it.
        (Some(lifecycle), dir) => {
            let apps = dir.unwrap_or_else(|| deepnest_apps::apps_dir(&lifecycle.home));
            router = router.with_parts(lifecycle, apps);
        }
        (None, Some(dir)) => router = router.with_apps_dir(dir),
        (None, None) => {}
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
    let live = Arc::new(Live::default());
    let mut next_keepalive = Instant::now() + interval;

    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // One connection, one thread. A request/response pair would fit in this loop, but a
                // WebSocket or an event stream does not: it is a connection that outlives its
                // request, and answering it here would stop the loop — and the token keepalive —
                // until the user closed the tab.
                if live.count() >= MAX_CONNECTIONS {
                    refuse_busy(stream);
                    continue;
                }
                let Some(seat) = live.admit(&stream) else {
                    // The socket could not be duplicated, so this connection could not be closed
                    // by `shutdown`. Dropping it is the honest answer.
                    continue;
                };
                let router = Arc::clone(&router);
                // The connection is told when the server is stopping, because closing its socket
                // is not enough to end a stream on every platform: a thread blocked in `read` on
                // a duplicated handle is not guaranteed to wake (measured on windows-latest,
                // 2026-10-31). The flag is what a stream that is saying nothing can still see.
                let stopping = Arc::clone(&stop);
                // Detached on purpose: `shutdown` ends connections by closing their sockets, not
                // by waiting on these handles.
                let spawned = thread::Builder::new()
                    .name("deepnest-conn".to_string())
                    .spawn(move || {
                        let _seat = seat;
                        let _ = answer(stream, &router, stopping);
                    });
                // A refused spawn closes the connection by dropping it, which is what the client
                // sees either way. There is no reason to stop accepting.
                let _ = spawned;
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

    // What is still open is ended here. Without this a stream would hold its thread, and its
    // socket, until the user left — and `shutdown` would be a word this server did not keep.
    live.close_all();
}

/// The answer to a connection that arrived when there was no room for it.
fn refuse_busy(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let response = Response::text(
        503,
        "deepnest is already serving the most connections it will hold at once\n",
    );
    let _ = response.write_to(&mut stream, false);
    let _ = stream.shutdown(Shutdown::Write);
}

fn answer(mut stream: TcpStream, router: &Router, stop: Arc<AtomicBool>) -> io::Result<()> {
    // Blocking, explicitly. BSD — and therefore macOS — hands the accepted socket the listening
    // socket's non-blocking state, while Linux does not. Inheriting it turns "the head has not
    // arrived in one packet yet" into a client error, and the half-written answer that follows
    // is a connection reset at the other end. This cost two of eight integration tests on the
    // macOS CI job and none on Linux: exactly the difference this matrix exists to find.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));

    let (answer, head_only) = match http::read(&mut stream) {
        Ok(request) => {
            let head_only = request.method == Method::Head;
            (router.respond(&request), head_only)
        }
        Err(ReadError::Parse(ParseError::UnsupportedMethod)) => (
            Answer::Full(
                Response::text(405, "this server answers GET, HEAD, POST and DELETE\n")
                    .header("Allow", "GET, HEAD, POST, DELETE"),
            ),
            false,
        ),
        // A chunked body is refused before its route is looked at: the bound that makes a body
        // safe to read is a declared length, and a chunked one has none until it has all arrived.
        Err(ReadError::Parse(ParseError::Body)) => (
            Answer::Full(Response::text(
                411,
                "this server reads a request body only with a Content-Length: send one, or send \
                 nothing\n",
            )),
            false,
        ),
        // A body past the bound is a decision about how much memory this process spends, and the
        // answer is the status rather than the buffer.
        Err(ReadError::Parse(ParseError::TooLarge)) => (
            Answer::Full(Response::text(
                413,
                format!(
                    "request body too large: this server reads at most {} bytes\n",
                    deepnest_gateway::http::MAX_BODY
                ),
            )),
            false,
        ),
        Err(ReadError::Parse(ParseError::Oversized)) => (
            Answer::Full(Response::text(413, "request head too large\n")),
            false,
        ),
        Err(ReadError::Parse(ParseError::Malformed)) => (
            Answer::Full(Response::text(400, "malformed request\n")),
            false,
        ),
        Err(ReadError::Io(error)) => {
            // Nothing arrived, or the client left. Answering is still the polite thing to do —
            // the write fails harmlessly on a closed socket.
            let _ = error;
            (
                Answer::Full(Response::text(400, "incomplete request\n")),
                false,
            )
        }
    };

    match answer {
        Answer::Full(response) => {
            let result = response.write_to(&mut stream, head_only);
            let _ = stream.shutdown(Shutdown::Write);
            result
        }
        // An app that upgraded or is streaming. Its head goes out exactly as the app framed it —
        // its framing headers are the framing of everything after — and then the two connections
        // are joined for as long as both sides stay.
        Answer::Stream(through) => through.pump(&mut stream, head_only, stop),
        // An answer this process writes itself: the head now, the body as it is produced. The
        // connection closes when the body is done, which is what delimits an answer with no length.
        Answer::Produced(produced) => {
            let result = produced.write_to(&mut stream, head_only);
            let _ = stream.shutdown(Shutdown::Write);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    use crate::http::Request;
    use crate::router::Lifecycle;
    use deepnest_gateway::{ForwardError, Forwarder, Stream, StreamKind, Target};

    /// A policy that gives up almost at once: a real "already in use" must not cost a test two
    /// seconds per assertion.
    fn brisk() -> BindAttempts {
        BindAttempts {
            attempts: 2,
            delay: Duration::from_millis(10),
        }
    }

    /// Send one raw request on a fresh connection and read the whole answer.
    ///
    /// The whole answer, not a stream: every route below deepnest's own closes the connection when
    /// it is done, which is exactly what makes it usable as a one-shot probe.
    fn call(addr: SocketAddr, head: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect to the server this test started");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).unwrap();
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// A home with one app in it, ready to be started.
    struct Home {
        base: PathBuf,
    }

    impl Home {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-stream-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn apps(&self) -> PathBuf {
            deepnest_apps::apps_dir(&self.base)
        }

        fn built_app(&self, name: &str) {
            let dir = self.apps().join("notes");
            deepnest_apps::Manifest::new(name).save(&dir).unwrap();
            let binary = dir.join(deepnest_apps::EXPECTED_BINARY);
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, b"a stand-in for the app the template will build").unwrap();
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// An app that says yes to an upgrade and then echoes whatever it is sent.
    ///
    /// A tunnel's whole contract, in one loop: the head goes out once, and after that bytes cross
    /// in both directions untouched.
    fn echoing_app() -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\r\n",
            );
            let mut chunk = [0u8; 1024];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => {
                        if stream.write_all(&chunk[..count]).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        (addr, handle)
    }

    /// Hands every `/app/` request the same connection to a stand-in app.
    struct HandsOver {
        app: SocketAddr,
    }

    impl Forwarder for HandsOver {
        fn forward(&self, _target: &Target, _request: &Request) -> Result<Answer, ForwardError> {
            let mut upstream =
                TcpStream::connect(self.app).map_err(|source| ForwardError::Connect {
                    port: self.app.port(),
                    source,
                })?;
            // The app answers the handshake as soon as it accepts, and this reads exactly its head
            // — a byte at a time, so nothing past the head is swallowed into the wrong buffer.
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match upstream.read(&mut byte) {
                    Ok(0) | Err(_) => {
                        return Err(ForwardError::Malformed("the app did not upgrade"))
                    }
                    Ok(_) => head.push(byte[0]),
                }
            }
            Ok(Answer::Stream(Stream::new(
                upstream,
                head,
                StreamKind::Upgrade,
            )))
        }
    }

    struct NeverExits;

    impl deepnest_apps::Handle for NeverExits {
        fn id(&self) -> u32 {
            4242
        }

        fn poll(&mut self) -> io::Result<Option<deepnest_apps::Exit>> {
            Ok(None)
        }

        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct NeverStarts;

    impl deepnest_apps::Spawner for NeverStarts {
        fn spawn(&self, _spec: &deepnest_apps::Spec) -> io::Result<Box<dyn deepnest_apps::Handle>> {
            Ok(Box::new(NeverExits))
        }
    }

    /// Answers on the first probe: what a start waits for is `deepnest-apps`' test.
    struct AlwaysReady;

    impl deepnest_apps::Prober for AlwaysReady {
        fn ready(&self, _port: deepnest_apps::Port) -> bool {
            true
        }
    }

    /// A server whose `/app/` requests are answered by `app`.
    fn streaming_server(home: &Home, app: SocketAddr) -> (Server, Token) {
        let supervisor = Arc::new(deepnest_apps::Supervisor::with_parts(
            Box::new(NeverStarts),
            Box::new(AlwaysReady),
        ));
        let lifecycle = Lifecycle::new(supervisor, home.base.clone())
            .with_forwarder(Arc::new(HandsOver { app }));
        let token = token::ensure(&home.base.join(token::FILE_NAME)).unwrap();
        let server = start(ServerOptions {
            port: 0,
            token: Some(token.clone()),
            apps_dir: Some(home.apps()),
            lifecycle: Some(lifecycle),
            bind: brisk(),
            ..Default::default()
        })
        .unwrap();
        (server, token)
    }

    #[test]
    fn a_client_that_says_nothing_does_not_hold_the_server() {
        // The regression this loop exists for: one connection at a time meant a browser that
        // preconnected and had not sent its request yet held every other request behind it for
        // `READ_TIMEOUT`.
        let server = start(ServerOptions {
            port: 0,
            bind: brisk(),
            ..Default::default()
        })
        .unwrap();
        let addr = server.addr();

        let _idle = TcpStream::connect(addr).unwrap();

        let started = Instant::now();
        let answer = call(
            addr,
            &format!(
                "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                addr.port()
            ),
        );
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the idle connection held the server for {:?}",
            started.elapsed()
        );

        server.shutdown();
    }

    #[test]
    fn an_upgraded_connection_carries_frames_and_leaves_the_server_free() {
        let home = Home::new("upgrade");
        home.built_app("Notes");
        let (app_addr, app) = echoing_app();
        let (server, token) = streaming_server(&home, app_addr);
        let addr = server.addr();

        let started = call(
            addr,
            &format!(
                "POST /api/apps/notes/start HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\r\n",
                addr.port(),
                token.as_str()
            ),
        );
        assert!(started.starts_with("HTTP/1.1 200"), "{started}");

        let mut socket = TcpStream::connect(addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        socket
            .write_all(
                format!(
                    "GET /app/notes/socket HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
                    addr.port()
                )
                .as_bytes(),
            )
            .unwrap();

        // The head may arrive in more than one read; what matters is that it arrives.
        let mut received = Vec::new();
        let mut buffer = [0u8; 256];
        while !received.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "the connection ended before its head did");
            received.extend_from_slice(&buffer[..read]);
        }
        let head = String::from_utf8_lossy(&received).to_string();
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        assert!(
            head.to_ascii_lowercase().contains("upgrade: websocket"),
            "{head}"
        );

        // Past the head this is not HTTP any more: a byte sent is a byte echoed.
        socket.write_all(b"frame-one").unwrap();
        let mut echoed = [0u8; 9];
        socket.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"frame-one");

        // And the server is still serving: the tunnel holds a thread, not the port.
        let health = call(
            addr,
            &format!(
                "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                addr.port()
            ),
        );
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");

        // Shutdown ends the open stream instead of waiting for the user to leave.
        let before = Instant::now();
        server.shutdown();
        assert!(
            before.elapsed() < CLOSE_WAIT,
            "shutdown waited {:?} for a stream that never ends",
            before.elapsed()
        );
        let mut rest = [0u8; 64];
        assert_eq!(
            socket.read(&mut rest).unwrap_or(0),
            0,
            "the stream survived the shutdown"
        );
        app.join().unwrap();
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
