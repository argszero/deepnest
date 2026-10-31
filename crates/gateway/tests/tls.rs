//! What `crates/gateway::transport` does once the endpoint is `https`.
//!
//! Every test here runs a **real TLS listener** on loopback, holding one of the two throwaway
//! identities in `tests/fixtures/tls/`. Nothing is mocked: the handshake, the certificate check
//! and the record layer are the ones `native-tls` performs on the platform running the test, which
//! is the only way to learn what this machine actually does with a certificate it should refuse.
//!
//! **The trust decision is the test's, not the machine's.** Every connector below is built with
//! `disable_built_in_roots(true)` and then handed exactly the roots the test means it to have, so a
//! green run cannot depend on what happens to be installed in the store of whichever machine ran
//! it. The one test that deliberately uses the platform store says so.
//!
//! The fixture keys protect nothing — see `tests/fixtures/tls/README.md`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

use deepnest_gateway::{HttpTransport, TlsHttp};
use deepnest_llm::{HttpRequest, Transport};
use native_tls::{Certificate, Identity, TlsAcceptor, TlsConnector};

/// Self-signed, SAN `IP:127.0.0.1` — the name a test dials, so a connector that trusts it accepts.
const LOOPBACK_CERT: &str = include_str!("fixtures/tls/loopback.pem");
const LOOPBACK_KEY: &str = include_str!("fixtures/tls/loopback.key");
/// Self-signed, SAN `DNS:wrong.example` — trusted as a root, so the only thing wrong with it is its
/// name.
const OTHER_CERT: &str = include_str!("fixtures/tls/other-name.pem");
const OTHER_KEY: &str = include_str!("fixtures/tls/other-name.key");

fn acceptor(cert: &str, key: &str) -> TlsAcceptor {
    let identity = Identity::from_pkcs8(cert.as_bytes(), key.as_bytes()).expect("a test identity");
    TlsAcceptor::new(identity).expect("a test acceptor")
}

/// A connector that trusts `root` and nothing else.
fn trusting(root: &str) -> TlsConnector {
    let mut builder = TlsConnector::builder();
    builder.disable_built_in_roots(true);
    builder.add_root_certificate(Certificate::from_pem(root.as_bytes()).expect("a test root"));
    builder.build().expect("a test connector")
}

/// A one-shot TLS server: one handshake, the request it read, and `response` written back.
///
/// It hands back the URL to post to and a channel carrying the bytes the client sent. A client that
/// refuses the certificate makes the server's `accept` fail, which ends this thread quietly —
/// that is the outcome one of the tests below is checking for.
fn serve(cert: &str, key: &str, response: Vec<u8>) -> (String, mpsc::Receiver<String>) {
    let acceptor = acceptor(cert, key);
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((socket, _)) = listener.accept() else {
            return;
        };
        let Ok(mut tls) = acceptor.accept(socket) else {
            return;
        };
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        // A head and its body arrive as separate writes, so read until the declared length is here
        // rather than until one `read` returns.
        loop {
            match tls.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    seen.extend_from_slice(&buf[..read]);
                    if complete(&seen) {
                        break;
                    }
                }
            }
        }
        let _ = tls.write_all(&response);
        let _ = tls.flush();
        let _ = tx.send(String::from_utf8_lossy(&seen).to_string());
        // Dropping `tls` here is what ends the client's read: a `Connection: close` answer is only
        // whole once the other end has closed.
    });
    (format!("https://127.0.0.1:{port}/v1/chat/completions"), rx)
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

fn request(url: &str) -> HttpRequest {
    HttpRequest {
        url: url.to_string(),
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("Authorization".into(), "Bearer sk-not-a-real-key".into()),
        ],
        body: r#"{"model":"m","stream":true}"#.to_string(),
    }
}

