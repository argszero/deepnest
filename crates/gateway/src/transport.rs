//! The outbound half of the door: the HTTP client a model call goes through.
//!
//! `crates/llm` decides *what* a failure means and never touches a socket; this is the adapter
//! that moves the bytes, which the design puts here (`design.md`, the agent's turn loop: "the
//! adapter belongs to `crates/gateway`"). One module, one trait, so every rule in `llm` stays
//! testable with no network.
//!
//! **It carries both schemes, and neither one degrades into the other.** `http://` goes over
//! [`PlainHttp`]: a `TcpStream` and no dependency, which is what a local model server
//! (`http://127.0.0.1:11434` and friends) needs. `https://` goes over [`TlsHttp`], which hands the
//! same bytes to `native-tls` and so verifies the endpoint against **this machine's own trust
//! store** — Security framework on macOS, SChannel on Windows, OpenSSL on Linux. That is the trust
//! the user's browser already gives, and the only one a machine behind a corporate proxy has: a
//! bundled root list would refuse the proxy's own certificate and the user would read that as
//! deepnest being broken. [`HttpTransport`] is the two together, and it is what the binary wires
//! in. An `https` endpoint is never sent by [`PlainHttp`] and an `http` one never by [`TlsHttp`],
//! because a plaintext request aimed at a TLS port is a hang the user blames on their key.
//!
//! **A refused certificate says which refusal it was.** An untrusted certificate, a certificate
//! for another name, and an endpoint that cannot be reached are three situations with three
//! different fixes, so they are three different messages. What the platform reports is a sentence
//! rather than a code — `native-tls` exposes no error kind — so the split is read out of that
//! sentence (see [`handshake_failure`]); where a platform does not name the mismatch, the message
//! still says the certificate was refused instead of falling back to "connection failed".
//!
//! **Two verbs, and the second one is not the model client's.** A model call is a `POST` carrying a
//! JSON body; the skills release check is a `GET` carrying none (`design.md` §9: "`GET` was not
//! borrowed from the model client, and does not need to be"). So the verb is a parameter of the one
//! exchange below, `llm::HttpRequest` keeps its single posted shape, and `llm::Transport` keeps its
//! `post` and is given no `get`. A `GET` is sent with **no `Content-Length` at all** rather than with
//! a length of zero, because a request that declares a body is one whose next bytes a server waits
//! for, and this one has none to send.
//!
//! Three rules the code below is shaped by:
//!
//! - **A body shorter than its `Content-Length` is an error, not a short answer.** A connection
//!   that dies mid-answer would otherwise be parsed as a complete completion, and a truncated
//!   tool call is a call the agent then runs.
//! - **`Connection: close` and read to the end.** One request per connection: a keep-alive pool is
//!   a cache of sockets whose staleness produces failures that look like the endpoint's fault.
//! - **No redirect is followed.** A 3xx is returned as a 3xx, because a model endpoint that
//!   redirects is answering from somewhere the user did not configure, and silently going there
//!   spends their key at a host they never chose.
//! - **A stream is delivered as it arrives, and that is the whole point of `post_stream`.** The
//!   body is decoded as it is read — de-chunked, held to its declared length, or read to the
//!   close — and each complete server-sent event is handed to the caller **before the next byte
//!   has been asked for**. An adapter that read the body whole and then replayed the events would
//!   satisfy the same signature and deliver nothing: the answer would appear at the end, exactly
//!   as the non-streaming path already does. Two smaller consequences follow from the same
//!   requirement: a character can be split across two reads (so the decoder is fed only complete
//!   UTF-8 and the rest waits), and a refusal is the one answer read whole (because
//!   [`LlmError::classify`] needs the document, and a refusal is not a stream).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use deepnest_llm::{HttpRequest, HttpResponse, LlmError, SseDecoder, SseEvent, Transport};
use native_tls::{HandshakeError, TlsConnector, TlsStream};

/// How long a connection may take to establish, and how long a read may block.
///
/// The read budget is generous because a model can think for a while before its first byte; the
/// connect budget is short because a machine that cannot reach an address in ten seconds is not
/// going to reach it.
const CONNECT: Duration = Duration::from_secs(10);
const IO: Duration = Duration::from_secs(180);

/// The plaintext HTTP adapter: `http://` only, and a `TcpStream`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlainHttp;

impl PlainHttp {
    pub fn new() -> Self {
        Self
    }

    /// The target of a call this adapter can carry, or the refusal naming the half that can.
    ///
    /// Shared by both verbs and by both paths — whole answer and stream — so a scheme this adapter
    /// cannot carry is refused in one place and in one wording, before any socket is opened.
    fn target(&self, url: &str, headers: &[(String, String)]) -> Result<Target, LlmError> {
        check_headers(headers)?;
        let target = parse(url)?;
        if target.scheme == Scheme::Https {
            return Err(LlmError::transport(format!(
                "`{url}` is https, and this adapter carries plaintext only: use `HttpTransport`, which \
                 picks the half that matches the scheme"
            )));
        }
        Ok(target)
    }

    /// One exchange over a plaintext connection, for whichever verb the caller named.
    ///
    /// The scheme is refused by [`Self::target`], because the two verbs have the same reason:
    /// sending these bytes in the clear at a TLS port is either a hang the user would blame on
    /// their key or a key sent in the clear. Neither is a fallback.
    fn send(
        &self,
        verb: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<HttpResponse, LlmError> {
        let target = self.target(url, headers)?;
        let mut stream = connect(&target, url)?;
        let raw = exchange(&mut stream, verb, &target, headers, body)?;
        read_response(&raw)
    }

    /// The streaming path over the same connection: the answer is decoded and delivered while it is
    /// being written. [`stream_answer`] carries the rules.
    fn stream(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
        on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        let target = self.target(url, headers)?;
        let mut stream = connect(&target, url)?;
        stream_answer(&mut stream, "POST", &target, headers, Some(body), on_event)
    }

    /// A `GET`: the URL and whatever headers the endpoint wants, and no body.
    ///
    /// This belongs to the adapter rather than to `deepnest_llm::Transport`, which is the model
    /// client's seam: the release check that asks a `GET` has a seam of its own
    /// (`crates/skills::release::Releases`, an `owner/name` in and the answer's body out), so
    /// nothing about it is shaped like a model call. There is no body parameter because a `GET`
    /// that carried one would be a `POST` spelled differently.
    pub fn get(&self, url: &str, headers: &[(String, String)]) -> Result<HttpResponse, LlmError> {
        self.send("GET", url, headers, None)
    }
}

/// Which scheme a URL names. The two are carried by different code below, and each adapter refuses
/// the other's, so neither can quietly send plaintext at a TLS port or run TLS at a plaintext one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

/// A URL taken apart: where to connect, and what to ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    scheme: Scheme,
    host: String,
    port: u16,
    path: String,
}

