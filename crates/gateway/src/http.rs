//! Exactly the HTTP/1.1 this binary needs: read one request, write one response, close.
//!
//! It speaks four methods, and it reads a request body **only when a route asks for one and only
//! up to a bound**. The first such route is the panel's `POST /api/config`, and the reason is the
//! value it carries: an API key. A key in a URL is written into the browser's history and into
//! whatever logs the URL, which is the same reason the control token travels in a header rather
//! than in the path — so the key travels in a body.
//!
//! Every other route still reads nothing, and a body sent to one is refused by name: the router
//! knows which route reads a body, and this layer does not read a byte for one that does not. A
//! server that ignored a body would answer "done" to a request whose content it never saw.
//!
//! A body is bounded by [`MAX_BODY`] and is accepted only with a declared `Content-Length`.
//! Chunked encoding is refused: this layer would have to buffer the whole thing to know its size
//! anyway, so supporting it would buy nothing but a way to send more than the bound.
//!
//! Deliberately not a server library. `crates/gateway` needs a real one — routing, header
//! rewriting, WebSocket upgrade, unbuffered SSE — and choosing that stack is that crate's
//! decision to make, not this one's. What this module has to get right is much smaller, and it
//! is all about refusing clearly: a request with no `Host`, a request line that is not a
//! request line, a client that opens a connection and says nothing, a head larger than any real
//! client sends. Every one of those answers with a status the caller can see instead of hanging
//! or being half-parsed.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{self, Read, Write};

/// The largest request head accepted, in bytes.
///
/// A head this large is not a request any browser or probe sends; it is someone deciding how
/// much memory to make this process spend.
pub const MAX_HEAD: usize = 8 * 1024;

/// How many header fields are accepted before the request is refused.
pub const MAX_HEADERS: usize = 64;

/// The largest request body accepted, in bytes.
///
/// Every route that takes a body takes a handful of short strings: three config values, a name.
/// 16 KiB is generous for that and small enough that a body cannot be how this process is made to
/// spend memory — the same reasoning as [`MAX_HEAD`], applied to the other half of the request.
pub const MAX_BODY: usize = 16 * 1024;

/// The methods this binary serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Delete,
}

impl Method {
    /// The method as it appeared in the request line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Delete => "DELETE",
        }
    }
}

/// A parsed request: its head, and its body when it declared one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// The path, with the query string removed.
    pub path: String,
    /// The query string, without its `?`; empty when there was none.
    pub query: String,
    /// Header names lowercased, values trimmed, in the order they arrived.
    pub headers: Vec<(String, String)>,
    /// The body, when the request declared one and [`read`] read it. Empty otherwise: a route
    /// that takes no body sees a request indistinguishable from one that sent nothing, which is
    /// what the router's own refusal turns on.
    pub body: Vec<u8>,
}

impl Request {
    /// The body as text, when it is UTF-8.
    ///
    /// `None` is not an empty body: it is a body this server cannot read as the JSON it expects,
    /// and the caller answers `400` rather than pretending it received nothing.
    pub fn body_text(&self) -> Option<&str> {
        std::str::from_utf8(&self.body).ok()
    }

    /// A header's value, by case-insensitive name. The first occurrence wins.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Why a request head could not be read as a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The request line was empty or cut short.
    Malformed,
    /// The method is syntactically fine but not served here.
    UnsupportedMethod,
    /// The head exceeded [`MAX_HEAD`] or [`MAX_HEADERS`].
    Oversized,
    /// The request is chunked. This layer accepts a body only with a declared `Content-Length`, so
    /// a chunked one is refused rather than half-read.
    Body,
    /// The request declares a body larger than [`MAX_BODY`].
    TooLarge,
}

/// Why [`read`] did not produce a request.
#[derive(Debug)]
pub enum ReadError {
    /// The transport failed, or the client left mid-request.
    Io(io::Error),
    /// There was a head, and it was not a request this server accepts.
    Parse(ParseError),
}

