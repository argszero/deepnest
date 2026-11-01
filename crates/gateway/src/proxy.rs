//! Serving an app under `/app/<slug>/`: the forward itself.
//!
//! The app's server listens on a loopback port of its own, allocated per start and never
//! persisted. This module turns a request that arrived on deepnest's port into one for that app,
//! and the app's answer back into one the browser can use under the prefix.
//!
//! **An answer is not always a body** (`decided in implementation`, 2026-10-31). A WebSocket
//! upgrade and an SSE stream are both a head followed by bytes that stop when someone leaves, and
//! reading either of them to the end is reading it forever: the browser would receive the first
//! byte of its live connection only after it had given up on it. So the forward no longer promises
//! "all the bytes"; it answers one of two things — a complete response, or the connection itself
//! ([`Answer`]). Everything below the decision is unchanged: an app that answers and closes is
//! still read whole, still decoded if it chunked its body, still adapted to the prefix.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::dialog;
use crate::http::{Method, Request, Response};
use crate::rewrite::{self, is_hop_by_hop};

/// How long to wait for the app to accept a connection.
///
/// It is on this machine and it is already running — `start` does not report success until it
/// answers — so a connection that takes seconds is a connection that is not coming.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the app gets to send its answer head, and an ordinary body, and to take one.
///
/// It does **not** apply to a connection once it is handed over: an idle WebSocket is a working
/// WebSocket, and a stream that says nothing for a minute is not broken.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest answer head accepted from an app, in bytes.
const MAX_ANSWER_HEAD: usize = 64 * 1024;

/// How long a tunnel's copy waits on a socket before looking at the stop flag.
///
/// A blocked `read` cannot hear a flag, and a stream is idle most of the time, so the copy is
/// bounded instead: a quarter of a second of silence costs one wakeup and, when the server is
/// stopping, ends the connection. Polling a socket is cheap precisely because the read returns as
/// soon as a byte arrives — this adds no latency to anything that is saying something.
const TUNNEL_POLL: Duration = Duration::from_millis(250);

/// Where an app is: what it is called, and the port it is on right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub slug: String,
    pub port: u16,
}

impl Target {
    pub fn new(slug: impl Into<String>, port: u16) -> Self {
        Self {
            slug: slug.into(),
            port,
        }
    }

    /// The path prefix this app is served under.
    pub fn prefix(&self) -> String {
        rewrite::prefix(&self.slug)
    }
}

/// Why a request did not reach the app and come back.
///
/// These are three different things to the user, which is why they are three variants rather than
/// one `io::Error`: the app is not answering, the app answered something that is not HTTP, or the
/// forward itself is what broke.
#[derive(Debug)]
pub enum ForwardError {
    /// The connection did not happen. Usually "the app died since `start`".
    Connect { port: u16, source: io::Error },
    /// The connection happened and the exchange did not finish.
    Io(io::Error),
    /// The bytes that came back are not an HTTP response.
    Malformed(&'static str),
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect { port, source } => {
                write!(
                    f,
                    "the app's server on 127.0.0.1:{port} did not answer: {source}"
                )
            }
            Self::Io(source) => write!(f, "the exchange with the app failed: {source}"),
            Self::Malformed(what) => write!(f, "the app's answer is not HTTP: {what}"),
        }
    }
}

impl std::error::Error for ForwardError {}

/// What an app answered.
///
/// One variant per kind of answer, and the second one exists because a proxy that can only carry
/// bodies cannot carry a conversation. The third is the one deepnest produces itself, and it is
/// here rather than anywhere else because *what this connection does next* is one decision: the
/// server's loop matches on this type and must answer every case, so a kind kept somewhere else
/// would be a case that loop could forget.
#[derive(Debug)]
pub enum Answer {
    /// A complete answer: read whole, adapted to the prefix, written as one response.
    Full(Response),
    /// A head and a live connection to the app, passed on as the bytes arrive.
    Stream(Stream),
    /// A head now, and this process's own body produced afterwards.
    Produced(Produced),
}

impl Answer {
    /// The complete answer.
    ///
    /// # Panics
    ///
    /// When this answer is a connection, or a body that has not been produced yet. A caller that
    /// can only take a body has to ask first, and silently dropping a stream — or a body nobody
    /// ever wrote — would report success for bytes nobody received.
    #[track_caller]
    pub fn complete(self) -> Response {
        match self {
            Self::Full(response) => response,
            Self::Stream(_) => panic!("this answer is a connection, not a body"),
            Self::Produced(_) => panic!("this answer's body has not been produced yet"),
        }
    }
}

/// An answer this server writes itself, a piece at a time.
///
/// The two kinds above are the app's. This one is deepnest's own: a route that has something to say
/// before it has finished working out what to say, and a client that should see each piece as it
/// exists instead of the whole of it at the end. The head goes out first and the body is **called**
/// afterwards, so the difference between this and [`Answer::Full`] is not the bytes but when they
/// are produced — which is the only thing that makes "watch the answer being written" possible.
///
/// The head is already framed bytes rather than a [`Response`] for the reason [`Stream`] keeps its
/// head as bytes: a response would write a `Content-Length`, and the whole point here is that the
/// length is not known until the body has been produced. The answer is delimited by the connection
/// closing, which is what `Connection: close` in the head states.
pub struct Produced {
    head: Vec<u8>,
    body: ProducedBody,
}