fn parse(url: &str) -> Result<Target, LlmError> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        LlmError::transport(format!(
            "`{url}` has no scheme — a model endpoint is `http://host[:port][/path]` or the same \
             over `https`"
        ))
    })?;
    let scheme = if scheme.eq_ignore_ascii_case("http") {
        Scheme::Http
    } else if scheme.eq_ignore_ascii_case("https") {
        Scheme::Https
    } else {
        return Err(LlmError::transport(format!(
            "`{url}` uses `{scheme}`, and only http and https are supported"
        )));
    };
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(LlmError::transport(format!("`{url}` names no host")));
    }
    let (host, port) = match authority.rsplit_once(':') {
        // A bare IPv6 host would break this, and it is not a shape a model endpoint takes.
        Some((host, port)) => {
            let port = port.parse::<u16>().map_err(|_| {
                LlmError::transport(format!("`{url}` has a port that is not a number: `{port}`"))
            })?;
            (host.to_string(), port)
        }
        None => (authority.to_string(), scheme.default_port()),
    };
    Ok(Target {
        scheme,
        host,
        port,
        path: path.to_string(),
    })
}

impl Transport for PlainHttp {
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, LlmError> {
        self.send("POST", &request.url, &request.headers, Some(&request.body))
    }

    fn post_stream(
        &self,
        request: &HttpRequest,
        on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        self.stream(&request.url, &request.headers, &request.body, on_event)
    }
}

/// The TLS adapter: the same request, over a connection this machine's trust store vouched for.
///
/// The connector is held rather than rebuilt, and it is a `Result` because building one is the only
/// step here that can fail before a request exists: a build whose TLS backend will not initialise
/// says so on the first `https` call instead of refusing to start.
pub struct TlsHttp {
    connector: Result<TlsConnector, String>,
}

impl TlsHttp {
    /// The platform's own trust decision: whatever a browser on this machine accepts, this accepts.
    pub fn system() -> Self {
        Self {
            connector: TlsConnector::new().map_err(|error| error.to_string()),
        }
    }

    /// A connector the caller built. This is the seam the tests use, so that a test trusts a
    /// certificate it minted rather than whatever store the machine running it happens to have.
    pub fn with_connector(connector: TlsConnector) -> Self {
        Self {
            connector: Ok(connector),
        }
    }

    fn connector(&self, url: &str) -> Result<&TlsConnector, LlmError> {
        self.connector.as_ref().map_err(|why| {
            LlmError::transport(format!(
                "`{url}` is https, and this build has no usable TLS backend: {why}"
            ))
        })
    }

    /// The target of a call this adapter can carry, or the refusal naming the half that can — the
    /// mirror of [`PlainHttp::target`], and shared by both paths for the same reason.
    fn target(&self, url: &str, headers: &[(String, String)]) -> Result<Target, LlmError> {
        check_headers(headers)?;
        let target = parse(url)?;
        if target.scheme == Scheme::Http {
            return Err(LlmError::transport(format!(
                "`{url}` is http, and this adapter carries TLS only: use `HttpTransport`, which picks \
                 the half that matches the scheme"
            )));
        }
        Ok(target)
    }

    /// One exchange over a connection this machine's trust store vouched for, for either verb.
    fn send(
        &self,
        verb: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<HttpResponse, LlmError> {
        let target = self.target(url, headers)?;
        let mut tls = self.handshake(&target, url)?;
        let raw = exchange(&mut tls, verb, &target, headers, body)?;
        read_response(&raw)
    }

    /// The same handshake for the streaming path, which then reads the answer as it is written.
    fn stream(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
        on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        let target = self.target(url, headers)?;
        let mut tls = self.handshake(&target, url)?;
        stream_answer(&mut tls, "POST", &target, headers, Some(body), on_event)
    }

    /// The verified connection both paths send through.
    fn handshake(&self, target: &Target, url: &str) -> Result<TlsStream<TcpStream>, LlmError> {
        let connector = self.connector(url)?;
        // The socket timeouts are set before the handshake, so a peer that accepts the connection
        // and then says nothing is bounded by the same budget as a stalled read.
        let stream = connect(target, url)?;
        // The domain handed to the handshake is the host from the URL, which is what makes a
        // certificate for another name a refusal rather than a warning.
        connector
            .connect(&target.host, stream)
            .map_err(|error| handshake_failure(url, &target.host, error))
    }

    /// A `GET` over the same handshake; [`PlainHttp::get`] says why the verb is here rather than in
    /// `deepnest_llm::Transport`.
    pub fn get(&self, url: &str, headers: &[(String, String)]) -> Result<HttpResponse, LlmError> {
        self.send("GET", url, headers, None)
    }
}

impl Transport for TlsHttp {
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, LlmError> {
        self.send("POST", &request.url, &request.headers, Some(&request.body))
    }

    fn post_stream(
        &self,
        request: &HttpRequest,
        on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        self.stream(&request.url, &request.headers, &request.body, on_event)
    }
}

/// Both halves, chosen by the scheme in the URL. This is what the binary wires in.
pub struct HttpTransport {
    plain: PlainHttp,
    tls: TlsHttp,
}

impl HttpTransport {
    pub fn new() -> Self {
        Self {
            plain: PlainHttp::new(),
            tls: TlsHttp::system(),
        }
    }