/// An answer with a declared length, built so a body can be counted rather than guessed.
fn declared(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[test]
fn a_trusted_certificate_carries_a_whole_sse_answer() {
    // The bytes an OpenAI-compatible endpoint streams, read whole: this build asks for a whole
    // answer (`Client::turn`), so what has to arrive intact is the event text.
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let (url, seen) = serve(LOOPBACK_CERT, LOOPBACK_KEY, declared(sse));

    let answer = TlsHttp::with_connector(trusting(LOOPBACK_CERT))
        .post(&request(&url))
        .expect("a trusted certificate is accepted");
    assert_eq!(answer.status, 200);
    assert_eq!(
        answer.body, sse,
        "the event text crossed the handshake whole"
    );

    let sent = seen.recv().expect("the server saw a request");
    assert!(
        sent.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"),
        "{sent}"
    );
    // The rules are the plaintext adapter's, unchanged: one request per connection, and the
    // `Host` header names the authority the certificate was checked against.
    assert!(sent.contains("Connection: close"), "{sent}");
    assert!(
        sent.contains(&format!(
            "Host: {}",
            url.trim_start_matches("https://")
                .split('/')
                .next()
                .unwrap()
        )),
        "{sent}"
    );
    assert!(
        sent.contains("Authorization: Bearer sk-not-a-real-key"),
        "{sent}"
    );
}

#[test]
fn a_certificate_the_seam_does_not_trust_is_refused_and_says_so() {
    let (url, _seen) = serve(LOOPBACK_CERT, LOOPBACK_KEY, declared("{}"));

    // The platform's own roots, which this self-signed fixture is not in: whatever this machine has
    // installed, it did not install the certificate this test just minted.
    let platform = TlsConnector::builder()
        .build()
        .expect("a platform connector");
    let error = TlsHttp::with_connector(platform)
        .post(&request(&url))
        .unwrap_err();

    assert!(
        error.body.contains("not one this machine trusts"),
        "{}",
        error.body
    );
    assert!(error.body.contains("127.0.0.1"), "{}", error.body);
    // The three-way split's second branch: a certificate fault is never reported as a connection
    // that could not be made.
    assert!(
        !error.body.contains("could not be reached"),
        "{}",
        error.body
    );
    // The join, which the wording assertions above let through: the platform's sentence ends in a
    // full stop and so does the one that follows it.
    assert!(!error.body.contains(".."), "{}", error.body);
    assert!(error.body.ends_with('.'), "{}", error.body);
}

#[test]
fn a_certificate_for_another_name_is_refused_for_that_reason() {
    let (url, _seen) = serve(OTHER_CERT, OTHER_KEY, declared("{}"));

    // Trusted as a root, so the chain is fine and the only fault left is the name: the server
    // presents `wrong.example` and the client dialled `127.0.0.1`.
    let error = TlsHttp::with_connector(trusting(OTHER_CERT))
        .post(&request(&url))
        .unwrap_err();

    // macOS is where this wording was measured: "A host name mismatch has occurred." Windows
    // reports the same fault in its own sentence. OpenSSL collapses both certificate faults into
    // `certificate verify failed`, so on Linux the message still names a refused certificate —
    // which is the line this test holds everywhere: never "connection failed".
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    assert!(
        error.body.contains("for a different name"),
        "{}",
        error.body
    );
    #[cfg(target_os = "linux")]
    assert!(
        error.body.contains("for a different name")
            || error.body.contains("not one this machine trusts"),
        "{}",
        error.body
    );
    assert!(
        !error.body.contains("could not be reached"),
        "{}",
        error.body
    );
    assert!(!error.body.contains(".."), "{}", error.body);
    assert!(error.body.ends_with('.'), "{}", error.body);
}

#[test]
fn a_handshake_that_fails_for_another_reason_still_ends_its_sentence() {
    // A listener that answers in HTTP to a client expecting TLS: the record layer rejects it before
    // any certificate is looked at, which is the third of the three messages — and the only one whose
    // detail is the end of the sentence rather than the middle, so it is where a missing full stop
    // would show instead of a doubled one.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
            let _ = sock.flush();
        }
    });

    let error = TlsHttp::with_connector(trusting(LOOPBACK_CERT))
        .post(&request(&format!("https://127.0.0.1:{port}/v1")))
        .unwrap_err();
    assert!(
        error.body.contains("failed before any request was sent"),
        "{}",
        error.body
    );
    assert!(error.body.ends_with('.'), "{}", error.body);
    assert!(!error.body.contains(".."), "{}", error.body);
}

