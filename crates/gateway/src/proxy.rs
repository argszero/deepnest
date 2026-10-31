//! Serving an app under `/app/<slug>/`: the forward itself.
//!
//! The app's server listens on a loopback port of its own, allocated per start and never
//! persisted. This module turns a request that arrived on deepnest's port into one for that app,
//! and the app's answer back into one the browser can use under the prefix.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::http::{Method, Request, Response};
use crate::rewrite::{self, is_hop_by_hop};

/// How long to wait for the app to accept a connection.
///
/// It is on this machine and it is already running — `start` does not report success until it
/// answers — so a connection that takes seconds is a connection that is not coming.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the app gets to send its answer, and to take it.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

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

/// How a request reaches an app.
///
/// A trait for the reason every other seam here is one: the behaviour worth testing in the router is
/// *which app a request is sent to and what is answered when there is no app to send it to*, and a
/// test that needed a running app on the exact port the supervisor allocated would be measuring the
/// kernel's port allocator.
pub trait Forwarder: Send + Sync {
    fn forward(&self, target: &Target, request: &Request) -> Result<Response, ForwardError>;
}

/// The real forward: the one in this module.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdForwarder;

impl Forwarder for StdForwarder {
    fn forward(&self, target: &Target, request: &Request) -> Result<Response, ForwardError> {
        forward(target, request)
    }
}

/// Forward one request to the app and return its answer, adapted to the prefix.
pub fn forward(target: &Target, request: &Request) -> Result<Response, ForwardError> {
    let head = upstream_request(target, request);
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
    let raw = write_and_read(&mut stream, head.as_bytes()).map_err(ForwardError::Io)?;
    let answer = parse_response(&raw)?;
    Ok(adapt(target, answer))
}

/// The request the app receives.
///
/// The app's own path, its own headers, and `Connection: close` — this forward reads one answer and
/// then stops, so asking the app to hold the connection open would only leave a socket waiting for
/// a second request that never comes.
fn upstream_request(target: &Target, request: &Request) -> String {
    let path = rewrite::upstream_path(&target.prefix(), &request.path);
    let mut head = String::with_capacity(256);
    head.push_str(request.method.as_str());
    head.push(' ');
    head.push_str(&path);
    if !request.query.is_empty() {
        head.push('?');
        head.push_str(&request.query);
    }
    head.push_str(" HTTP/1.1\r\n");
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

/// The whole exchange against an address: connect, send, read. What a test points at a real app.
pub fn exchange(addr: SocketAddr, head: &[u8]) -> io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    write_and_read(&mut stream, head)
}

/// Send `head` on an open connection, read everything the app says back.
fn write_and_read(stream: &mut TcpStream, head: &[u8]) -> io::Result<Vec<u8>> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(head)?;
    // Half-close: the app is told the request is complete, and can still answer.
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => raw.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(raw)
}

/// An HTTP response as the app sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn parse_response(raw: &[u8]) -> Result<Answer, ForwardError> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(ForwardError::Malformed(
            "the answer ended before its headers did",
        ))?;
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| ForwardError::Malformed("the answer's head is not text"))?;
    let body = &raw[split + 4..];

    let mut lines = head.split("\r\n");
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

    let mut answer = Answer {
        status,
        headers,
        body: body.to_vec(),
    };

    // A body with no length is delimited by the end of the connection, and that is what arrived.
    // A chunked body is not: its framing is part of the bytes, so it has to be decoded before
    // anything downstream can treat it as a body.
    if answer
        .header("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        answer.body = decode_chunked(&answer.body)?;
    } else if let Some(length) = answer.header("content-length").and_then(|v| v.parse().ok()) {
        // Trust the announced length over the connection: an app that closes early has still told
        // us how much it meant to send, and a truncated body is a broken answer either way.
        answer.body.truncate(length);
    }
    Ok(answer)
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
fn adapt(target: &Target, answer: Answer) -> Response {
    let prefix = target.prefix();
    let content_type = answer
        .header("content-type")
        .unwrap_or("application/octet-stream")
        .to_string();

    let mut body = answer.body;
    if content_type.to_ascii_lowercase().starts_with("text/html") {
        if let Ok(page) = std::str::from_utf8(&body) {
            if let Some(rewritten) = rewrite::inject_base(&prefix, page) {
                body = rewritten.into_bytes();
            }
        }
    }

    let mut response = Response::new(answer.status, content_type, body);
    for (name, value) in answer.headers {
        if is_hop_by_hop(&name) || name.eq_ignore_ascii_case("content-type") {
            continue;
        }
        if name.eq_ignore_ascii_case("location") {
            response = response.header(name, rewrite::location(&prefix, &value));
        } else if name.eq_ignore_ascii_case("set-cookie") {
            response = response.header(name, rewrite::set_cookie(&prefix, &value));
        } else {
            response = response.header(name, value);
        }
    }
    response
}