/// The body of a [`Produced`] answer: written when it is time to write it, and not before.
///
/// Named rather than spelled into the field, because a reader who wants to know what a body *is*
/// here should not have to parse three sigils and a bound to find out.
type ProducedBody = Box<dyn FnOnce(&mut dyn Write) -> io::Result<()> + Send>;

impl Produced {
    /// An answer whose head is ready and whose body is produced when it is written.
    pub fn new(
        head: Vec<u8>,
        body: impl FnOnce(&mut dyn Write) -> io::Result<()> + Send + 'static,
    ) -> Self {
        Self {
            head,
            body: Box::new(body),
        }
    }

    /// The bytes the client receives first.
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// Write the head, then produce the body.
    ///
    /// The head is flushed before the body is called, because a client has to be reading an answer
    /// while the rest of it is being produced — a head held in a buffer until the body finished
    /// would be this type behaving exactly like the one it was added beside.
    pub fn write_to(self, out: &mut dyn Write, head_only: bool) -> io::Result<()> {
        out.write_all(&self.head)?;
        out.flush()?;
        if head_only {
            // `HEAD` promised no body, so the head is the whole answer — and the body is never
            // produced, which for a turn means it never runs.
            return Ok(());
        }
        (self.body)(out)
    }
}

impl std::fmt::Debug for Produced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The closure has no name to print, and wrapping the head's bytes in a `Debug` of their
        // own would fill a log line with a whole response head.
        f.debug_struct("Produced")
            .field("head_bytes", &self.head.len())
            .finish()
    }
}

/// Why an answer is being passed through instead of read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// `101 Switching Protocols`. From here the connection is a tunnel in both directions.
    Upgrade,
    /// `text/event-stream`. The app has more to say and no way to say when it is done.
    Events,
}

/// An app's response head, and the connection it arrived on.
///
/// The head is kept as bytes rather than as a [`Response`] on purpose: its framing headers —
/// `Transfer-Encoding: chunked`, `Connection` — are the framing of everything that follows, and a
/// `Response` would rewrite them into a length that does not exist.
#[derive(Debug)]
pub struct Stream {
    head: Vec<u8>,
    /// Bytes the app sent past its head while the head was being read. They are the first bytes of
    /// the stream, and dropping them would lose the beginning of the conversation.
    pending: Vec<u8>,
    upstream: TcpStream,
    kind: StreamKind,
}

impl Stream {
    /// A connection to hand over, with the head the browser gets first.
    pub fn new(upstream: TcpStream, head: Vec<u8>, kind: StreamKind) -> Self {
        Self {
            head,
            pending: Vec::new(),
            upstream,
            kind,
        }
    }

    pub fn kind(&self) -> StreamKind {
        self.kind
    }

    /// The bytes the browser receives first.
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// Write the head, then copy bytes both ways until either side stops.
    ///
    /// Two threads, one per direction, because a single one would have to poll both sockets and a
    /// WebSocket is idle in both directions at once: a thread that waits for the browser cannot
    /// hear the app. Each direction writes its own socket and no other, so no lock is needed.
    ///
    /// `stop` is how the server ends a connection it owns. Shutting the sockets down is not enough
    /// on its own — whether a blocked read on a duplicated handle wakes is a platform's business,
    /// and on Windows this was measured not to (`CI`, windows-latest, 2026-10-31: shutdown waited
    /// the full grace period) — so each direction also looks at the flag between reads.
    pub fn pump(
        self,
        client: &mut TcpStream,
        head_only: bool,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        // From here on, silence is normal, so the timeouts are the poll interval rather than a
        // deadline: a stream that says nothing for a minute is a working stream.
        let _ = self.upstream.set_read_timeout(Some(TUNNEL_POLL));
        let _ = self.upstream.set_write_timeout(None);
        let _ = client.set_read_timeout(Some(TUNNEL_POLL));
        let _ = client.set_write_timeout(None);

        client.write_all(&self.head)?;
        if head_only {
            // `HEAD` promised no body, so the head is the whole answer.
            let _ = client.shutdown(Shutdown::Write);
            return Ok(());
        }
        if !self.pending.is_empty() {
            client.write_all(&self.pending)?;
        }
        client.flush()?;

        let mut app_reader = self.upstream.try_clone()?;
        let mut app_writer = self.upstream;
        let mut client_reader = client.try_clone()?;
        let mut client_writer = client.try_clone()?;

        let upward_stop = Arc::clone(&stop);
        let upward = thread::spawn(move || {
            let result = copy(&mut client_reader, &mut app_writer, &upward_stop);
            // The app is told the conversation is over, so a server waiting for its next request
            // is not left holding a socket whose peer already left.
            let _ = app_writer.shutdown(Shutdown::Write);
            result
        });

        let downward = copy(&mut app_reader, &mut client_writer, &stop);
        // Same in this direction: half-close the browser's side so its reader sees the end of the
        // stream instead of waiting for a byte that will never come.
        let _ = client_writer.shutdown(Shutdown::Write);
        let upward = upward.join().unwrap_or(Ok(()));
        downward.and(upward)
    }
}