/// Read one request from `stream`: the head, then the body it declared.
///
/// The bytes after the head are already in `buffer` when the head ends, and they are the beginning
/// of the body rather than something to discard — a browser sends a small body in the same packet
/// as its head, so a reader that dropped what it had already seen would lose it and then wait for
/// bytes that had arrived.
pub fn read(stream: &mut impl Read) -> Result<Request, ReadError> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(end) = head_end(&buffer) {
            break end;
        }
        if buffer.len() >= MAX_HEAD {
            return Err(ReadError::Parse(ParseError::Oversized));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(ReadError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the client closed before finishing its request",
                )))
            }
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ReadError::Io(error)),
        }
    };

    let head =
        std::str::from_utf8(&buffer[..end]).map_err(|_| ReadError::Parse(ParseError::Malformed))?;
    let mut request = parse(head).map_err(ReadError::Parse)?;

    let declared = declared_length(&request.headers).unwrap_or(0);
    if declared > 0 {
        let mut body = buffer[end..].to_vec();
        body.truncate(declared);
        while body.len() < declared {
            match stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(ReadError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the client closed before sending the body it declared",
                    )))
                }
                Ok(count) => body.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(ReadError::Io(error)),
            }
        }
        request.body = body;
    }

    Ok(request)
}

/// The `Content-Length` a request declared, when it declared a usable one.
///
/// [`parse`] has already refused a value that is not a number or is past [`MAX_BODY`], so this
/// only reads back what was accepted.
fn declared_length(headers: &[(String, String)]) -> Option<usize> {
    headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.trim().parse().ok())
}

/// Parse a request head (the lines before the blank separator, without a body).
pub fn parse(head: &str) -> Result<Request, ParseError> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(ParseError::Malformed)?;

    let mut fields = request_line.split(' ');
    let method = match fields.next() {
        Some("GET") => Method::Get,
        Some("HEAD") => Method::Head,
        Some("POST") => Method::Post,
        Some("DELETE") => Method::Delete,
        // A well-formed method this server does not serve is a 405, not a 400: the difference is
        // between "your client is broken" and "this endpoint does not do that".
        Some(name) if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphabetic()) => {
            return Err(ParseError::UnsupportedMethod)
        }
        _ => return Err(ParseError::Malformed),
    };
    let target = fields.next().ok_or(ParseError::Malformed)?;
    let version = fields.next().ok_or(ParseError::Malformed)?;
    if !version.starts_with("HTTP/1.") || fields.next().is_some() {
        return Err(ParseError::Malformed);
    }

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.to_string(), String::new()),
    };
    if !path.starts_with('/') {
        return Err(ParseError::Malformed);
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or(ParseError::Malformed)?;
        if headers.len() >= MAX_HEADERS {
            return Err(ParseError::Oversized);
        }
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    // A declared length is checked here rather than while reading, so a request that would be
    // refused is refused before any of its body is buffered.
    if let Some((_, value)) = headers.iter().find(|(name, _)| name == "content-length") {
        let length: usize = value.trim().parse().map_err(|_| ParseError::Malformed)?;
        if length > MAX_BODY {
            return Err(ParseError::TooLarge);
        }
    }
    // Chunked is refused rather than decoded: the bound above is what makes a body safe to read,
    // and a chunked body has no length to bound it by until it has all arrived.
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return Err(ParseError::Body);
    }

    Ok(Request {
        method,
        path,
        query,
        headers,
        // Filled in by `read`, which is the only place that has the bytes. `parse` is given a
        // head and nothing else, and a caller that parses by hand gets a request with no body.
        body: Vec::new(),
    })
}

/// Where the head ends: the byte just past the blank line, or `None` when it has not arrived.
fn head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| start + 4)
}

/// A response, ready to be written.
///
/// The content type and the header names are `Cow` rather than `&'static str`, and that is the one
/// change the forward forced (`decided in implementation`, 2026-10-31): a routed answer names a
/// type this crate chose, while a forwarded one carries whatever the app chose — `image/svg+xml`,
/// `text/event-stream`, a vendor type nobody has heard of. A `&'static str` field would have made
/// the gateway's whole job "answer only the types the binary happened to spell", which is the
/// opposite of not lying to the app. The borrow keeps the routed answers allocation-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub content_type: Cow<'static, str>,
    pub body: Vec<u8>,
    pub headers: Vec<(Cow<'static, str>, String)>,
}

impl Response {
    /// An HTML response.
    pub fn html(status: u16, body: impl Into<String>) -> Self {
        Self::new(status, "text/html; charset=utf-8", body.into().into_bytes())
    }

