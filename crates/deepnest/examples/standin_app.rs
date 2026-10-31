//! A stand-in app: the smallest program that is still a real app under `/app/<slug>/`.
//!
//! `crates/apps` starts an app by running `<app>/target/release/app` with the allocated port in
//! `PORT` and nothing else arranged for it. This example honours exactly that contract, so the
//! end-to-end test (`tests/through_the_binary.rs`) can exercise the real supervisor launching a
//! real child process without needing a build toolchain, a package registry or a front end: the
//! test copies this binary into the app directory under the name the contract fixes.
//!
//! It serves the four things the door has to carry, and each one exists because a way of getting
//! it wrong has to be visible:
//!
//! - `/` a page, so the injected `<base href>` can be checked;
//! - `/api/items` a JSON answer with a length, so the ordinary path stays covered;
//! - `/events` a **slow, chunked** event stream — slow on purpose, because a body that is
//!   buffered anywhere between here and the browser delivers its first event only when the last
//!   one arrives, and that is the defect this route is here to catch;
//! - `/ws` a WebSocket upgrade that then echoes, so an upgraded connection can be shown to carry
//!   bytes in both directions rather than only to be agreed to.
//!
//! It is an example rather than a binary: `cargo test` builds it, `cargo install` does not ship
//! it, and nothing it does belongs in deepnest itself.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sha1::{Digest, Sha1};

/// The constant every WebSocket handshake mixes into its accept key (RFC 6455 §4.2.2).
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The page. No `<base>` of its own: the door is supposed to add one, and a page that declared
/// its own would be left alone — a different case, covered by a unit test.
const PAGE: &str =
    "<!doctype html><html><head><title>Notes</title></head><body><h1>Notes</h1></body></html>";

/// How long this process lives if nobody stops it.
///
/// The test that starts it stops it through the API in the normal case, but a test that fails
/// half-way never gets there, and a stand-in app holding a thread and a port for the rest of the
/// session is exactly the kind of leak that makes the next run mysterious. Two minutes is longer
/// than the whole test and shorter than anyone's patience.
const LIFETIME: Duration = Duration::from_secs(120);

fn main() {
    let port = match std::env::var("PORT").map(|value| value.parse::<u16>()) {
        Ok(Ok(port)) => port,
        _ => {
            let _ = writeln!(
                io::stderr(),
                "standin-app: $PORT must be the port to serve on (crates/apps sets it)"
            );
            std::process::exit(2);
        }
    };

    thread::spawn(|| {
        thread::sleep(LIFETIME);
        std::process::exit(0);
    });

    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = writeln!(io::stderr(), "standin-app: cannot bind {port}: {error}");
            std::process::exit(1);
        }
    };
    let _ = writeln!(io::stderr(), "standin-app listening on {port}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // One thread per connection, for the reason a stream needs one: an event stream and a
        // WebSocket both outlive the request that opened them.
        thread::spawn(move || {
            if let Err(error) = handle(stream) {
                let _ = writeln!(io::stderr(), "standin-app: {error}");
            }
        });
    }
}

/// One connection: read a request head, then answer by path.
fn handle(mut stream: TcpStream) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let head = match read_head(&mut stream)? {
        // The supervisor probes the port and leaves, which is not a request to answer.
        None => return Ok(()),
        Some(head) => head,
    };

    let mut parts = head.lines().next().unwrap_or_default().split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    let _ = method;
    match path.as_str() {
        "/events" => stream_events(&mut stream),
        "/ws" => upgrade(&mut stream, header("sec-websocket-key")),
        "/api/items" => reply(&mut stream, "application/json", br#"{"items":[1]}"#),
        _ => reply(&mut stream, "text/html; charset=utf-8", PAGE.as_bytes()),
    }
}

/// Read the head of one request, `None` when the peer said nothing and left.
fn read_head(stream: &mut TcpStream) -> io::Result<Option<String>> {
    let mut seen = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        seen.extend_from_slice(&chunk[..read]);
        if seen.windows(4).any(|window| window == b"\r\n\r\n") {
            // Latin-1: a header is bytes, and this app only ever looks at ASCII names.
            return Ok(Some(seen.iter().map(|byte| *byte as char).collect()));
        }
        if seen.len() > 64 * 1024 {
            return Ok(None);
        }
    }
}

/// An ordinary answer: length first, then the bytes.
fn reply(stream: &mut TcpStream, content_type: &str, body: &[u8]) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

/// A stream of server-sent events, chunked and unbuffered, one every 400 ms.
///
/// The pauses are the whole value of this route: a proxy that buffers the body flushes once, at
/// the end, and the first event then arrives at the same moment as the last.
fn stream_events(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-store\r\n\
          transfer-encoding: chunked\r\n\r\n",
    )?;
    stream.flush()?;
    for index in 0..3 {
        let event = format!("data:{index}\n\n");
        write!(stream, "{:x}\r\n{event}\r\n", event.len())?;
        stream.flush()?;
        thread::sleep(Duration::from_millis(400));
    }
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()?;
    // The stream is over, so the connection is too — the door forwards the app's framing, and a
    // chunked body with no length ends when the app says so.
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

/// Accept the upgrade with the key the client sent, then be a byte tunnel.
///
/// The accept is computed the way the RFC says, so the value the other end sees is the one a
/// browser would check: if anything in between rewrote `Sec-WebSocket-Accept`, the check fails
/// for the same reason a real browser's would.
fn upgrade(stream: &mut TcpStream, key: Option<&str>) -> io::Result<()> {
    let key = key.unwrap_or_default();
    let mut digest = Sha1::new();
    digest.update(key.as_bytes());
    digest.update(GUID.as_bytes());
    let accept = BASE64.encode(digest.finalize());

    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\
         sec-websocket-accept: {accept}\r\n\r\n"
    )?;
    stream.flush()?;

    // Past the handshake this is not HTTP any more. Echoing is the smallest honest frame protocol
    // a stand-in can speak, and it proves both directions cross rather than only the handshake.
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                stream.write_all(&chunk[..count])?;
                stream.flush()?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}