/// Copy until `from` ends or the server is stopping, reporting the first failure.
fn copy(from: &mut TcpStream, to: &mut TcpStream, stop: &AtomicBool) -> io::Result<()> {
    let mut buffer = [0u8; 8192];
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        match from.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                to.write_all(&buffer[..count])?;
                to.flush()?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // The poll interval elapsed. Silence is normal here, so this is not a failure; the
            // flag at the top of the loop is what decides whether to keep waiting.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        }
    }
}

/// How a request reaches an app.
///
/// A trait for the reason every other seam here is one: the behaviour worth testing in the router is
/// *which app a request is sent to and what is answered when there is no app to send it to*, and a
/// test that needed a running app on the exact port the supervisor allocated would be measuring the
/// kernel's port allocator.
pub trait Forwarder: Send + Sync {
    fn forward(&self, target: &Target, request: &Request) -> Result<Answer, ForwardError>;
}

/// The real forward: the one in this module.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdForwarder;

impl Forwarder for StdForwarder {
    fn forward(&self, target: &Target, request: &Request) -> Result<Answer, ForwardError> {
        forward(target, request)
    }
}

/// Forward one request to the app and return its answer, adapted to the prefix.
pub fn forward(target: &Target, request: &Request) -> Result<Answer, ForwardError> {
    let upgrade = is_upgrade(request);
    let head = if upgrade {
        upstream_handshake(target, request)
    } else {
        upstream_request(target, request)
    };

    // The connect is attempted separately from the exchange, and the separation is the point: "the
    // app is not listening" and "the app stopped mid-answer" want different words and different
    // next steps, and a single `io::Result` collapses them into one.
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, target.port)),
        CONNECT_TIMEOUT,
    )
    .map_err(|source| ForwardError::Connect {
        port: target.port,
        source,
    })?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    stream
        .write_all(head.as_bytes())
        .map_err(ForwardError::Io)?;
    if !upgrade {
        // Half-close: an ordinary request has nothing more to send, so the app is told the request
        // is complete and can answer. A handshake is the opposite — the connection is about to
        // carry traffic both ways, and closing it would end the upgrade before it began.
        stream.shutdown(Shutdown::Write).map_err(ForwardError::Io)?;
    }

    let (raw_head, pending) = read_head(&mut stream).map_err(ForwardError::Io)?;
    let head = parse_head(&raw_head)?;

    // The decision is made on the head alone, because the body of a stream has no end to read to.
    if let Some(kind) = stream_kind(&head) {
        return Ok(Answer::Stream(Stream {
            head: raw_head,
            pending,
            upstream: stream,
            kind,
        }));
    }

    let raw = read_all(&mut stream, pending).map_err(ForwardError::Io)?;
    let body = complete(&head, raw)?;
    Ok(Answer::Full(adapt(target, &head, body)))
}

/// Whether this request asks to stop speaking request/response.
///
/// Both headers are required, in the spellings RFC 9110 allows: `Upgrade: websocket` on its own is
/// a hint, and `Connection` may carry a list (`keep-alive, Upgrade`).
pub fn is_upgrade(request: &Request) -> bool {
    request.header("upgrade").is_some()
        && request.header("connection").is_some_and(|value| {
            value
                .to_ascii_lowercase()
                .split(',')
                .any(|token| token.trim() == "upgrade")
        })
}

/// The request the app receives for an ordinary exchange.
///
/// The app's own path, its own headers, and `Connection: close` — this forward reads one answer and
/// then stops, so asking the app to hold the connection open would only leave a socket waiting for
/// a second request that never comes.
fn upstream_request(target: &Target, request: &Request) -> String {
    let mut head = request_line(target, request);
    // The app must see itself, not deepnest's port: a framework that builds absolute URLs from the
    // Host header would otherwise send the browser to the panel.
    head.push_str(&format!("host: 127.0.0.1:{}\r\n", target.port));
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("host") || is_hop_by_hop(name) {
            continue;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("connection: close\r\n\r\n");
    head
}

/// The request the app receives when the client asked to upgrade.
///
/// Deliberately **not** [`upstream_request`] with a flag. For an ordinary request `Connection` and
/// `Upgrade` are hop-by-hop and stripping them is correct; for a handshake they *are* the message,
/// and an app that does not receive them answers `400` instead of `101`. So everything crosses —
/// including `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Sec-WebSocket-Protocol` and `Origin`,
/// which the app needs verbatim to compute `Sec-WebSocket-Accept` and to check where the browser
/// came from — and the only header changed is `Host`, for the usual reason.
fn upstream_handshake(target: &Target, request: &Request) -> String {
    let mut head = request_line(target, request);
    head.push_str(&format!("host: 127.0.0.1:{}\r\n", target.port));
    for (name, value) in &request.headers {
        // `Host` is replaced above. `Content-Length` describes a body this layer never read, and a
        // handshake has none: forwarding `0` is harmless but forwarding a stale number is not.
        if name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    head
}

/// The request line, with the prefix stripped and the query kept.
fn request_line(target: &Target, request: &Request) -> String {
    let path = rewrite::upstream_path(&target.prefix(), &request.path);
    let mut head = String::with_capacity(320);
    head.push_str(request.method.as_str());
    head.push(' ');
    head.push_str(&path);
    if !request.query.is_empty() {
        head.push('?');
        head.push_str(&request.query);
    }
    head.push_str(" HTTP/1.1\r\n");
    head
}

/// The whole exchange against an address: connect, send, read. What a test points at a real app.
pub fn exchange(addr: SocketAddr, head: &[u8]) -> io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(head)?;
    // Half-close: the app is told the request is complete, and can still answer.
    stream.shutdown(Shutdown::Write)?;
    read_all(&mut stream, Vec::new())
}

/// Read the answer head, and keep whatever arrived after it.
///
/// Split from [`read_all`] because the head is the last thing this layer may read: past it, an
/// answer is either a body with a length or a connection with none.
fn read_head(stream: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(end) = buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|at| at + 4)
        {
            let rest = buffer.split_off(end);
            return Ok((buffer, rest));
        }
        if buffer.len() > MAX_ANSWER_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the app's answer head is larger than any real head",
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the app closed before finishing its answer head",
                ))
            }
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Read to the end of the connection, starting from bytes already in hand.
fn read_all(stream: &mut TcpStream, mut raw: Vec<u8>) -> io::Result<Vec<u8>> {
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(raw),
            Ok(count) => raw.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// An answer head, as the app sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Head {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn parse_head(raw: &[u8]) -> Result<Head, ForwardError> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(ForwardError::Malformed(
            "the answer ended before its headers did",
        ))?;
    let text = std::str::from_utf8(&raw[..split])
        .map_err(|_| ForwardError::Malformed("the answer's head is not text"))?;

    let mut lines = text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or(ForwardError::Malformed("the answer had no status line"))?;
    let mut fields = status_line.split(' ');
    let version = fields.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(ForwardError::Malformed("the status line is not HTTP/1.x"));
    }
    let status: u16 = fields
        .next()
        .and_then(|code| code.parse().ok())
        .filter(|code| (100..600).contains(code))
        .ok_or(ForwardError::Malformed("the status line carries no status"))?;

    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    Ok(Head { status, headers })
}