    /// The same dispatch over a TLS half the caller built, which is how a test drives the binary's
    /// own adapter against a certificate it minted.
    pub fn with_tls(tls: TlsHttp) -> Self {
        Self {
            plain: PlainHttp::new(),
            tls,
        }
    }

    /// A `GET` through whichever half the URL's scheme names.
    ///
    /// Dispatched on the prefix for the same reason [`Transport::post`] is: the adapter that cannot
    /// carry this scheme must never be the one that reports the fault.
    pub fn get(&self, url: &str, headers: &[(String, String)]) -> Result<HttpResponse, LlmError> {
        if names_https(url) {
            self.tls.get(url, headers)
        } else {
            self.plain.get(url, headers)
        }
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for HttpTransport {
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, LlmError> {
        // Decided on the URL's prefix, before parsing: the adapter that cannot carry this scheme
        // must never be the one that reports the fault.
        if names_https(&request.url) {
            self.tls.post(request)
        } else {
            self.plain.post(request)
        }
    }

    fn post_stream(
        &self,
        request: &HttpRequest,
        on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        if names_https(&request.url) {
            self.tls.post_stream(request, on_event)
        } else {
            self.plain.post_stream(request, on_event)
        }
    }
}

fn names_https(url: &str) -> bool {
    url.get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

/// Refuse a header that would inject a second request, before any socket is opened — so that a test
/// of the refusal measures the refusal rather than the network.
fn check_headers(headers: &[(String, String)]) -> Result<(), LlmError> {
    for (name, value) in headers {
        // Nothing here produces a line break, and a transport that trusts its caller is how it
        // starts.
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            return Err(LlmError::transport(format!(
                "refused a header with a line break in it: `{name}`"
            )));
        }
    }
    Ok(())
}

/// What a platform says when the certificate it was shown is for a different host. macOS and
/// Windows word this themselves; OpenSSL reports it as a plain verification failure, which the
/// next list catches.
const NAMED_MISMATCH: [&str; 6] = [
    "mismatch",
    "does not match",
    "not valid for",
    "wrong host",
    "no alternative name",
    "host name does not match",
];

/// What a platform says when it will not vouch for the chain at all.
const CERTIFICATE_REFUSED: [&str; 8] = [
    "not trusted",
    "untrusted",
    "certificate verify failed",
    "local issuer",
    "unknown authority",
    "self-signed",
    "self signed",
    "certificate",
];

/// Turn a failed handshake into the sentence that names the fix.
///
/// Three outcomes, three messages: a certificate for another name, a certificate this machine will
/// not vouch for, and anything else the handshake hit. The detail the platform gave is carried in
/// every one of them — a message that hides it leaves the user nothing to search for.
///
/// **The platform's sentence already ends in a full stop**, and ours comes after it, so
/// [`without_trailing_full_stop`] takes theirs off before the two are joined. Without that the
/// message read `The certificate was not trusted.. If the endpoint is yours, …` — measured on macOS
/// through the binary's own model path, not spotted in review, which is why the tests now assert
/// the join and not only the wording.
fn handshake_failure(url: &str, host: &str, error: HandshakeError<TcpStream>) -> LlmError {
    let said = error.to_string();
    let lowered = said.to_lowercase();
    let detail = without_trailing_full_stop(&said);
    let message = if NAMED_MISMATCH.iter().any(|needle| lowered.contains(needle)) {
        format!(
            "`{url}`: the certificate `{host}` presented is for a different name — {detail}. Point \
             the endpoint at the name the certificate carries."
        )
    } else if CERTIFICATE_REFUSED
        .iter()
        .any(|needle| lowered.contains(needle))
    {
        format!(
            "`{url}`: the certificate `{host}` presented is not one this machine trusts — {detail}. \
             If the endpoint is yours, add its certificate authority to the system trust store; if \
             it is not, do not send the key there."
        )
    } else {
        // No trailing sentence of its own here: the platform's detail is the end of the message, so
        // it needs the full stop the branch above gets from its own following sentence.
        format!(
            "`{url}`: the TLS handshake with `{host}` failed before any request was sent — {detail}."
        )
    };
    LlmError::transport(message)
}

/// The platform's sentence, ready to be followed by one of ours.
///
/// A platform message is a finished sentence — `The certificate was not trusted.`, `A host name
/// mismatch has occurred.` — and every one of our branches writes punctuation after it. Strip the
/// platform's own trailing stops and whitespace, so the join produces `… trusted. If the endpoint`
/// rather than `… trusted.. If the endpoint`. `trim_end_matches` takes a run of them, because a
/// platform that ends a sentence with an ellipsis (`…`) is one more full stop away from the same
/// doubling.
fn without_trailing_full_stop(detail: &str) -> &str {
    detail.trim_end().trim_end_matches('.')
}

/// Write one request, head and whatever body it declares.
///
/// `verb` is the request's method, and it is a parameter because this adapter sends two of them.
/// `body` is what decides the `Content-Length` line, so a `GET` — which passes `None` — goes out
/// with no length declared at all. Shared by both paths, because a stream is the same request with
/// a different way of reading the answer.
fn send_request<C: Write>(
    stream: &mut C,
    verb: &str,
    target: &Target,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<(), LlmError> {
    let mut head = format!(
        "{verb} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        target.path,
        host_header(target),
    );
    if let Some(body) = body {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .and_then(|()| match body {
            Some(body) => stream.write_all(body.as_bytes()),
            None => Ok(()),
        })
        .and_then(|()| stream.flush())
        .map_err(|e| LlmError::transport(format!("the request could not be sent: {e}")))
}

/// Write one request and read everything the connection answers with.
fn exchange<C: Read + Write>(
    stream: &mut C,
    verb: &str,
    target: &Target,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<Vec<u8>, LlmError> {
    send_request(stream, verb, target, headers, body)?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| LlmError::transport(format!("the answer could not be read: {e}")))?;
    Ok(raw)
}

fn host_header(target: &Target) -> String {
    if target.port == target.scheme.default_port() {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    }
}

fn connect(target: &Target, url: &str) -> Result<TcpStream, LlmError> {
    let addrs = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .map_err(|e| LlmError::transport(format!("`{url}` could not be resolved: {e}")))?;
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(IO));
                let _ = stream.set_write_timeout(Some(IO));
                return Ok(stream);
            }
            Err(error) => last = Some(error),
        }
    }
    Err(LlmError::transport(format!(
        "`{url}` could not be reached: {}",
        last.map(|e| e.to_string())
            .unwrap_or_else(|| "no address to try".to_string())
    )))
}

/// How an answer's body is framed, which is what decides when there is nothing more to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// `Transfer-Encoding: chunked` — sizes interleaved with the data.
    Chunked,
    /// `Content-Length: n` — a bounded number of bytes.
    Length(usize),
    /// Neither, so the body runs until the connection closes.
    UntilClose,
}