/// Only `HEAD` and `GET` reach an app through this forward today.
///
/// The hand-written layer reads one request per connection and no body, so a `POST` cannot be
/// forwarded faithfully: its content would be dropped on the floor and the app would be told a
/// request arrived that said nothing. Until the connection model changes, an app's state changes
/// are its own to serve, not this door's to mangle.
pub fn can_forward(method: Method) -> bool {
    matches!(method, Method::Get | Method::Head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn get(path: &str) -> Request {
        Request {
            method: Method::Get,
            path: path.to_string(),
            query: String::new(),
            headers: vec![("host".to_string(), "127.0.0.1:4270".to_string())],
        }
    }

    /// An app that answers every request with the same bytes, and one request read.
    fn serving(answer: &'static [u8]) -> (SocketAddr, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
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
            let _ = stream.write_all(answer);
            let _ = stream.flush();
            seen
        });
        (addr, handle)
    }

    fn forward_to(addr: SocketAddr, request: &Request) -> Result<Response, ForwardError> {
        let target = Target::new("notes", addr.port());
        let head = upstream_request(&target, request);
        let raw = exchange(addr, head.as_bytes()).map_err(|source| ForwardError::Connect {
            port: addr.port(),
            source,
        })?;
        let answer = parse_response(&raw)?;
        Ok(adapt(&target, answer))
    }

    #[test]
    fn the_target_knows_the_prefix_it_is_served_under() {
        assert_eq!(Target::new("notes", 4271).prefix(), "/app/notes");
    }

    #[test]
    fn the_app_receives_its_own_path_and_its_own_host() {
        let (addr, handle) = serving(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi");
        let response = forward_to(addr, &get("/app/notes/api/items")).unwrap();
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
        let response = forward_to(addr, &get("/app/notes/")).unwrap();
        let body = String::from_utf8(response.body.clone()).unwrap();
        assert!(body.contains("<base href=\"/app/notes/\">"), "{body}");
        assert_eq!(response.content_type, "text/html; charset=utf-8");
    }

    #[test]
    fn a_redirect_comes_back_pointing_inside_the_app() {
        let answer = b"HTTP/1.1 302 Found\r\nlocation: /login\r\ncontent-length: 0\r\n\r\n";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/private")).unwrap();
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
        let response = forward_to(addr, &get("/app/notes/")).unwrap();
        assert_eq!(response.body, b"hello world");
    }

    #[test]
    fn a_non_html_answer_is_handed_over_byte_for_byte() {
        let answer = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 12\r\n\r\n{\"items\":[]}";
        let (addr, _handle) = serving(answer);
        let response = forward_to(addr, &get("/app/notes/api/items")).unwrap();
        assert_eq!(response.body, b"{\"items\":[]}");
        assert_eq!(response.content_type, "application/json");
    }

    #[test]
    fn an_answer_that_is_not_http_is_named_rather_than_forwarded() {
        let (addr, _handle) = serving(b"this is not a response");
        match forward_to(addr, &get("/app/notes/")) {
            Err(ForwardError::Malformed(_)) => {}
            other => panic!("{other:?}"),
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
            Ok(response) => panic!("expected a connect failure, got {}", response.status),
        }
    }

    #[test]
    fn only_the_two_methods_this_layer_can_carry_are_forwarded() {
        assert!(can_forward(Method::Get));
        assert!(can_forward(Method::Head));
        assert!(!can_forward(Method::Post));
        assert!(!can_forward(Method::Delete));
    }
}