/// Whether this answer is a connection rather than a body.
///
/// Two shapes, and neither can be recognised from anything but the head: a `101` is a tunnel, and
/// `text/event-stream` is a stream whose end is the user closing the tab. Reading on to find out
/// would be reading until the browser is gone.
fn stream_kind(head: &Head) -> Option<StreamKind> {
    if head.status == 101 && head.header("upgrade").is_some() {
        return Some(StreamKind::Upgrade);
    }
    if head.header("content-type").is_some_and(|value| {
        value
            .trim()
            .to_ascii_lowercase()
            .starts_with("text/event-stream")
    }) {
        return Some(StreamKind::Events);
    }
    None
}

/// The body of an answer that has one, framed as a body.
fn complete(head: &Head, mut body: Vec<u8>) -> Result<Vec<u8>, ForwardError> {
    // A body with no length is delimited by the end of the connection, and that is what arrived.
    // A chunked body is not: its framing is part of the bytes, so it has to be decoded before
    // anything downstream can treat it as a body.
    if head
        .header("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        return decode_chunked(&body);
    }
    if let Some(length) = head.header("content-length").and_then(|v| v.parse().ok()) {
        // Trust the announced length over the connection: an app that closes early has still told
        // us how much it meant to send, and a truncated body is a broken answer either way.
        body.truncate(length);
    }
    Ok(body)
}

