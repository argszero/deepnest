//! The outbound half of the door: the HTTP client a model call goes through.
//!
//! `crates/llm` decides *what* a failure means and never touches a socket; this is the adapter
//! that moves the bytes, which the design puts here (`design.md`, the agent's turn loop: "the
//! adapter belongs to `crates/gateway`"). One module, one trait, so every rule in `llm` stays
//! testable with no network.
//!
//! **It speaks plaintext HTTP/1.1 and says so.** A TLS client is a dependency tree — a certificate
//! store, a cipher suite, a trust decision — and this binary ships without one today. The two
//! honest options are to refuse an `https` endpoint by name or to quietly send a request that
//! cannot arrive; the second is a hang the user would blame on their key. So an `https` endpoint is
//! refused with a message that names what is missing, and everything a local model server
//! (`http://127.0.0.1:11434` and friends) needs works today.
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

/// How long a connection may take to establish, and how long a read may block.
///
/// The read budget is generous because a model can think for a while before its first byte; the
/// connect budget is short because a machine that cannot reach an address in ten seconds is not
/// going to reach it.
const CONNECT: Duration = Duration::from_secs(10);
const IO: Duration = Duration::from_secs(180);

/// The plaintext HTTP adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlainHttp;

impl PlainHttp {
    pub fn new() -> Self {
        Self
    }
}

/// A URL taken apart: where to connect, and what to ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    host: String,
    port: u16,
    path: String,
}

fn parse(url: &str) -> Result<Target, LlmError> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        LlmError::transport(format!(
            "`{url}` has no scheme — a model endpoint is `http://host[:port][/path]`"
        ))
    })?;
    if scheme.eq_ignore_ascii_case("https") {
        return Err(LlmError::transport(format!(
            "`{url}` is https, and this build has no TLS client: the request cannot be made. \
             Point the endpoint at a local model server over http, or terminate TLS in front of \
             it"
        )));
    }
    if !scheme.eq_ignore_ascii_case("http") {
        return Err(LlmError::transport(format!(
            "`{url}` uses `{scheme}`, and only http is supported"
        )));
    }
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
        None => (authority.to_string(), 80),
    };
    Ok(Target {
        host,
        port,
        path: path.to_string(),
    })
}

impl Transport for PlainHttp {
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, LlmError> {
        // Before the socket: a header this function refuses must be refused whether or not the
        // endpoint happens to be reachable, or a test of the refusal measures the network instead.
        for (name, value) in &request.headers {
            // A header name or value with a newline in it would inject a second request. Nothing
            // here produces one, and a transport that trusts its caller is how it starts.
            if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
                return Err(LlmError::transport(format!(
                    "refused a header with a line break in it: `{name}`"
                )));
            }
        }

        let target = parse(&request.url)?;
        let mut stream = connect(&target, &request.url)?;

        let mut head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            target.path,
            host_header(&target),
            request.body.len()
        );
        for (name, value) in &request.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");

        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(request.body.as_bytes()))
            .and_then(|()| stream.flush())
            .map_err(|e| LlmError::transport(format!("the request could not be sent: {e}")))?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| LlmError::transport(format!("the answer could not be read: {e}")))?;
        read_response(&raw)
    }

    fn post_stream(
        &self,
        _request: &HttpRequest,
        _on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
    ) -> Result<(), LlmError> {
        // Stated rather than left as a `todo!()`: the loop above this seam asks for a whole answer
        // (`Client::turn`), and an adapter that pretended to stream would deliver one event at the
        // end, which is a worse lie than a refusal.
        Err(LlmError::transport(
            "this build asks for a whole answer, not a stream — the model adapter has no \
             streaming path",
        ))
    }
}

fn host_header(target: &Target) -> String {
    if target.port == 80 {
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
    fn an_https_endpoint_is_refused_by_name() {
        let error = post_to("https://api.example.com/v1/chat/completions").unwrap_err();
        assert!(error.body.contains("no TLS client"), "{}", error.body);
        assert!(
            error.body.contains("https://api.example.com"),
            "{}",
            error.body
        );
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
}
