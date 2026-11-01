//! The binary, started and stopped **inside this test process**.
//!
//! Everything here runs in one process and one thread's worth of real sockets on loopback: no
//! child process, no daemon, nothing to kill afterwards. That matters beyond tidiness — the
//! properties being checked (the port is taken exclusively, the token heals, the routes answer)
//! are properties of *this* process's listener, and starting a second program to observe them
//! would test the supervisor instead.
//!
//! The port is always `0` (let the OS choose). A test that bound 4270 would fail on a machine
//! where the developer happens to be running deepnest, and would say nothing about the code.
//!
//! `tests/through_the_binary.rs` is the one exception, and it is the opposite case rather than an
//! oversight: it starts the *program*, whose port the design fixes at 4270, so it takes that port
//! and says so when the port is already held.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use deepnest::server::{self, BindAttempts, BindError, ServerOptions};
use deepnest::token::{self, Token};

/// Send one raw request and read the whole answer back.
fn request(addr: SocketAddr, head: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to the server this test started");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(head.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    String::from_utf8(answer).unwrap()
}

fn get(addr: SocketAddr, target: &str, extra: &str) -> String {
    request(
        addr,
        &format!(
            "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{extra}\r\n",
            addr.port()
        ),
    )
}

fn brisk() -> BindAttempts {
    BindAttempts {
        attempts: 2,
        delay: Duration::from_millis(10),
    }
}

struct Scratch {
    base: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("deepnest-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        Self { base }
    }

    fn token_path(&self) -> PathBuf {
        self.base.join(token::FILE_NAME)
    }

    fn config_path(&self) -> PathBuf {
        self.base.join(deepnest::config::FILE_NAME)
    }