/// Undo HTTP's chunked framing.
fn decode_chunked(raw: &[u8]) -> Result<Vec<u8>, ForwardError> {
    let mut body = Vec::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") else {
            return Err(ForwardError::Malformed(
                "a chunk ended without its size line",
            ));
        };
        let size_line = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| ForwardError::Malformed("a chunk size is not text"))?;
        // `1a;ext=value` is legal: the size ends at the first `;`.
        let digits = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(digits, 16)
            .map_err(|_| ForwardError::Malformed("a chunk size is not hexadecimal"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        if rest.len() < size + 2 {
            return Err(ForwardError::Malformed("a chunk is shorter than its size"));
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

/// The app's answer, as the browser must see it under the prefix.
fn adapt(target: &Target, head: &Head, answer_body: Vec<u8>) -> Response {
    let prefix = target.prefix();
    let content_type = head
        .header("content-type")
        .unwrap_or("application/octet-stream")
        .to_string();

    // Only a page can carry a dialog, and only a dialog that went in is worth loosening a policy
    // for: `injected` is the whole reason the CSP rewrite below is conditional.
    let mut body = answer_body;
    let mut injected = false;
    if content_type.to_ascii_lowercase().starts_with("text/html") {
        if let Ok(page) = std::str::from_utf8(&body) {
            // Two rewrites of the same text, and the second sees the first one's output: the base
            // has to be ahead of every URL, and the dialog after everything the app rendered.
            let based = rewrite::inject_base(&prefix, page);
            let candidate = based.as_deref().unwrap_or(page);
            match dialog::inject(&target.slug, candidate) {
                Some(rewritten) => {
                    body = rewritten.into_bytes();
                    injected = true;
                }
                // The page already carries a dialog: keep the base if there was one, and loosen
                // nothing.
                None => {
                    if let Some(rewritten) = based {
                        body = rewritten.into_bytes();
                    }
                }
            }
        }
    }

    let mut response = Response::new(head.status, content_type, body);
    for (name, value) in &head.headers {
        if is_hop_by_hop(name) || name.eq_ignore_ascii_case("content-type") {
            continue;
        }
        if name.eq_ignore_ascii_case("location") {
            response = response.header(name.clone(), rewrite::location(&prefix, value));
        } else if name.eq_ignore_ascii_case("set-cookie") {
            response = response.header(name.clone(), rewrite::set_cookie(&prefix, value));
        } else if name.eq_ignore_ascii_case("content-security-policy") {
            // Only for the page the dialog reached, and only ever widened: an app that said
            // nothing about scripts keeps saying nothing, because an absent policy restricts
            // nothing and inventing one would.
            if injected {
                response = response.header(name.clone(), dialog::policy(value));
            } else {
                response = response.header(name.clone(), value.clone());
            }
        } else {
            response = response.header(name.clone(), value.clone());
        }
    }
    response
}

/// Only `HEAD` and `GET` reach an app through this layer.
///
/// The hand-written layer reads one request per connection and no body, so a `POST` cannot be
/// forwarded faithfully: its content would be dropped on the floor and the app would be told a
/// request arrived that said nothing. A `GET` is no longer one answer, though — it may become a
/// WebSocket tunnel or an event stream — which is why the method list is here and the shape of the
/// answer is not.
pub fn can_forward(method: Method) -> bool {
    matches!(method, Method::Get | Method::Head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Instant;

    fn get(path: &str) -> Request {
        Request {
            method: Method::Get,
            path: path.to_string(),
            query: String::new(),
            headers: vec![("host".to_string(), "127.0.0.1:4270".to_string())],
            // A forwarded request carries no body: `can_forward` refuses every method that could.
            body: Vec::new(),
        }
    }

    fn handshake(path: &str) -> Request {
        Request {
            method: Method::Get,
            path: path.to_string(),
            query: String::new(),
            headers: vec![
                ("host".to_string(), "127.0.0.1:4270".to_string()),
                ("connection".to_string(), "keep-alive, Upgrade".to_string()),
                ("upgrade".to_string(), "websocket".to_string()),
                (
                    "sec-websocket-key".to_string(),
                    "dGhlIHNhbXBsZSBub25jZQ==".to_string(),
                ),
                ("sec-websocket-version".to_string(), "13".to_string()),
                ("origin".to_string(), "http://127.0.0.1:4270".to_string()),
            ],
            body: Vec::new(),
        }
    }

    /// Read a request head from an accepted connection.
    fn read_head_of(stream: &mut TcpStream) -> Vec<u8> {
        let mut seen = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    seen.extend_from_slice(&chunk[..count]);
                    if seen.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        seen
    }

    /// An app that answers every request with the same bytes, and one request read.
    fn serving(answer: &'static [u8]) -> (SocketAddr, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let seen = read_head_of(&mut stream);
            let _ = stream.write_all(answer);
            let _ = stream.flush();
            seen
        });
        (addr, handle)
    }

    /// One response header, by case-insensitive name.
    fn header(response: &Response, name: &str) -> Option<String> {
        response
            .headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    fn forward_to(addr: SocketAddr, request: &Request) -> Result<Answer, ForwardError> {
        let target = Target::new("notes", addr.port());
        let head = if is_upgrade(request) {
            upstream_handshake(&target, request)
        } else {
            upstream_request(&target, request)
        };
        let raw = exchange(addr, head.as_bytes()).map_err(|source| ForwardError::Connect {
            port: addr.port(),
            source,
        })?;
        let (raw_head, rest) = {
            let end = raw
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|at| at + 4)
                .ok_or(ForwardError::Malformed("no head"))?;
            (raw[..end].to_vec(), raw[end..].to_vec())
        };
        let parsed = parse_head(&raw_head)?;
        if let Some(kind) = stream_kind(&parsed) {
            let upstream = TcpStream::connect(addr).unwrap();
            return Ok(Answer::Stream(Stream {
                head: raw_head,
                pending: rest,
                upstream,
                kind,
            }));
        }
        let body = complete(&parsed, rest)?;
        Ok(Answer::Full(adapt(&target, &parsed, body)))
    }

    #[test]
    fn the_target_knows_the_prefix_it_is_served_under() {
        assert_eq!(Target::new("notes", 4271).prefix(), "/app/notes");
    }

    #[test]
    fn the_app_receives_its_own_path_and_its_own_host() {
        let (addr, handle) = serving(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi");
        let response = forward_to(addr, &get("/app/notes/api/items"))
            .unwrap()
            .complete();
        assert_eq!(response.status, 200);

        let seen = String::from_utf8(handle.join().unwrap()).unwrap();
        assert!(seen.starts_with("GET /api/items HTTP/1.1\r\n"), "{seen}");
        assert!(
            seen.to_ascii_lowercase()
                .contains(&format!("host: 127.0.0.1:{}", addr.port())),
            "{seen}"
        );
        // The client's Host named the panel. The app must not learn it.
        assert!(!seen.contains("127.0.0.1:4270"), "{seen}");
        assert!(
            seen.to_ascii_lowercase().contains("connection: close"),
            "{seen}"
        );
    }

    #[test]
    fn the_query_string_survives_the_hop() {
        let (addr, handle) = serving(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
        let mut request = get("/app/notes/search");
        request.query = "q=wal&page=2".to_string();
        forward_to(addr, &request).unwrap();
        let seen = String::from_utf8(handle.join().unwrap()).unwrap();
        assert!(
            seen.starts_with("GET /search?q=wal&page=2 HTTP/1.1\r\n"),
            "{seen}"
        );
    }

    #[test]
    fn a_page_comes_back_with_its_base_and_a_redirect_with_its_prefix() {
        let answer = b"HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: 46\r\n\r\n<html><head></head><body>hi</body></html>      ";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/")).unwrap().complete();
        let body = String::from_utf8(response.body.clone()).unwrap();
        assert!(body.contains("<base href=\"/app/notes/\">"), "{body}");
        assert_eq!(response.content_type, "text/html; charset=utf-8");
    }

    #[test]
    fn a_redirect_comes_back_pointing_inside_the_app() {
        let answer = b"HTTP/1.1 302 Found\r\nlocation: /login\r\ncontent-length: 0\r\n\r\n";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/private"))
            .unwrap()
            .complete();
        assert_eq!(response.status, 302);
        assert!(
            response
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("location")
                    && value == "/app/notes/login"),
            "{:?}",
            response.headers
        );
    }

    #[test]
    fn a_chunked_body_is_decoded_rather_than_forwarded_as_framing() {
        // What a Node or axum app does by default for a page it did not measure.
        let answer = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\ncontent-type: text/plain\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/")).unwrap().complete();
        assert_eq!(response.body, b"hello world");
    }

    #[test]
    fn an_html_page_carries_the_dialog_and_a_policy_that_allows_it() {
        // The pair the design asks for: the ball goes in every page an app answers, and the app's
        // own policy is widened by exactly what the ball needs to load.
        let answer = b"HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\n\
                       content-security-policy: default-src 'none'\r\n\r\n\
                       <html><body><p>hi</p></body></html>";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/")).unwrap().complete();
        let body = String::from_utf8(response.body.clone()).unwrap();

        assert!(
            body.contains(r#"<meta name="deepnest-app" content="notes">"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<script src="/dialog.js" defer></script>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<link rel="stylesheet" href="/dialog.css">"#),
            "{body}"
        );
        // And the base is still there, ahead of it.
        assert!(body.contains("<base href=\"/app/notes/\">"), "{body}");

        assert_eq!(
            header(&response, "content-security-policy").as_deref(),
            Some("default-src 'none'; script-src 'self'; style-src 'self'")
        );
    }

    #[test]
    fn a_non_page_carries_neither_the_dialog_nor_a_widened_policy() {
        // A JSON answer cannot hold a dialog, so its policy must not move either: loosening one
        // for a dialog the page does not carry is loosening it for nothing.
        let answer = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                       content-security-policy: script-src 'none'\r\n\r\n{\"items\":[]}";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/api/items"))
            .unwrap()
            .complete();
        assert_eq!(response.body, b"{\"items\":[]}");
        assert_eq!(
            header(&response, "content-security-policy").as_deref(),
            Some("script-src 'none'")
        );
    }

    #[test]
    fn an_app_that_set_no_policy_is_not_given_one() {
        // An absent policy restricts nothing, so inventing one would only take things away.
        let answer =
            b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n\r\n<html><body></body></html>";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/")).unwrap().complete();
        assert_eq!(header(&response, "content-security-policy"), None);
        // The dialog is still injected: the two decisions are separate.
        assert!(String::from_utf8_lossy(&response.body).contains("/dialog.js"));
    }

    #[test]
    fn a_page_that_already_carries_the_dialog_keeps_its_policy() {
        // The second hop's case: nothing was injected, so nothing is loosened.
        let answer = b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n\
                       content-security-policy: script-src 'none'\r\n\r\n\
                       <html><body><meta name=\"deepnest-app\" content=\"notes\"></body></html>";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/")).unwrap().complete();
        assert_eq!(
            header(&response, "content-security-policy").as_deref(),
            Some("script-src 'none'")
        );
        let body = String::from_utf8(response.body.clone()).unwrap();
        assert_eq!(body.matches("deepnest-app").count(), 1, "{body}");
    }

    #[test]
    fn a_non_html_answer_is_handed_over_byte_for_byte() {
        let answer = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 12\r\n\r\n{\"items\":[]}";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/api/items"))
            .unwrap()
            .complete();
        assert_eq!(response.body, b"{\"items\":[]}");
        assert_eq!(response.content_type, "application/json");
    }

    #[test]
    fn an_answer_that_is_not_http_is_named_rather_than_forwarded() {
        let (addr, _handle) = serving(b"this is not a response");
        match forward_to(addr, &get("/app/notes/")) {
            Err(ForwardError::Malformed(_)) => {}
            Ok(other) => panic!("{other:?}"),
            Err(other) => panic!("expected a malformed answer, got {other}"),
        }
    }

    #[test]
    fn a_port_nothing_listens_on_is_a_connect_failure_naming_the_port() {
        // Bound and dropped: the port is free, so the connection is refused.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let target = Target::new("notes", port);
        match forward(&target, &get("/app/notes/")) {
            Err(ForwardError::Connect { port: reported, .. }) => assert_eq!(reported, port),
            Err(other) => panic!("expected a connect failure, got {other}"),
            Ok(other) => panic!("expected a connect failure, got {other:?}"),
        }
    }

    #[test]
    fn only_the_two_methods_this_layer_can_carry_are_forwarded() {
        assert!(can_forward(Method::Get));
        assert!(can_forward(Method::Head));
        assert!(!can_forward(Method::Post));
        assert!(!can_forward(Method::Delete));
    }

    #[test]
    fn an_upgrade_is_recognised_from_both_headers() {
        assert!(is_upgrade(&handshake("/app/notes/socket")));

        // `Upgrade` alone is a hint, and a plain request is not one at all.
        let mut hint = handshake("/app/notes/socket");
        hint.headers.retain(|(name, _)| name != "connection");
        assert!(!is_upgrade(&hint));

        assert!(!is_upgrade(&get("/app/notes/socket")));
    }

    #[test]
    fn the_handshake_carries_the_headers_the_app_needs_to_accept_it() {
        // The rule this test exists for: hop-by-hop headers are stripped for an ordinary request
        // and must survive for a handshake, because here they are the message.
        let (addr, handle) = serving(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n");
        let request = handshake("/app/notes/socket");
        let _ = forward_to(addr, &request);

        let seen = String::from_utf8(handle.join().unwrap()).unwrap();
        let lowered = seen.to_ascii_lowercase();
        assert!(
            lowered.contains("connection: keep-alive, upgrade"),
            "{seen}"
        );
        assert!(lowered.contains("upgrade: websocket"), "{seen}");
        // The key is compared lowercased on both sides: it is base64, and base64 is case-sensitive.
        assert!(
            lowered.contains(&"sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==".to_ascii_lowercase()),
            "{seen}"
        );
        assert!(lowered.contains("sec-websocket-version: 13"), "{seen}");
        assert!(lowered.contains("origin: http://127.0.0.1:4270"), "{seen}");
        // And the two things that must never happen to a handshake: being told to close, and the
        // panel's own port arriving as Host.
        assert!(!lowered.contains("connection: close"), "{seen}");
        assert!(!lowered.contains("host: 127.0.0.1:4270"), "{seen}");
    }

    #[test]
    fn an_upgrade_the_app_refuses_is_still_a_whole_answer() {
        // A 400 to a handshake is an ordinary response, and the browser must see it.
        let (addr, _handle) =
            serving(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 5\r\n\r\nnope!");
        let answer = forward_to(addr, &handshake("/app/notes/socket")).unwrap();
        match answer {
            Answer::Full(response) => {
                assert_eq!(response.status, 400);
                assert_eq!(response.body, b"nope!");
            }
            Answer::Stream(_) => panic!("a 400 was treated as a tunnel"),
            // Forwarding an app's bytes never yields an answer this process produces itself.
            Answer::Produced(_) => panic!("a forwarded answer was produced locally"),
        }
    }

    /// An app that starts a stream and holds it open until it is told to finish.
    fn streaming_app() -> (SocketAddr, mpsc::Sender<()>, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let (finish, wait) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let seen = read_head_of(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\n6\r\ndata:1\r\n\r\n",
                )
                .unwrap();
            stream.flush().unwrap();
            // Held open on purpose: this is what a real event stream does, and what the forward
            // must not wait for.
            let _ = wait.recv();
            let _ = stream.write_all(b"0\r\n\r\n");
            seen
        });
        (addr, finish, handle)
    }

    #[test]
    fn a_streamed_answer_comes_back_before_the_stream_ends() {
        let (addr, finish, handle) = streaming_app();

        // The app is blocked until `finish` is sent, so an answer that arrives here is an answer
        // that was not read to the end — which is the whole reason a stream is not a body.
        let answer = forward_to_streaming(addr);

        let stream = match answer {
            Answer::Stream(stream) => stream,
            Answer::Full(response) => panic!("an event stream was read whole: {response:?}"),
            Answer::Produced(_) => panic!("a forwarded answer was produced locally"),
        };
        assert_eq!(stream.kind(), StreamKind::Events);
        assert!(stream.head().starts_with(b"HTTP/1.1 200 OK\r\n"));
        // The head's own framing is kept: it is the framing of everything after it.
        assert!(String::from_utf8_lossy(stream.head())
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked"));

        // Dropping the connection and the app's release together lets its thread end. What the
        // bytes do once they are being copied is `a_tunnel_carries_bytes_in_both_directions`.
        drop(stream);
        drop(finish);
        handle.join().unwrap();
    }

    /// A stop flag that never goes up: the ordinary case, where a tunnel ends because a side does.
    fn running() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// The forward against a real app, through the same code path the server uses.
    fn forward_to_streaming(addr: SocketAddr) -> Answer {
        let target = Target::new("notes", addr.port());
        forward(&target, &get("/app/notes/events")).unwrap()
    }

    #[test]
    fn a_tunnel_carries_bytes_in_both_directions() {
        // What an upgraded connection is: not one answer, but a conversation.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let app_addr = listener.local_addr().unwrap();
        let app = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut chunk = [0u8; 1024];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(count) => {
                        // Echo, which is the smallest thing a frame protocol's tunnel must carry.
                        let _ = stream.write_all(&chunk[..count]);
                    }
                    Err(_) => return,
                }
            }
        });

        let upstream = TcpStream::connect(app_addr).unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let stream = Stream::new(
            upstream,
            b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\r\n"
                .to_vec(),
            StreamKind::Upgrade,
        );

        let browser_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let browser_addr = browser_listener.local_addr().unwrap();
        let mut server_side = TcpStream::connect(browser_addr).unwrap();
        let (mut browser, _) = browser_listener.accept().unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let pump = thread::spawn(move || {
            let _ = stream.pump(&mut server_side, false, running());
        });

        // The head arrives before anything is sent: the browser is told the upgrade happened.
        let mut head = [0u8; 128];
        let read = browser.read(&mut head).unwrap();
        assert!(
            String::from_utf8_lossy(&head[..read]).starts_with("HTTP/1.1 101"),
            "{}",
            String::from_utf8_lossy(&head[..read])
        );

        // And then the conversation: the browser's bytes reach the app and come back.
        browser.write_all(b"frame-one").unwrap();
        let mut echoed = [0u8; 9];
        browser.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"frame-one");

        // The browser leaving ends the tunnel: both directions stop, and the pump returns.
        browser.shutdown(Shutdown::Both).unwrap();
        pump.join().unwrap();
        app.join().unwrap();
    }

    #[test]
    fn a_silent_tunnel_ends_when_the_server_stops() {
        // The defect this exists for: a tunnel is idle most of the time, so both of its copies are
        // parked in a read. Closing the socket does not reliably wake them — a blocked read on a
        // duplicated handle is a platform's business, and windows-latest was measured serving out
        // the whole shutdown grace period with a quiet WebSocket open. So the pump looks at the
        // flag between reads, and what is asserted here is that a conversation saying nothing at
        // all still ends promptly.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let app_addr = listener.local_addr().unwrap();
        let app = thread::spawn(move || {
            // Hold the connection open and say nothing: an idle WebSocket.
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(10));
        });

        let upstream = TcpStream::connect(app_addr).unwrap();
        let stream = Stream::new(
            upstream,
            b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\r\n"
                .to_vec(),
            StreamKind::Upgrade,
        );

        let browser_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let browser_addr = browser_listener.local_addr().unwrap();
        let mut server_side = TcpStream::connect(browser_addr).unwrap();
        let (mut browser, _) = browser_listener.accept().unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let pump_stop = Arc::clone(&stop);
        let pump = thread::spawn(move || stream.pump(&mut server_side, false, pump_stop));

        // The head still arrives, so this is a tunnel rather than a connection that was dropped.
        let mut head = [0u8; 128];
        let read = browser.read(&mut head).unwrap();
        assert!(String::from_utf8_lossy(&head[..read]).starts_with("HTTP/1.1 101"));

        let started = Instant::now();
        stop.store(true, Ordering::SeqCst);
        let ended = pump.join().expect("the pump thread must not panic");
        ended.expect("an idle tunnel ends cleanly when the server stops");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "an idle tunnel took {:?} to notice the server was stopping",
            started.elapsed()
        );

        // And the browser sees the tunnel end rather than being left holding it.
        let mut rest = Vec::new();
        browser.read_to_end(&mut rest).unwrap();
        drop(app);
    }

    #[test]
    fn a_head_only_tunnel_sends_the_head_and_no_frames() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let app_addr = listener.local_addr().unwrap();
        let app = thread::spawn(move || {
            let _ = listener.accept();
        });

        let upstream = TcpStream::connect(app_addr).unwrap();
        let stream = Stream::new(
            upstream,
            b"HTTP/1.1 101 Switching Protocols\r\n\r\n".to_vec(),
            StreamKind::Upgrade,
        );

        let browser_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let browser_addr = browser_listener.local_addr().unwrap();
        let mut server_side = TcpStream::connect(browser_addr).unwrap();
        let (mut browser, _) = browser_listener.accept().unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        stream.pump(&mut server_side, true, running()).unwrap();
        let mut received = Vec::new();
        browser.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"HTTP/1.1 101 Switching Protocols\r\n\r\n");
        app.join().unwrap();
    }

    /// A produced answer writes its head before its body exists — which is the whole difference
    /// between it and a complete response, and the reason it was added.
    #[test]
    fn a_produced_answer_writes_the_head_before_the_body_is_produced() {
        let produced = Produced::new(
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec(),
            |out| {
                out.write_all(b"one\n")?;
                out.write_all(b"two\n")
            },
        );
        let mut written = Vec::new();
        produced.write_to(&mut written, false).unwrap();
        assert_eq!(
            String::from_utf8(written).unwrap(),
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\none\ntwo\n"
        );
    }

    /// `HEAD` promised no body, so a body that would have taken a minute is never produced at all.
    #[test]
    fn a_head_only_produced_answer_never_produces_the_body() {
        let produced = Produced::new(b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), |_| {
            panic!("the body of a HEAD answer was produced")
        });
        let mut written = Vec::new();
        produced.write_to(&mut written, true).unwrap();
        assert_eq!(written, b"HTTP/1.1 200 OK\r\n\r\n");
    }

    /// A caller that can only take a body must not silently get none: dropping a body nobody wrote
    /// would report success for bytes that do not exist.
    #[test]
    #[should_panic(expected = "has not been produced yet")]
    fn a_produced_answer_is_not_a_body() {
        let _ = Answer::Produced(Produced::new(b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), |_| {
            Ok(())
        }))
        .complete();
    }
}
