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
    // The panel says what it is not. A placeholder that looked finished would be worse.
    assert!(panel.contains("crates/web"));
    assert!(panel.contains("Content-Security-Policy"));

    let health = get(addr, "/api/health", "");
    assert!(health.starts_with("HTTP/1.1 200 OK"), "{health}");
    assert!(health.contains("\"status\":\"ok\""));
    assert!(health.contains("Content-Type: application/json"));

    running.shutdown();
}

#[test]
fn the_token_gate_is_enforced_end_to_end() {
    let scratch = Scratch::new("auth");
    let (running, token) = start(&scratch, None);
    let addr = running.addr();

    let refused = get(addr, "/api/apps", "");
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");

    let accepted = get(
        addr,
        "/api/apps",
        &format!("Authorization: Bearer {token}\r\n"),
    );
    // 404 because `/api/apps` does not exist yet — the point is that it is no longer 401.
    assert!(accepted.starts_with("HTTP/1.1 404"), "{accepted}");

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
fn a_post_is_refused_with_a_method_not_allowed() {
    let scratch = Scratch::new("method");
    let (running, _token) = start(&scratch, None);
    let addr = running.addr();

    let answer = request(
        addr,
        &format!("POST / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", addr.port()),
    );
    assert!(answer.starts_with("HTTP/1.1 405"), "{answer}");
    assert!(answer.contains("Allow: GET, HEAD"));

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