/// What an answer's head says: the status, and how its body is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    status: u16,
    framing: Framing,
}

/// The status line and the framing headers of one answer head.
///
/// One reader for both paths: a stream and a whole answer are told the same things by the same
/// lines, and two parsers would eventually disagree about which of `Content-Length` and
/// `Transfer-Encoding` wins.
fn parse_head(head: &str) -> Result<Head, LlmError> {
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| LlmError::transport("the answer had no status line"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            LlmError::transport(format!("the status line made no sense: `{status_line}`"))
        })?;

    let mut length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }

    // Chunked wins, as HTTP/1.1 requires: a message with both declares a length it does not mean.
    let framing = if chunked {
        Framing::Chunked
    } else if let Some(length) = length {
        Framing::Length(length)
    } else {
        Framing::UntilClose
    };
    Ok(Head { status, framing })
}

/// Read the head of an answer without waiting for its body.
///
/// The bytes that arrived after the blank line are the beginning of the body — for a stream, that
/// is usually the first events — so they are handed back rather than dropped. This is where the
/// streaming path stops being able to peek: everything below reads a bounded amount, so nothing
/// here has to hold a whole answer.
fn read_head<C: Read>(stream: &mut C) -> Result<(Head, Vec<u8>), LlmError> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 1024];
    let split = loop {
        if let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break split;
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err(LlmError::transport("the answer ended before its head did")),
            Ok(read) => raw.extend_from_slice(&buffer[..read]),
            Err(error) => {
                return Err(LlmError::transport(format!(
                    "the answer's head could not be read: {error}"
                )))
            }
        }
    };
    let head = parse_head(&String::from_utf8_lossy(&raw[..split]))?;
    Ok((head, raw[split + 4..].to_vec()))
}

/// Read one whole answer out of the bytes the connection gave us.
fn read_response(raw: &[u8]) -> Result<HttpResponse, LlmError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| LlmError::transport("the answer had no head: no blank line ended it"))?;
    let head = parse_head(&String::from_utf8_lossy(&raw[..split]))?;
    let body = &raw[split + 4..];

    let text = match head.framing {
        Framing::Chunked => dechunk(body)?,
        Framing::Length(length) => {
            if body.len() < length {
                return Err(LlmError::transport(format!(
                    "the answer stopped after {} of {length} bytes — a truncated answer is not an \
                     answer",
                    body.len()
                )));
            }
            String::from_utf8_lossy(&body[..length]).to_string()
        }
        Framing::UntilClose => String::from_utf8_lossy(body).to_string(),
    };
    Ok(HttpResponse {
        status: head.status,
        body: text,
    })
}

/// Undo chunked transfer encoding.
fn dechunk(mut body: &[u8]) -> Result<String, LlmError> {
    let mut out = Vec::new();
    loop {
        let Some(end) = body.windows(2).position(|w| w == b"\r\n") else {
            return Err(LlmError::transport(
                "a chunked answer ended without its terminating chunk",
            ));
        };
        let header = String::from_utf8_lossy(&body[..end]).to_string();
        // A chunk's size line may carry `;extension` parameters after the size.
        let size_text = header.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| {
            LlmError::transport(format!("a chunk size was not hexadecimal: `{size_text}`"))
        })?;
        body = &body[end + 2..];
        if size == 0 {
            return Ok(String::from_utf8_lossy(&out).to_string());
        }
        if body.len() < size {
            return Err(LlmError::transport(
                "a chunked answer stopped inside a chunk",
            ));
        }
        out.extend_from_slice(&body[..size]);
        body = body.get(size + 2..).unwrap_or_default();
    }
}