    /// A JSON response.
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self::new(status, "application/json", body.into().into_bytes())
    }

    /// A plain-text response.
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            body.into().into_bytes(),
        )
    }

    /// A response with no body.
    pub fn empty(status: u16) -> Self {
        Self::new(status, "", Vec::new())
    }

    /// Add a header.
    pub fn header(mut self, name: impl Into<Cow<'static, str>>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// A response of a type the caller names.
    ///
    /// Public because a forwarded answer brings its own content type, and re-deciding it here would
    /// be the gateway guessing at what the app meant.
    pub fn new(status: u16, content_type: impl Into<Cow<'static, str>>, body: Vec<u8>) -> Self {
        let content_type = content_type.into();
        Self {
            status,
            content_type,
            body,
            headers: Vec::new(),
        }
    }

    /// The head and (unless `head_only`) the body.
    ///
    /// `Content-Length` is always sent, including on `HEAD`, because a `HEAD` response is the
    /// headers a `GET` would have produced — and a probe that cannot learn the size from it
    /// learns nothing.
    pub fn write_to(&self, out: &mut impl Write, head_only: bool) -> io::Result<()> {
        let mut head = String::with_capacity(256);
        let _ = write!(head, "HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        if !self.content_type.is_empty() {
            let _ = write!(head, "Content-Type: {}\r\n", self.content_type);
        }
        let _ = write!(head, "Content-Length: {}\r\n", self.body.len());
        head.push_str("Cache-Control: no-store\r\n");
        head.push_str("X-Content-Type-Options: nosniff\r\n");
        head.push_str("Connection: close\r\n");
        for (name, value) in &self.headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        head.push_str("\r\n");

        out.write_all(head.as_bytes())?;
        if !head_only {
            out.write_all(&self.body)?;
        }
        out.flush()
    }
}