#[test]
fn an_unreachable_https_endpoint_says_it_could_not_be_reached() {
    // A port the OS just handed out and nothing is listening on: the third of the three messages.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let error = TlsHttp::with_connector(trusting(LOOPBACK_CERT))
        .post(&request(&format!("https://127.0.0.1:{port}/v1")))
        .unwrap_err();
    assert!(
        error.body.contains("could not be reached"),
        "{}",
        error.body
    );
    // The other two branches must not be reachable from here: no certificate was ever presented.
    assert!(!error.body.contains("certificate"), "{}", error.body);
}

#[test]
fn a_body_shorter_than_its_content_length_is_still_an_error_over_tls() {
    // Half an answer parsed as a whole one is a tool call run on a fragment, and TLS makes that no
    // more acceptable than plaintext does.
    let truncated = b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\n{\"choices\":".to_vec();
    let (url, _seen) = serve(LOOPBACK_CERT, LOOPBACK_KEY, truncated);

    let error = TlsHttp::with_connector(trusting(LOOPBACK_CERT))
        .post(&request(&url))
        .unwrap_err();
    assert!(error.body.contains("truncated"), "{}", error.body);
}

#[test]
fn a_chunked_answer_is_reassembled_over_tls_too() {
    let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"ok\r\n4\r\n\":1}\r\n0\r\n\r\n"
        .to_vec();
    let (url, _seen) = serve(LOOPBACK_CERT, LOOPBACK_KEY, chunked);

    let answer = TlsHttp::with_connector(trusting(LOOPBACK_CERT))
        .post(&request(&url))
        .expect("a chunked answer over TLS");
    assert_eq!(answer.body, r#"{"ok":1}"#);
}

#[test]
fn the_transport_the_binary_wires_in_sends_https_over_tls() {
    // `HttpTransport` is what `crates/deepnest` constructs. If it dispatched by anything other than
    // the scheme, this would arrive as plaintext bytes at a TLS listener and fail the handshake.
    let sse = "data: [DONE]\n\n";
    let (url, seen) = serve(LOOPBACK_CERT, LOOPBACK_KEY, declared(sse));

    let answer = HttpTransport::with_tls(TlsHttp::with_connector(trusting(LOOPBACK_CERT)))
        .post(&request(&url))
        .expect("the dispatching transport hands https to the TLS half");
    assert_eq!(answer.body, sse);
    assert!(seen
        .recv()
        .expect("the server saw a request")
        .starts_with("POST "));
}

#[test]
fn a_line_break_in_a_header_is_refused_before_the_handshake() {
    // Before, because a refusal after the handshake measures the endpoint's reachability instead of
    // the refusal.
    let mut with_break = request("https://127.0.0.1:1/v1");
    with_break.headers = vec![("X-Thing".into(), "a\r\nX-Injected: b".into())];
    let error = TlsHttp::with_connector(trusting(LOOPBACK_CERT))
        .post(&with_break)
        .unwrap_err();
    assert!(error.body.contains("line break"), "{}", error.body);
}

#[test]
fn streaming_is_still_a_refusal_rather_than_a_lie() {
    // Nothing above changes this seam: the loop asks for a whole answer, and an adapter that
    // delivered every event at the end would look like a stream and behave like a lie.
    let mut sink = |_event: deepnest_llm::SseEvent| Ok(());
    let error = TlsHttp::with_connector(trusting(LOOPBACK_CERT))
        .post_stream(&request("https://127.0.0.1:1/v1"), &mut sink)
        .unwrap_err();
    assert!(error.body.contains("no streaming path"), "{}", error.body);
}

/// A live request against a public `https` host, opt-in because CI has to stay deterministic and a
/// test that needs the internet fails for reasons that are not about this code.
///
/// ```sh
/// cargo test -p deepnest-gateway --test tls -- --ignored live
/// ```
///
/// What it adds over the loopback tests above: a real certificate chain and a real hostname, both
/// checked by this machine's own trust store through `TlsHttp::system()` — the path the binary
/// actually runs. A status of any kind is a pass; the transport's job is to have got there.
#[test]
#[ignore]
fn live_https_smoke() {
    let answer = TlsHttp::system()
        .post(&request("https://api.github.com/"))
        .expect("a real https host answers");
    assert!(
        (100..500).contains(&answer.status),
        "a real status: {}",
        answer.status
    );
    assert!(!answer.body.is_empty(), "a real body came back");
    println!(
        "live https: status {} body {} bytes",
        answer.status,
        answer.body.len()
    );
}