/// Send one request and hand the answer's server-sent events over as they arrive.
///
/// This is the whole difference between an answer that appears and an answer that is watched being
/// written, and it is a property of *when* `on_event` is called rather than of the bytes: every
/// complete event reaches the caller before the next read is asked for, so a provider that pauses
/// mid-answer is a pause the caller sees rather than one it waits out.
///
/// Three things follow, and each is a decision rather than an accident:
///
/// - **A refusal is read whole.** A non-2xx answer is a document, and [`LlmError::classify`] reads
///   the document to decide what kind of failure it is — the same classification the whole-answer
///   path produces, which is what the trait's contract asks for.
/// - **The framing is decoded as it arrives.** Chunked bodies are de-chunked incrementally
///   ([`Dechunker`]), declared lengths are honoured, and a body with neither is read to the close.
/// - **Text is fed to the decoder only when it is complete.** A character can be split across two
///   reads ([`Utf8Tail`]) — for an answer in Chinese that is the ordinary case, not a corner one —
///   and replacing it with `U+FFFD` would corrupt the very text the user is watching.
fn stream_answer<C: Read + Write>(
    stream: &mut C,
    verb: &str,
    target: &Target,
    headers: &[(String, String)],
    body: Option<&str>,
    on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
) -> Result<(), LlmError> {
    send_request(stream, verb, target, headers, body)?;
    let (head, arrived) = read_head(stream)?;

    if !(200..300).contains(&head.status) {
        let mut raw = arrived;
        stream
            .read_to_end(&mut raw)
            .map_err(|e| LlmError::transport(format!("the refusal could not be read: {e}")))?;
        let text = match head.framing {
            Framing::Chunked => dechunk(&raw)?,
            Framing::Length(length) => {
                String::from_utf8_lossy(&raw[..length.min(raw.len())]).to_string()
            }
            Framing::UntilClose => String::from_utf8_lossy(&raw).to_string(),
        };
        return Err(LlmError::classify(Some(head.status), text));
    }

    let mut decoder = SseDecoder::new();
    let mut tail = Utf8Tail::new();
    let mut dechunker = Dechunker::new();
    // How many body bytes are still owed, for a body that declared a length. Every other framing
    // is bounded by the stream itself, so the budget is simply never reached.
    let mut owed = match head.framing {
        Framing::Length(length) => length,
        _ => usize::MAX,
    };

    let mut arrival = arrived;
    loop {
        let take = arrival.len().min(owed);
        owed -= take;
        let wire = &arrival[..take];
        let decoded = match head.framing {
            Framing::Chunked => dechunker.feed(wire)?,
            _ => wire.to_vec(),
        };

        if !decoded.is_empty() {
            let text = tail.feed(&decoded);
            if !text.is_empty() {
                for event in decoder.push(&text) {
                    // `[DONE]` is the end of the answer, not the end of the connection: a provider
                    // may hold the socket open afterwards, and waiting for it would turn a finished
                    // turn into a request that appears to hang.
                    let last = event.is_done();
                    on_event(event)?;
                    if last {
                        return Ok(());
                    }
                }
            }
        }

        if dechunker.is_done() || owed == 0 {
            break;
        }

        let mut buffer = [0u8; 8192];
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => arrival = buffer[..read].to_vec(),
            Err(error) => {
                return Err(LlmError::transport(format!(
                    "the stream stopped in the middle of the answer: {error}"
                )))
            }
        }
    }

    // A provider that closes without a blank line after its last event has still sent that event.
    for event in decoder.finish() {
        on_event(event)?;
    }
    Ok(())
}

/// Chunked transfer encoding, undone a piece at a time.
///
/// [`dechunk`] needs the whole body because it can look for every size line at once; a stream has
/// no such moment, so the state has to be held: whether a size line or chunk data comes next, and
/// how much of the current chunk is still owed. Feed it whatever arrived and it returns the body
/// bytes that are now complete.
#[derive(Debug, Default)]
struct Dechunker {
    /// Wire bytes that have arrived and have not been consumed yet.
    wire: Vec<u8>,
    /// `None` when a size line comes next; otherwise the bytes still owed by the current chunk.
    owed: Option<usize>,
    done: bool,
}

impl Dechunker {
    fn new() -> Self {
        Self::default()
    }

    fn is_done(&self) -> bool {
        self.done
    }

    fn feed(&mut self, wire: &[u8]) -> Result<Vec<u8>, LlmError> {
        if self.done {
            // Everything after the terminating chunk is a trailer nobody here reads.
            return Ok(Vec::new());
        }
        self.wire.extend_from_slice(wire);
        let mut body = Vec::new();

        loop {
            match self.owed {
                // A size line.
                None => {
                    let Some(end) = self.wire.windows(2).position(|w| w == b"\r\n") else {
                        break;
                    };
                    let line = String::from_utf8_lossy(&self.wire[..end]).to_string();
                    // A chunk's size line may carry `;extension` parameters after the size.
                    let size_text = line.split(';').next().unwrap_or("").trim().to_string();
                    let size = usize::from_str_radix(&size_text, 16).map_err(|_| {
                        LlmError::transport(format!(
                            "a chunk size was not hexadecimal: `{size_text}`"
                        ))
                    })?;
                    self.wire.drain(..end + 2);
                    if size == 0 {
                        self.done = true;
                        break;
                    }
                    self.owed = Some(size);
                }
                // The CRLF that closes the data of a chunk.
                Some(0) => {
                    match self.wire.first() {
                        // A server that ends its lines with a bare `\n` is not speaking HTTP/1.1,
                        // and refusing it here would be a stricter reader than the rest of this
                        // file; the byte is consumed and the data is what it said it was.
                        Some(b'\n') => self.wire.drain(..1),
                        Some(b'\r') if self.wire.len() == 1 => break,
                        Some(b'\r') if self.wire[1] == b'\n' => self.wire.drain(..2),
                        Some(_) => {
                            return Err(LlmError::transport(
                                "a chunk was not followed by the blank line that ends it",
                            ))
                        }
                        None => break,
                    };
                    self.owed = None;
                }
                // The data of a chunk, as much of it as has arrived.
                Some(owed) => {
                    if self.wire.is_empty() {
                        break;
                    }
                    let take = owed.min(self.wire.len());
                    body.extend_from_slice(&self.wire[..take]);
                    self.wire.drain(..take);
                    self.owed = Some(owed - take);
                }
            }
        }
        Ok(body)
    }
}

/// Bytes that may end in the middle of a character, held until the rest of it arrives.
///
/// [`SseDecoder`] takes text and every byte here arrives in whatever pieces the network chose, so
/// the two have to be joined without the join being visible. Anything that is not merely incomplete
/// — a byte that can never begin a character — is replaced and stepped over, because one bad byte
/// must not stall a stream that is otherwise fine.
#[derive(Debug, Default)]
struct Utf8Tail {
    held: Vec<u8>,
}

impl Utf8Tail {
    fn new() -> Self {
        Self::default()
    }