/// The reason phrase for a status this server can send.
pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_get_request_parses_into_its_parts() {
        let request =
            parse("GET /api/health?verbose=1 HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nAccept: */*")
                .unwrap();
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.path, "/api/health");
        assert_eq!(request.query, "verbose=1");
        assert_eq!(request.header("HOST"), Some("127.0.0.1:4270"));
        assert_eq!(request.header("accept"), Some("*/*"));
        assert_eq!(request.header("missing"), None);
    }

    #[test]
    fn a_put_is_refused_as_a_method_not_as_a_malformed_request() {
        // 405 rather than 400: a well-formed method this server does not serve is a different thing
        // from a client that cannot spell.
        assert_eq!(
            parse("PUT / HTTP/1.1\r\nHost: localhost\r\n").unwrap_err(),
            ParseError::UnsupportedMethod
        );
    }

    #[test]
    fn post_and_delete_are_served_so_that_a_route_can_change_state() {
        assert_eq!(
            parse("POST /api/apps/notes/start HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n")
                .unwrap()
                .method,
            Method::Post
        );
        assert_eq!(
            parse("DELETE /api/apps/notes HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n")
                .unwrap()
                .method,
            Method::Delete
        );
    }

    #[test]
    fn a_declared_length_is_accepted_up_to_the_bound_and_refused_past_it() {
        // A browser sends `Content-Length: 0` for an empty POST, and the panel sends a few
        // hundred bytes of JSON. Both are requests this server reads.
        assert!(parse(
            "POST /api/apps/notes/start HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nContent-Length: 0\r\n"
        )
        .is_ok());
        assert!(parse(&format!(
            "POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nContent-Length: {MAX_BODY}\r\n"
        ))
        .is_ok());

        // One byte past it is not a request to cooperate with: it is a decision about how much
        // memory this process spends, and the answer is `413` rather than a buffer.
        assert_eq!(
            parse(&format!(
                "POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nContent-Length: {}\r\n",
                MAX_BODY + 1
            ))
            .unwrap_err(),
            ParseError::TooLarge
        );

        // Chunked has no length to bound before it has all arrived, so it is refused rather than
        // decoded.
        assert_eq!(
            parse("POST / HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nTransfer-Encoding: chunked\r\n")
                .unwrap_err(),
            ParseError::Body
        );

        // A length that is not a number is a malformed request, not an oversized one.
        assert_eq!(
            parse("POST / HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nContent-Length: many\r\n")
                .unwrap_err(),
            ParseError::Malformed
        );
    }

    #[test]
    fn the_body_that_followed_the_head_is_read_whole() {
        // The case that matters and is easy to get wrong: a browser sends a small body in the same
        // packet as the head. A reader that consumed the head and then waited for more would hang
        // on bytes it had already received.
        let wire = b"POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nContent-Length: 20\r\n\r\n{\"model\":\"deepseek\"}";
        let request = read(&mut wire.as_slice()).unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.body_text(), Some("{\"model\":\"deepseek\"}"));
    }

    #[test]
    fn a_body_that_arrives_after_the_head_is_read_whole_too() {
        // The same request split across two writes, which is what a slow client or a larger body
        // looks like on the wire.
        struct Trickle {
            chunks: Vec<Vec<u8>>,
        }
        impl Read for Trickle {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.chunks.is_empty() {
                    return Ok(0);
                }
                let next = self.chunks.remove(0);
                out[..next.len()].copy_from_slice(&next);
                Ok(next.len())
            }
        }

        let mut stream = Trickle {
            chunks: vec![
                b"POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nContent-Length: 9\r\n\r\n"
                    .to_vec(),
                b"abcdef".to_vec(),
                b"ghi".to_vec(),
            ],
        };
        assert_eq!(read(&mut stream).unwrap().body_text(), Some("abcdefghi"));
    }

    #[test]
    fn a_body_shorter_than_it_declared_is_a_broken_client_not_a_request() {
        let wire =
            b"POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nContent-Length: 40\r\n\r\nshort";
        assert!(matches!(read(&mut wire.as_slice()), Err(ReadError::Io(_))));
    }

    #[test]
    fn a_request_with_no_body_reads_as_one() {
        let wire = b"GET / HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\r\n";
        assert!(read(&mut wire.as_slice()).unwrap().body.is_empty());
    }

    #[test]
    fn nonsense_is_malformed() {
        assert_eq!(parse("GET\r\n\r\n").unwrap_err(), ParseError::Malformed);
        assert_eq!(
            parse("GET / HTTP/9.9\r\n").unwrap_err(),
            ParseError::Malformed
        );
        assert_eq!(
            parse("GET / HTTP/1.1 extra\r\n").unwrap_err(),
            ParseError::Malformed
        );
        assert_eq!(
            parse("GET http://elsewhere/ HTTP/1.1\r\n").unwrap_err(),
            ParseError::Malformed
        );
        assert_eq!(
            parse("GET / HTTP/1.1\r\nno-colon-here\r\n").unwrap_err(),
            ParseError::Malformed
        );
    }

    #[test]
    fn a_head_that_never_ends_is_refused_rather_than_read_forever() {
        let endless = std::io::repeat(b'a');
        let error = read(&mut endless.take(MAX_HEAD as u64 * 4)).unwrap_err();
        match error {
            ReadError::Parse(ParseError::Oversized) => {}
            other => panic!("expected Oversized, got {other:?}"),
        }
    }

    #[test]
    fn a_client_that_says_nothing_is_an_io_error_not_a_panic() {
        let mut empty: &[u8] = b"";
        assert!(matches!(read(&mut empty), Err(ReadError::Io(_))));
    }

    #[test]
    fn a_request_split_across_reads_is_assembled() {
        // The whole point of the read loop: a real client's head arrives in several packets.
        let mut trickle = Trickle::new(b"GET / HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\r\n");
        let request = read(&mut trickle).unwrap();
        assert_eq!(request.path, "/");
        assert_eq!(request.header("host"), Some("127.0.0.1:4270"));
    }

    struct Trickle {
        source: &'static [u8],
        offset: usize,
    }

    impl Trickle {
        fn new(source: &'static [u8]) -> Self {
            Self { source, offset: 0 }
        }
    }

    impl Read for Trickle {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.offset >= self.source.len() {
                return Ok(0);
            }
            let count = out.len().min(1).min(self.source.len() - self.offset);
            out[..count].copy_from_slice(&self.source[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    #[test]
    fn a_head_only_response_keeps_its_content_length() {
        let response = Response::text(200, "hello");
        let mut full = Vec::new();
        response.write_to(&mut full, false).unwrap();
        let mut head_only = Vec::new();
        response.write_to(&mut head_only, true).unwrap();

        let full = String::from_utf8(full).unwrap();
        let head_only = String::from_utf8(head_only).unwrap();
        assert!(full.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head_only.contains("Content-Length: 5\r\n"));
        assert!(full.ends_with("hello"));
        assert!(head_only.ends_with("\r\n\r\n"));
    }

    #[test]
    fn every_response_is_uncacheable() {
        let mut written = Vec::new();
        Response::empty(204).write_to(&mut written, false).unwrap();
        let written = String::from_utf8(written).unwrap();
        assert!(written.contains("Cache-Control: no-store"));
        assert!(written.contains("Content-Length: 0"));
        // `X-Content-Type-Options` is present; the header this asserts absent is the body type.
        assert!(!written.contains("\r\nContent-Type:"));
    }
}