    fn apps(&self) -> PathBuf {
        deepnest_apps::apps_dir(&self.base)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn start(scratch: &Scratch, model: Option<&str>) -> (server::Server, Token) {
    let token = token::ensure(&scratch.token_path()).unwrap();
    let started = server::start(ServerOptions {
        port: 0,
        token: Some(token.clone()),
        token_path: Some(scratch.token_path()),
        model: model.map(str::to_string),
        apps_dir: Some(scratch.apps()),
        lifecycle: None,
        config_path: Some(scratch.config_path()),
        keepalive: Duration::from_millis(20),
        bind: brisk(),
    })
    .unwrap();
    (started, token)
}

#[test]
fn the_panel_and_the_health_route_answer_over_a_real_socket() {
    let scratch = Scratch::new("routes");
    let (running, _token) = start(&scratch, Some("deepseek-flash"));
    let addr = running.addr();

    let panel = get(addr, "/", "");
    assert!(panel.starts_with("HTTP/1.1 200 OK"), "{panel}");
    assert!(panel.contains("deepnest"));
    assert!(panel.contains(&format!("127.0.0.1:{}", addr.port())));
    assert!(panel.contains("deepseek-flash"));
    // The panel's script is a route, not an inline block, so the policy can refuse an inline one:
    // a string that reaches the page from an app's directory cannot become code.
    assert!(panel.contains("/panel.js"), "{panel}");
    assert!(panel.contains("Content-Security-Policy"), "{panel}");
    assert!(panel.contains("script-src 'self'"), "{panel}");

    let health = get(addr, "/api/health", "");
    assert!(health.starts_with("HTTP/1.1 200 OK"), "{health}");
    assert!(health.contains("\"status\":\"ok\""));
    assert!(health.contains("Content-Type: application/json"));

    running.shutdown();
}

#[test]
fn the_dialog_assets_answer_over_a_real_socket_without_the_token() {
    // The app page that loads them cannot carry the control token — its app's own script shares
    // the origin — so these two routes answer without one, like `/panel.js`.
    let scratch = Scratch::new("dialog");
    let (running, _token) = start(&scratch, Some("deepseek-flash"));
    let addr = running.addr();

    let script = get(addr, "/dialog.js", "");
    assert!(script.starts_with("HTTP/1.1 200 OK"), "{script}");
    assert!(script.contains("Content-Type: text/javascript"), "{script}");
    assert!(script.contains("deepnest-dialog"), "{script}");
    // Nothing about the control token reaches a page the app's script can read.
    assert!(!script.contains("deepnest-token"), "{script}");

    let style = get(addr, "/dialog.css", "");
    assert!(style.starts_with("HTTP/1.1 200 OK"), "{style}");
    assert!(style.contains("Content-Type: text/css"), "{style}");
    assert!(style.contains("#deepnest-dialog"), "{style}");

    running.shutdown();
}

#[test]
fn the_token_gate_is_enforced_end_to_end() {
    // The unit tests call the router directly; this one goes through the socket, the header parse
    // and the write path, because that is where an auth check stops being a check.
    let scratch = Scratch::new("auth");
    deepnest_apps::Manifest::new("Notes")
        .save(&scratch.apps().join("notes"))
        .unwrap();
    let (running, token) = start(&scratch, None);
    let addr = running.addr();

    let refused = get(addr, "/api/apps", "");
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
    assert!(!refused.contains("notes"), "the refusal leaked the list");

    let accepted = get(
        addr,
        "/api/apps",
        &format!("Authorization: Bearer {token}\r\n"),
    );
    assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
    assert!(accepted.contains("\"slug\":\"notes\""), "{accepted}");
    assert!(accepted.contains("\"name\":\"Notes\""), "{accepted}");

    running.shutdown();
}

#[test]
fn a_rebinding_host_is_refused_end_to_end() {
    let scratch = Scratch::new("host");
    let (running, _token) = start(&scratch, None);
    let addr = running.addr();

    let answer = request(
        addr,
        "GET / HTTP/1.1\r\nHost: rebind.attacker.example\r\n\r\n",
    );
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");

    running.shutdown();
}

#[test]
fn head_answers_with_the_headers_a_get_would_have() {
    let scratch = Scratch::new("head");
    let (running, _token) = start(&scratch, None);
    let addr = running.addr();

    let answer = request(
        addr,
        &format!("HEAD / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", addr.port()),
    );
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    assert!(answer.contains("Content-Length:"));
    // The head is all there is: nothing follows the blank line.
    assert!(answer.ends_with("\r\n\r\n"), "{answer}");

    running.shutdown();
}

#[test]
fn a_method_this_server_does_not_serve_is_refused_with_a_method_not_allowed() {
    let scratch = Scratch::new("method");
    let (running, _token) = start(&scratch, None);
    let addr = running.addr();

    // PUT is a real HTTP method this build does not answer; the refusal names the four it does,
    // so a client does not have to guess.
    let answer = request(
        addr,
        &format!("PUT / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", addr.port()),
    );
    assert!(answer.starts_with("HTTP/1.1 405"), "{answer}");
    assert!(
        answer.contains("Allow: GET, HEAD, POST, DELETE"),
        "{answer}"
    );

    running.shutdown();
}

#[test]
fn a_body_sent_to_a_route_that_reads_none_is_refused_rather_than_ignored() {
    let scratch = Scratch::new("body");
    let (running, token) = start(&scratch, None);
    let addr = running.addr();

    // `POST /api/config` is the one route that reads a body. Answering any other route anyway would
    // report "done" to a request whose content was never seen.
    let answer = request(
        addr,
        &format!(
            "POST /api/apps/notes/start HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: 8\r\n\r\nanything",
            addr.port(),
            token.as_str()
        ),
    );
    assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
    assert!(answer.contains("/api/config"), "{answer}");

    running.shutdown();
}

#[test]
fn the_lifecycle_route_answers_over_a_real_socket() {
    // This server was started without a lifecycle, so the answer is the honest one: the route
    // exists, needs the token, and says this process cannot run an app. What the supervisor does
    // with a real app is `deepnest-apps`' test, and the router's with a stand-in.
    let scratch = Scratch::new("lifecycle");
    let (running, token) = start(&scratch, None);
    let addr = running.addr();

    let refused = request(
        addr,
        &format!(
            "POST /api/apps/notes/start HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            addr.port()
        ),
    );
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");

    let answered = request(
        addr,
        &format!(
            "POST /api/apps/notes/start HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\r\n",
            addr.port(),
            token.as_str()
        ),
    );
    assert!(answered.starts_with("HTTP/1.1 503"), "{answered}");
    assert!(answered.contains("without a lifecycle"), "{answered}");

    running.shutdown();
}

#[test]
fn the_port_is_held_while_it_serves_and_free_afterwards() {
    let scratch = Scratch::new("admission");
    let (running, _token) = start(&scratch, None);
    let port = running.port();

    // The single-instance rule, observed from outside: while one server listens, a second bind
    // on the same port is refused by the kernel.
    match server::bind(port, brisk()) {
        Err(BindError::InUse { .. }) => {}
        other => panic!("expected the second bind to be refused, got {other:?}"),
    }

    running.shutdown();

    // And a restart immediately afterwards works — which is the case that `SO_REUSEADDR` exists
    // for: the connections this test just made can hold the port in `TIME_WAIT`.
    let again = server::bind(port, brisk()).expect("the port must be free after shutdown");
    assert_eq!(again.local_addr().unwrap().port(), port);
}

#[test]
fn a_token_deleted_underneath_the_server_comes_back() {
    let scratch = Scratch::new("keepalive");
    let (running, token) = start(&scratch, None);
    let path = scratch.token_path();

    std::fs::remove_file(&path).unwrap();
    wait_until(Duration::from_secs(10), || {
        std::fs::read_to_string(&path)
            .map(|text| text.trim() == token.as_str())
            .unwrap_or(false)
    });

    assert_eq!(
        std::fs::read_to_string(&path).unwrap().trim(),
        token.as_str()
    );
    running.shutdown();
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_whole_home_is_assembled_the_way_the_binary_assembles_it() {
    // The pieces the binary wires together, checked as one: a private home, a config read from
    // it, a token written into it, and a server that refuses a request naming another host.
    let scratch = Scratch::new("binary-shape");
    deepnest::prepare_home(&scratch.base).unwrap();
    std::fs::write(
        scratch.base.join("config.toml"),
        "[llm]\nbase_url = \"https://api.deepseek.com\"\napi_key = \"sk-test\"\nmodel = \"deepseek-flash\"\n",
    )
    .unwrap();

    let config = deepnest::config::load(&scratch.base.join("config.toml"))
        .unwrap()
        .expect("a config that parses");
    let token = token::ensure(&scratch.token_path()).unwrap();

    let running = server::start(ServerOptions {
        port: 0,
        token: Some(token),
        token_path: Some(scratch.token_path()),
        model: Some(config.model().to_string()),
        apps_dir: Some(scratch.apps()),
        lifecycle: None,
        config_path: Some(scratch.config_path()),
        keepalive: Duration::from_secs(30),
        bind: brisk(),
    })
    .unwrap();

    let panel = get(running.addr(), "/", "");
    assert!(panel.contains(config.model()), "{panel}");
    running.shutdown();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(scratch.token_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the token file is the one thing that keeps a web page out of the API"
        );
    }
}

#[test]
fn the_first_run_form_writes_the_config_over_a_real_socket() {
    // The whole path a first-run form takes: socket, head, body, file, and the running process
    // reading it back. The body exists because of what it carries — a key in a URL would be written
    // into the browser's history and into whatever logs the URL.
    let scratch = Scratch::new("config-post");
    let (running, token) = start(&scratch, None);
    let addr = running.addr();
    assert!(!scratch.config_path().exists(), "first run has no config");

    let body = r#"{"base_url":"https://api.deepseek.com","api_key":"sk-from-the-form","model":"deepseek-flash"}"#;
    let answer = request(
        addr,
        &format!(
            "POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            addr.port(),
            token.as_str(),
            body.len(),
            body
        ),
    );
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(
        !answer.contains("sk-from-the-form"),
        "the key was echoed back: {answer}"
    );

    let saved = deepnest::config::load(&scratch.config_path())
        .unwrap()
        .expect("the config was written");
    assert_eq!(saved.llm.model, "deepseek-flash");
    assert_eq!(saved.llm.api_key, "sk-from-the-form");

    // No restart: the process that took the form is the process using the answer.
    let health = get(addr, "/api/health", "");
    assert!(
        health.contains("\"model_endpoint_configured\":true"),
        "{health}"
    );

    running.shutdown();
}

#[test]
fn a_body_past_the_bound_is_refused_before_it_is_buffered() {
    // The bound is what makes reading a body safe at all, and the status is how the refusal travels.
    let scratch = Scratch::new("body-limit");
    let (running, token) = start(&scratch, None);
    let addr = running.addr();

    let answer = request(
        addr,
        &format!(
            "POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            addr.port(),
            token.as_str(),
            deepnest::http::MAX_BODY + 1
        ),
    );
    assert!(answer.starts_with("HTTP/1.1 413"), "{answer}");
    assert!(!scratch.config_path().exists());

    // And a chunked body is refused by name: it has no length to bound it by until it has all
    // arrived, so this layer does not read it at all.
    let chunked = request(
        addr,
        &format!(
            "POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
             Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            addr.port(),
            token.as_str()
        ),
    );
    assert!(chunked.starts_with("HTTP/1.1 411"), "{chunked}");

    running.shutdown();
}

#[test]
fn a_message_carrying_a_screenshot_sized_picture_is_read_rather_than_refused() {
    // The other half of the bound, and the half that was missing: a body the chat route says it
    // takes has to be a body this server reads. A screenshot pasted into the dialog is about a
    // megabyte of `data:` URL, and it used to be answered `413` — the route's own allowance
    // (8,000,000 characters of pictures per message) sat above a body bound of 16 KiB, so the
    // picture feature worked for pictures too small for anyone to paste.
    let scratch = Scratch::new("picture-body");
    let (running, token) = start(&scratch, Some("deepseek-flash"));
    let addr = running.addr();

    let picture = format!("data:image/png;base64,{}", "A".repeat(1_000_000));
    let body = format!("{{\"message\": \"what is this?\", \"images\": [\"{picture}\"]}}");
    let answer = request(
        addr,
        &format!(
            "POST /api/apps/no-such-app/chat HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Authorization: Bearer {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            addr.port(),
            token.as_str(),
            body.len()
        ),
    );

    // A route was reached, which is the whole claim: `read` either has the body whole or refuses the
    // request outright (`413` past the bound, `400` when the declared bytes never all arrive), so
    // the router's own answer is proof that a megabyte of picture was read and handed on. This
    // harness starts the server without a lifecycle, so what the router says about an app is that
    // it cannot serve one — its answer, about the body it was asked with, and not the transport's.
    assert!(!answer.starts_with("HTTP/1.1 413"), "{answer}");
    assert!(answer.starts_with("HTTP/1.1 503"), "{answer}");
    assert!(answer.contains("started without a lifecycle"), "{answer}");

    running.shutdown();
}