    fn feed(&mut self, bytes: &[u8]) -> String {
        self.held.extend_from_slice(bytes);
        let (text, used) = match std::str::from_utf8(&self.held) {
            Ok(text) => (text.to_string(), self.held.len()),
            Err(error) => {
                let valid = error.valid_up_to();
                let mut text = String::from_utf8_lossy(&self.held[..valid]).to_string();
                match error.error_len() {
                    // A sequence that can never complete: replace it and carry on.
                    Some(bad) => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        (text, valid + bad)
                    }
                    // A sequence that has simply not arrived yet: hold it.
                    None => (text, valid),
                }
            }
        };
        self.held.drain(..used);
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Instant;

    /// A one-shot HTTP server: it answers `response` to whatever arrives, and hands back the
    /// request it read so a test can assert on the bytes sent.
    fn serve(response: &'static [u8]) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut seen = Vec::new();
                let mut buf = [0u8; 1024];
                // A head and a body are two writes, so one `read` can return the head alone and
                // make a body assertion measure the scheduler. Read until a blank line ends the
                // head *and* everything its `Content-Length` promised is here.
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            seen.extend_from_slice(&buf[..read]);
                            if complete(&seen) {
                                break;
                            }
                        }
                    }
                }
                let _ = stream.write_all(response);
                let _ = stream.flush();
                let _ = tx.send(String::from_utf8_lossy(&seen).to_string());
            }
        });
        (format!("http://127.0.0.1:{port}/v1/chat/completions"), rx)
    }

    /// Whether what has arrived is the whole request, head and declared body.
    fn complete(seen: &[u8]) -> bool {
        let Some(split) = seen.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let head = String::from_utf8_lossy(&seen[..split]).to_lowercase();
        let promised = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        seen.len() >= split + 4 + promised
    }

    fn post_to(url: &str) -> Result<HttpResponse, LlmError> {
        PlainHttp::new().post(&HttpRequest {
            url: url.to_string(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: r#"{"model":"m"}"#.to_string(),
        })
    }

    fn get_from(url: &str) -> Result<HttpResponse, LlmError> {
        PlainHttp::new().get(
            url,
            &[("Accept".into(), "application/vnd.github+json".into())],
        )
    }

    #[test]
    fn a_whole_answer_comes_back_with_its_status_and_body() {
        let (url, seen) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
        );
        let answer = post_to(&url).expect("a plaintext answer");
        assert_eq!(answer.status, 200);
        assert_eq!(answer.body, r#"{"ok":true}"#);

        let request = seen.recv().expect("the server saw a request");
        assert!(
            request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"),
            "{request}"
        );
        // One connection per request: a pooled socket's staleness looks like the endpoint's fault.
        assert!(request.contains("Connection: close"), "{request}");
        assert!(request.contains(r#"Content-Length: 13"#), "{request}");
        assert!(request.contains(r#"{"model":"m"}"#), "{request}");
    }

    #[test]
    fn the_plaintext_adapter_refuses_an_https_endpoint_by_name() {
        // Not a fallback and not a hang: plaintext aimed at a TLS port either says nothing or says
        // the key in the clear, and the message names the adapter that does carry this scheme.
        let error = post_to("https://api.example.com/v1/chat/completions").unwrap_err();
        assert!(
            error.body.contains("carries plaintext only"),
            "{}",
            error.body
        );
        assert!(error.body.contains("HttpTransport"), "{}", error.body);
        assert!(
            error.body.contains("https://api.example.com"),
            "{}",
            error.body
        );
    }

    #[test]
    fn the_tls_adapter_refuses_an_http_endpoint_by_name() {
        let error = TlsHttp::with_connector(native_tls::TlsConnector::builder().build().unwrap())
            .post(&HttpRequest {
                url: "http://127.0.0.1:1/v1".to_string(),
                headers: Vec::new(),
                body: String::new(),
            })
            .unwrap_err();
        assert!(error.body.contains("carries TLS only"), "{}", error.body);
    }

    #[test]
    fn an_unknown_scheme_names_both_that_are_supported() {
        let error = post_to("ftp://127.0.0.1/v1").unwrap_err();
        assert!(error.body.contains("only http and https"), "{}", error.body);
    }

    #[test]
    fn a_url_with_no_scheme_says_what_a_url_looks_like() {
        let error = post_to("127.0.0.1:8080/v1").unwrap_err();
        assert!(error.body.contains("has no scheme"), "{}", error.body);
    }

    #[test]
    fn a_body_shorter_than_its_content_length_is_an_error_not_a_short_answer() {
        // The whole point: half a completion parsed as a whole one is a tool call run on a
        // fragment, and nothing downstream can tell it was cut.
        let (url, _seen) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\n{\"choices\":");
        let error = post_to(&url).unwrap_err();
        assert!(error.body.contains("truncated"), "{}", error.body);
    }

    #[test]
    fn a_chunked_answer_is_reassembled() {
        let (url, _seen) = serve(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"ok\r\n4\r\n\":1}\r\n0\r\n\r\n",
        );
        let answer = post_to(&url).expect("a chunked answer");
        assert_eq!(answer.body, r#"{"ok":1}"#);
    }

    #[test]
    fn a_chunked_answer_that_stops_mid_chunk_is_an_error() {
        let (url, _seen) =
            serve(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\n{\"ok");
        let error = post_to(&url).unwrap_err();
        assert!(error.body.contains("inside a chunk"), "{}", error.body);
    }

    #[test]
    fn a_redirect_is_returned_rather_than_followed() {
        // Following it would spend the user's key at a host they never configured.
        let (url, _seen) = serve(
            b"HTTP/1.1 302 Found\r\nLocation: http://elsewhere/\r\nContent-Length: 0\r\n\r\n",
        );
        let answer = post_to(&url).expect("a 3xx is a value");
        assert_eq!(answer.status, 302);
    }

    #[test]
    fn a_header_with_a_line_break_is_refused_before_it_is_sent() {
        let error = PlainHttp::new()
            .post(&HttpRequest {
                url: "http://127.0.0.1:1/v1".to_string(),
                headers: vec![("X-Thing".into(), "a\r\nX-Injected: b".into())],
                body: String::new(),
            })
            .unwrap_err();
        assert!(error.body.contains("line break"), "{}", error.body);
    }

    /// A one-shot server that writes its answer in pieces, waiting after each — which is how a
    /// test tells delivery-as-it-arrives apart from delivery-at-the-end, and how it produces the
    /// two separate reads a split character needs.
    fn serve_in_pieces(pieces: Vec<(Vec<u8>, Duration)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut seen = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            seen.extend_from_slice(&buf[..read]);
                            if complete(&seen) {
                                break;
                            }
                        }
                    }
                }
                for (piece, pause) in pieces {
                    let _ = stream.write_all(&piece);
                    let _ = stream.flush();
                    std::thread::sleep(pause);
                }
            }
        });
        format!("http://127.0.0.1:{port}/v1/chat/completions")
    }

    /// One chunked-encoded frame carrying `body`.
    fn chunk(body: &str) -> Vec<u8> {
        format!("{:x}\r\n{body}\r\n", body.len()).into_bytes()
    }

    /// One event as a provider writes it.
    fn event(data: &str) -> String {
        format!("data: {data}\n\n")
    }

    /// The head of a streamed answer, as an OpenAI-compatible endpoint sends it.
    fn stream_head() -> Vec<u8> {
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec()
    }

    /// Post to `url` on the streaming path, collecting what arrived and when.
    fn stream_to(url: &str) -> (Result<(), LlmError>, Vec<SseEvent>, Vec<Instant>) {
        let mut events = Vec::new();
        let mut at = Vec::new();
        let outcome = PlainHttp::new().post_stream(
            &HttpRequest {
                url: url.to_string(),
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: r#"{"model":"m","stream":true}"#.to_string(),
            },
            &mut |event: SseEvent| {
                at.push(Instant::now());
                events.push(event);
                Ok(())
            },
        );
        (outcome, events, at)
    }

    /// The one property this whole path exists for: an event reaches the caller while the provider
    /// is still producing the answer.
    ///
    /// An adapter that read the body whole and then replayed the events would pass every other
    /// assertion here and fail this one, which is why the assertion is on *when* rather than on
    /// *what*.
    #[test]
    fn a_stream_arrives_while_the_answer_is_still_being_written() {
        let url = serve_in_pieces(vec![
            (stream_head(), Duration::ZERO),
            (
                chunk(&event(r#"{"choices":[{"delta":{"content":"he"}}]}"#)),
                Duration::ZERO,
            ),
            // The provider thinks between the two halves of one answer.
            (
                chunk(&event(r#"{"choices":[{"delta":{"content":"llo"}}]}"#)),
                Duration::from_millis(600),
            ),
            (b"0\r\n\r\n".to_vec(), Duration::ZERO),
        ]);

        let started = Instant::now();
        let (outcome, events, at) = stream_to(&url);
        let ended = started.elapsed();
        outcome.expect("a streamed answer");

        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events[0].data.contains("\"he\""), "{events:?}");
        assert!(events[1].data.contains("\"llo\""), "{events:?}");
        let first = at[0] - started;
        assert!(
            first + Duration::from_millis(300) < ended,
            "the first event was delivered at {first:?} and the answer ended at {ended:?} — that \
             is delivery at the end, not as it arrived"
        );
    }

    #[test]
    fn a_stream_ends_at_its_declared_length_without_waiting_for_the_close() {
        let body = event(r#"{"choices":[{"delta":{"content":"done"}}]}"#);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        // The server keeps the socket open long after it has said everything, which a provider
        // that holds the connection for its next request does. Waiting for the close would make
        // every turn last as long as that hold.
        let url = serve_in_pieces(vec![
            (format!("{head}{body}").into_bytes(), Duration::from_secs(3)),
            (Vec::new(), Duration::ZERO),
        ]);

        let started = Instant::now();
        let (outcome, events, _) = stream_to(&url);
        outcome.expect("a streamed answer");
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the answer waited for the connection to close: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_stream_that_closes_without_a_blank_line_still_delivers_its_last_event() {
        let body = format!("data: {}", r#"{"choices":[{"delta":{"content":"tail"}}]}"#);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let url = serve_in_pieces(vec![(format!("{head}{body}").into_bytes(), Duration::ZERO)]);

        let (outcome, events, _) = stream_to(&url);
        outcome.expect("a streamed answer");
        assert_eq!(events.len(), 1, "the last event was dropped: {events:?}");
        assert!(events[0].data.contains("\"tail\""), "{events:?}");
    }

    /// A character can be split across two reads, and for an answer in Chinese that is ordinary.
    #[test]
    fn a_character_split_across_two_reads_arrives_whole() {
        let sentence = "这是一个中文回答";
        let body = event(&format!(
            r#"{{"choices":[{{"delta":{{"content":"{sentence}"}}}}]}}"#
        ));
        // Split inside the third character: 这 is three bytes, so two of them go in the first write
        // and the third arrives with the rest. Sliced as bytes, because a `&str` cannot be cut
        // where this test needs the cut.
        let cut = body.find('这').unwrap() + 2;
        let bytes = body.as_bytes();
        let (first, second) = bytes.split_at(cut);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut opening = head.into_bytes();
        opening.extend_from_slice(first);
        let url = serve_in_pieces(vec![
            (opening, Duration::from_millis(50)),
            (second.to_vec(), Duration::ZERO),
        ]);

        let (outcome, events, _) = stream_to(&url);
        outcome.expect("a streamed answer");
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            events[0].data.contains(sentence),
            "a character split across two reads was mangled: {}",
            events[0].data
        );
        assert!(
            !events[0].data.contains(char::REPLACEMENT_CHARACTER),
            "a half-arrived character was replaced instead of waited for: {}",
            events[0].data
        );
    }

    #[test]
    fn a_refusal_on_the_streaming_path_is_classified_from_its_body() {
        let body = r#"{"error":{"message":"invalid api key"}}"#;
        let head = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let url = serve_in_pieces(vec![(format!("{head}{body}").into_bytes(), Duration::ZERO)]);

        let (outcome, events, _) = stream_to(&url);
        let error = outcome.expect_err("a refusal is not an answer");
        assert!(events.is_empty(), "a refusal delivered events: {events:?}");
        assert_eq!(error.status, Some(401));
        assert!(error.body.contains("invalid api key"), "{}", error.body);
        // The same class the whole-answer path gives the same status: the trait's contract is that
        // a refusal is classified identically whichever path asked for it.
        assert_eq!(error, LlmError::classify(Some(401), body));
    }

    #[test]
    fn a_plaintext_stream_refuses_an_https_endpoint_by_name() {
        let mut sink = |_event: SseEvent| Ok(());
        let error = PlainHttp::new()
            .post_stream(
                &HttpRequest {
                    url: "https://api.example/v1/chat/completions".to_string(),
                    headers: Vec::new(),
                    body: String::new(),
                },
                &mut sink,
            )
            .unwrap_err();
        assert!(
            error.body.contains("carries plaintext only"),
            "{}",
            error.body
        );
    }

    #[test]
    fn the_dispatching_transport_streams_through_the_plaintext_half() {
        let url = serve_in_pieces(vec![
            (stream_head(), Duration::ZERO),
            (
                chunk(&event(r#"{"choices":[{"delta":{"content":"hi"}}]}"#)),
                Duration::ZERO,
            ),
            (b"0\r\n\r\n".to_vec(), Duration::ZERO),
        ]);

        let mut events = Vec::new();
        HttpTransport::new()
            .post_stream(
                &HttpRequest {
                    url,
                    headers: Vec::new(),
                    body: r#"{"model":"m","stream":true}"#.to_string(),
                },
                &mut |event: SseEvent| {
                    events.push(event);
                    Ok(())
                },
            )
            .expect("a streamed answer through the dispatching adapter");
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].data.contains("\"hi\""), "{events:?}");
    }

    #[test]
    fn a_port_that_is_not_a_number_is_refused() {
        let error = post_to("http://127.0.0.1:not-a-port/v1").unwrap_err();
        assert!(error.body.contains("not a number"), "{}", error.body);
    }

    #[test]
    fn a_get_asks_without_a_body_and_reads_the_answer() {
        let (url, seen) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 9\r\n\r\n{\"tag\":1}",
        );
        let answer = get_from(&url).expect("a plaintext answer");
        assert_eq!(answer.status, 200);
        assert_eq!(answer.body, r#"{"tag":1}"#);

        let request = seen.recv().expect("the server saw a request");
        assert!(
            request.starts_with("GET /v1/chat/completions HTTP/1.1\r\n"),
            "{request}"
        );
        // Absent, not `Content-Length: 0`: a request that declares a body is one whose next bytes a
        // server waits for, and this one has none to send.
        assert!(
            !request.to_lowercase().contains("content-length"),
            "{request}"
        );
        assert!(
            request.contains("Accept: application/vnd.github+json"),
            "{request}"
        );
        // Head, one blank line, and then nothing at all.
        assert!(request.ends_with("\r\n\r\n"), "{request}");
    }

    #[test]
    fn a_plaintext_get_refuses_an_https_endpoint_by_name() {
        // The release check's own endpoint, which is `https`: the refusal has to name the adapter
        // that carries it rather than say the request failed.
        let error = PlainHttp::new()
            .get(
                "https://api.github.com/repos/owner/name/releases/latest",
                &[],
            )
            .unwrap_err();
        assert!(
            error.body.contains("carries plaintext only"),
            "{}",
            error.body
        );
        assert!(error.body.contains("HttpTransport"), "{}", error.body);
    }

    #[test]
    fn a_tls_get_refuses_an_http_endpoint_by_name() {
        let error = TlsHttp::with_connector(native_tls::TlsConnector::builder().build().unwrap())
            .get("http://127.0.0.1:1/v1", &[])
            .unwrap_err();
        assert!(error.body.contains("carries TLS only"), "{}", error.body);
    }

    #[test]
    fn a_header_line_break_is_refused_on_a_get_too() {
        // The verb changed; the rule that a caller's header cannot inject a second request did not.
        let error = PlainHttp::new()
            .get(
                "http://127.0.0.1:1/v1",
                &[("X-Thing".into(), "a\r\nX-Injected: b".into())],
            )
            .unwrap_err();
        assert!(error.body.contains("line break"), "{}", error.body);
    }

    #[test]
    fn the_dispatching_transport_sends_a_plain_get_to_the_plaintext_half() {
        // `HttpTransport` is what the binary wires in. Dispatch is on the URL's prefix, so an `http`
        // GET must not reach the TLS half — that half would refuse it as "TLS only".
        let (url, seen) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{\"tag\":1}");
        let answer = HttpTransport::new()
            .get(&url, &[])
            .expect("an http answer through the dispatch");
        assert_eq!(answer.body, r#"{"tag":1}"#);
        assert!(seen
            .recv()
            .expect("the server saw a request")
            .starts_with("GET "));
    }

    #[test]
    fn the_platforms_full_stop_is_taken_off_before_ours_is_added() {
        // The two sentences the platforms actually produce, measured: joining them raw gave
        // `… was not trusted.. If the endpoint …`.
        assert_eq!(
            without_trailing_full_stop("The certificate was not trusted."),
            "The certificate was not trusted"
        );
        assert_eq!(
            without_trailing_full_stop("A host name mismatch has occurred."),
            "A host name mismatch has occurred"
        );
    }

    #[test]
    fn a_sentence_with_no_full_stop_is_left_alone() {
        // OpenSSL's own wording arrives without one on some paths; adding a second there would be
        // the same defect in the other direction.
        assert_eq!(
            without_trailing_full_stop("certificate verify failed"),
            "certificate verify failed"
        );
    }

    #[test]
    fn a_run_of_stops_and_trailing_space_comes_off() {
        assert_eq!(without_trailing_full_stop("something.."), "something");
        assert_eq!(without_trailing_full_stop("something. \n"), "something");
        assert_eq!(without_trailing_full_stop(""), "");
    }
}
