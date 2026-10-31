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

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use deepnest_llm::{HttpRequest, HttpResponse, LlmError, SseEvent, Transport};
use native_tls::{HandshakeError, TlsConnector};

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

    /// One exchange over a plaintext connection, for whichever verb the caller named.
    ///
    /// The scheme is refused here rather than at each verb, because the two verbs have the same
    /// reason: sending these bytes in the clear at a TLS port is either a hang the user would blame
    /// on their key or a key sent in the clear. Neither is a fallback.
    fn send(
        &self,
        verb: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<HttpResponse, LlmError> {
        check_headers(headers)?;
        let target = parse(url)?;
        if target.scheme == Scheme::Https {
            return Err(LlmError::transport(format!(
                "`{url}` is https, and this adapter carries plaintext only: use `HttpTransport`, which \
                 picks the half that matches the scheme"
            )));
        }
        let mut stream = connect(&target, url)?;
        let raw = exchange(&mut stream, verb, &target, headers, body)?;
        read_response(&raw)
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
        _request: &HttpRequest,
        _on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        // Stated rather than left as a `todo!()`: the loop above this seam asks for a whole answer
        // (`Client::turn`), and an adapter that pretended to stream would deliver one event at the
        // end, which is a worse lie than a refusal.
        Err(no_streaming_path())
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

    /// One exchange over a connection this machine's trust store vouched for, for either verb.
    fn send(
        &self,
        verb: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<HttpResponse, LlmError> {
        check_headers(headers)?;
        let target = parse(url)?;
        if target.scheme == Scheme::Http {
            return Err(LlmError::transport(format!(
                "`{url}` is http, and this adapter carries TLS only: use `HttpTransport`, which picks \
                 the half that matches the scheme"
            )));
        }
        let connector = self.connector(url)?;
        // The socket timeouts are set before the handshake, so a peer that accepts the connection
        // and then says nothing is bounded by the same budget as a stalled read.
        let stream = connect(&target, url)?;
        // The domain handed to the handshake is the host from the URL, which is what makes a
        // certificate for another name a refusal rather than a warning.
        let mut tls = connector
            .connect(&target.host, stream)
            .map_err(|error| handshake_failure(url, &target.host, error))?;
        let raw = exchange(&mut tls, verb, &target, headers, body)?;
        read_response(&raw)
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
        _request: &HttpRequest,
        _on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        Err(no_streaming_path())
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

fn no_streaming_path() -> LlmError {
    LlmError::transport(
        "this build asks for a whole answer, not a stream — the model adapter has no streaming path",
    )
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

/// Write one request and read everything the connection answers with.
///
/// `verb` is the request's method, and it is a parameter because this adapter sends two of them.
/// `body` is what decides the `Content-Length` line, so a `GET` — which passes `None` — goes out
/// with no length declared at all.
fn exchange<C: Read + Write>(
    stream: &mut C,
    verb: &str,
    target: &Target,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<Vec<u8>, LlmError> {
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
        .map_err(|e| LlmError::transport(format!("the request could not be sent: {e}")))?;

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

/// Read one whole answer out of the bytes the connection gave us.
fn read_response(raw: &[u8]) -> Result<HttpResponse, LlmError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| LlmError::transport("the answer had no head: no blank line ended it"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = &raw[split + 4..];

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

    let text = if chunked {
        dechunk(body)?
    } else if let Some(length) = length {
        if body.len() < length {
            return Err(LlmError::transport(format!(
                "the answer stopped after {} of {length} bytes — a truncated answer is not an \
                 answer",
                body.len()
            )));
        }
        String::from_utf8_lossy(&body[..length]).to_string()
    } else {
        String::from_utf8_lossy(body).to_string()
    };
    Ok(HttpResponse { status, body: text })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

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

    #[test]
    fn streaming_says_it_does_not_stream() {
        let mut sink = |_event: SseEvent| Ok(());
        let error = PlainHttp::new()
            .post_stream(
                &HttpRequest {
                    url: "http://127.0.0.1:1/v1".to_string(),
                    headers: Vec::new(),
                    body: String::new(),
                },
                &mut sink,
            )
            .unwrap_err();
        assert!(error.body.contains("no streaming path"), "{}", error.body);
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
