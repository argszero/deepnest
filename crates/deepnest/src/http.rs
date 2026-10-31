//! Exactly the HTTP/1.1 this binary needs: read one request, write one response, close.
//!
//! Deliberately not a server library. `crates/gateway` needs a real one — routing, header
//! rewriting, WebSocket upgrade, unbuffered SSE — and choosing that stack is that crate's
//! decision to make, not this one's. What this module has to get right is much smaller, and it
//! is all about refusing clearly: a request with no `Host`, a request line that is not a
//! request line, a client that opens a connection and says nothing, a head larger than any real
//! client sends. Every one of those answers with a status the caller can see instead of hanging
//! or being half-parsed.

use std::fmt::Write as _;
use std::io::{self, Read, Write};

/// The largest request head accepted, in bytes.
///
/// A head this large is not a request any browser or probe sends; it is someone deciding how
/// much memory to make this process spend.
pub const MAX_HEAD: usize = 8 * 1024;

/// How many header fields are accepted before the request is refused.
pub const MAX_HEADERS: usize = 64;

/// The two methods this binary serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
}

impl Method {
    /// The method as it appeared in the request line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
        }
    }
}

/// A parsed request head. The body is never read: nothing here accepts one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// The path, with the query string removed.
    pub path: String,
    /// The query string, without its `?`; empty when there was none.
    pub query: String,
    /// Header names lowercased, values trimmed, in the order they arrived.
    pub headers: Vec<(String, String)>,
}

impl Request {
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
}

/// Why [`read`] did not produce a request.
#[derive(Debug)]
pub enum ReadError {
    /// The transport failed, or the client left mid-request.
    Io(io::Error),
    /// There was a head, and it was not a request this server accepts.
    Parse(ParseError),
}

/// Read one request head from `stream`.
pub fn read(stream: &mut impl Read) -> Result<Request, ReadError> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = head_end(&buffer) {
            let head = std::str::from_utf8(&buffer[..end])
                .map_err(|_| ReadError::Parse(ParseError::Malformed))?;
            return parse(head).map_err(ReadError::Parse);
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
    }
}

/// Parse a request head (the lines before the blank separator, without a body).
pub fn parse(head: &str) -> Result<Request, ParseError> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(ParseError::Malformed)?;

    let mut fields = request_line.split(' ');
    let method = match fields.next() {
        Some("GET") => Method::Get,
        Some("HEAD") => Method::Head,
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

    Ok(Request {
        method,
        path,
        query,
        headers,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
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
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
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
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        500 => "Internal Server Error",
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
    fn a_post_is_refused_as_a_method_not_as_a_malformed_request() {
        assert_eq!(
            parse("POST / HTTP/1.1\r\nHost: localhost\r\n").unwrap_err(),
            ParseError::UnsupportedMethod
        );
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
