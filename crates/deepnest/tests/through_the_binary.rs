//! The binary, a real app process, and the door between them — the same run on all three
//! platforms.
//!
//! `serve.rs` starts the server inside this test process. This file starts the *program*, because
//! the part that has never run in a test is the wiring: `main` reads `$DEEPNEST_HOME`, the
//! supervisor allocates a port and launches `<app>/target/release/app`, and the gateway carries a
//! page, a stream and an upgrade between that child process and a browser. Substituting a stand-in
//! supervisor to test it would leave exactly the untested part untested.
//!
//! The app is `examples/standin_app.rs`, built by cargo as an example and copied into the app
//! directory under the name the app contract fixes. Nothing here needs a package registry, a
//! bundler or a browser: it is the binary, a real child process, and raw bytes on loopback.
//!
//! **The port is 4270 and cannot be anything else.** It is the one address the design fixes — the
//! user's bookmark, the README, and the single-instance rule all name it — so unlike every other
//! test in this workspace this one cannot let the OS choose. It therefore *fails* rather than
//! skips when something else already holds the port, with a message that says which: a test that
//! quietly skips is a test that reports green having checked nothing, which is the failure this
//! whole file exists to prevent.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sha1::{Digest, Sha1};

/// The port the design fixes, and the only one the binary listens on.
const PORT: u16 = 4270;

/// The RFC 6455 handshake constant, so this side can compute the accept a browser would demand.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// How long the binary gets to come up, and the app to answer after being started.
const PATIENCE: Duration = Duration::from_secs(30);

/// An installed deepnest in a temporary directory: a home, an app, and the child serving it.
struct Install {
    home: PathBuf,
    child: Option<Child>,
}

impl Install {
    /// A home with one app in it, ready for the binary to start.
    fn new() -> Self {
        // `temp_dir`, never a literal `/tmp`: on macOS that is a symlink into `/private`, and on
        // Windows there is no `/tmp` at all.
        let home = std::env::temp_dir().join(format!("deepnest-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create the temporary deepnest home");

        let install = Self { home, child: None };
        install.install_app();
        install
    }

    /// Write the app directory the contract describes, with the stand-in as its built binary.
    fn install_app(&self) {
        let app = self.home.join("apps").join("notes");
        let binary =
            app.join("target")
                .join("release")
                .join(if cfg!(windows) { "app.exe" } else { "app" });
        std::fs::create_dir_all(binary.parent().unwrap()).expect("create the app's release dir");

        std::fs::write(
            app.join("deepnest.toml"),
            "[app]\nname = \"Notes\"\ntemplate = 1\n",
        )
        .expect("write the app manifest");

        // A copy rather than a symlink: a link needs privileges on Windows and is resolved to the
        // example's own path, which the app contract would then see instead of its own binary.
        std::fs::copy(standin_app(), &binary).expect("copy the stand-in app into place");
    }

    /// Start the binary against this home, and wait until it answers.
    fn start(&mut self) {
        let log = self.home.join("deepnest.log");
        let output = std::fs::File::create(&log).expect("create the server log");
        let errors = output.try_clone().expect("clone the server log handle");

        let child = Command::new(env!("CARGO_BIN_EXE_deepnest"))
            // Without this the binary would hand the panel to the desktop and open a browser window
            // on the machine running the suite — three of them at once in CI, none of them wanted.
            .arg("--no-open")
            .env("DEEPNEST_HOME", &self.home)
            .stdin(Stdio::null())
            // To a file, never to a pipe: an undrained pipe fills and blocks the child, and that
            // failure looks like "the server never started".
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("spawn the deepnest binary this test was built with");
        self.child = Some(child);

        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(ours) = self.child.as_mut() {
                if let Ok(Some(status)) = ours.try_wait() {
                    panic!(
                        "the binary exited ({status}) before serving. Its output:\n{}{}",
                        self.read_log(),
                        TAKEN
                    );
                }
            }
            if let Some(answer) = self.try_get("/api/health") {
                if answer.starts_with("HTTP/1.1 200") {
                    return;
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "the binary did not answer on 127.0.0.1:{PORT} within {PATIENCE:?}. \
                     Its output:\n{}",
                    self.read_log()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn read_log(&self) -> String {
        std::fs::read_to_string(self.home.join("deepnest.log")).unwrap_or_default()
    }

    /// The token the binary wrote, which is what the routes that are not pages require.
    fn token(&self) -> String {
        std::fs::read_to_string(self.home.join("deepnest.token"))
            .expect("read the token the binary wrote")
            .trim()
            .to_string()
    }

    /// A request and its whole answer, `None` while nothing is listening yet.
    fn try_get(&self, target: &str) -> Option<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", PORT)).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(
                format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n\r\n").as_bytes(),
            )
            .ok()?;
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).ok()?;
        Some(String::from_utf8_lossy(&answer).into_owned())
    }

    fn get(&self, target: &str) -> String {
        self.try_get(target)
            .unwrap_or_else(|| format!("GET {target} could not reach 127.0.0.1:{PORT}"))
    }

    /// A request with the token, for the routes that are not pages.
    fn call(&self, method: &str, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", PORT)).expect("connect to the binary");
        stream
            .set_read_timeout(Some(Duration::from_secs(PATIENCE.as_secs())))
            .unwrap();
        stream
            .write_all(
                format!(
                    "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n\
                     Authorization: Bearer {}\r\n\r\n",
                    self.token()
                )
                .as_bytes(),
            )
            .unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).unwrap();
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// Start the app through the API, as the panel does, and insist it worked.
    fn start_app(&self) {
        let answer = self.call("POST", "/api/apps/notes/start");
        assert!(
            answer.starts_with("HTTP/1.1 200"),
            "starting the app failed: {answer}{}",
            self.read_log()
        );
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        // The app first, through the API, because killing the server is precisely the case where a
        // child outlives its supervisor — killing it here would hide that, and stopping it through
        // the door is also a check in its own right.
        if self.child.is_some() {
            let _ = self.call("POST", "/api/apps/notes/stop");
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Said when the binary refuses to start, so a taken port is never read as a code defect.
const TAKEN: &str =
    "\n\nIf that says \"deepnest is already running\", this test needs 127.0.0.1:4270 \
free — the design fixes that port, so a running deepnest on this machine holds it. Stop it and \
run the suite again; nothing is wrong with the code.\n";

/// The stand-in app, built as an example beside this test binary.
fn standin_app() -> PathBuf {
    let test_binary = std::env::current_exe().expect("the path of this test binary");
    let debug_dir = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("the test binary lives in target/<profile>/deps");
    let name = if cfg!(windows) {
        "standin_app.exe"
    } else {
        "standin_app"
    };
    let path = debug_dir.join("examples").join(name);
    assert!(
        path.is_file(),
        "the stand-in app is not at {} — `cargo test` builds examples, so this means the test was \
         run from a stale target directory; `cargo build -p deepnest --examples` fixes it",
        path.display()
    );
    path
}

/// Connect with timeouts, so a hang is a failure rather than a hung suite.
fn connect() -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", PORT)).expect("connect to the binary");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
}

fn handshake(target: &str) -> String {
    format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n\r\n")
}

/// The one scenario, in order: page, answer, stream, upgrade, the door staying open throughout, and
/// a second start finding this one already there.
#[test]
fn an_app_is_served_through_the_door_the_way_a_browser_sees_it() {
    let mut install = Install::new();
    install.start();
    install.start_app();

    // 1. A page, with the base the door adds so relative URLs resolve under the prefix.
    let page = install.get("/app/notes/");
    assert!(
        page.starts_with("HTTP/1.1 200"),
        "the app's page was not served: {page}"
    );
    assert!(
        page.contains("<base href=\"/app/notes/\">"),
        "the page arrived without the base the door injects: {page}"
    );

    // 2. An ordinary answer, with the app's own content type and bytes.
    let items = install.get("/app/notes/api/items");
    assert!(
        items.contains("{\"items\":[1]}"),
        "the app's JSON did not cross: {items}"
    );
    assert!(
        items
            .to_ascii_lowercase()
            .contains("content-type: application/json"),
        "the app's content type did not cross: {items}"
    );

    // 3. A stream: answered, framed as the app framed it, complete, and *not* held to the end.
    let mut stream = connect();
    stream
        .write_all(handshake("/app/notes/events").as_bytes())
        .unwrap();
    let started = Instant::now();
    let mut seen = Vec::new();
    let mut first_event = None;
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream
            .read(&mut chunk)
            .expect("read the event stream the door is carrying");
        if read == 0 {
            break;
        }
        seen.extend_from_slice(&chunk[..read]);
        if first_event.is_none() && contains(&seen, b"data:0") {
            first_event = Some(started.elapsed());
        }
    }
    let ended = started.elapsed();
    drop(stream);

    let text = String::from_utf8_lossy(&seen).into_owned();
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "the stream was not answered: {text}"
    );
    assert!(
        text.to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "the stream lost the app's framing: {text}"
    );
    assert!(
        contains(&seen, b"data:0") && contains(&seen, b"data:2"),
        "the stream did not carry every event: {text}"
    );
    let first = first_event.expect("the first event never arrived");
    assert!(
        first + Duration::from_millis(500) < ended,
        "the first event was held until the stream ended (first {first:?}, end {ended:?}) — \
         something between the app and the browser is buffering the body"
    );

    // 4. An upgrade: accepted, with the accept key the app itself computed, then bytes both ways.
    let mut socket = connect();
    let key = BASE64.encode(b"deepnest-e2e-1234");
    socket
        .write_all(
            format!(
                "GET /app/notes/ws HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: {key}\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();

    let mut upgrade = Vec::new();
    let mut byte = [0u8; 1];
    while !upgrade.ends_with(b"\r\n\r\n") {
        let read = socket.read(&mut byte).expect("read the upgrade head");
        assert!(read > 0, "the door closed the upgrade before its head");
        upgrade.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&upgrade).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "the upgrade was not accepted: {head}"
    );
    let mut digest = Sha1::new();
    digest.update(key.as_bytes());
    digest.update(GUID.as_bytes());
    let accept = BASE64.encode(digest.finalize());
    assert!(
        head.to_ascii_lowercase()
            .contains(&accept.to_ascii_lowercase()),
        "the app's own accept key did not cross unchanged (expected {accept}): {head}"
    );

    socket.write_all(b"frame-one").unwrap();
    let mut echoed = [0u8; 9];
    socket
        .read_exact(&mut echoed)
        .expect("read the echo the tunnel should carry back");
    assert_eq!(
        &echoed, b"frame-one",
        "the tunnel did not carry bytes both ways"
    );

    // 5. With that tunnel open and idle, the door still answers other connections: a connection
    //    that outlives its request must not become a connection that owns the server.
    let health = install.get("/api/health");
    assert!(
        health.starts_with("HTTP/1.1 200"),
        "the server stopped answering while a tunnel was open: {health}"
    );
    let _ = socket.shutdown(Shutdown::Both);

    // 6. A method this door cannot carry is refused by name rather than mis-forwarded.
    let mut refused = TcpStream::connect(("127.0.0.1", PORT)).unwrap();
    refused
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    refused
        .write_all(
            format!(
                "POST /app/notes/save HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n\
                 Content-Length: 0\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut answer = Vec::new();
    refused.read_to_end(&mut answer).unwrap();
    let answer = String::from_utf8_lossy(&answer).into_owned();
    assert!(
        answer.starts_with("HTTP/1.1 405"),
        "a POST to an app was not refused: {answer}"
    );
    assert!(
        answer.contains("Allow: GET, HEAD"),
        "the refusal did not say what is allowed: {answer}"
    );

    // 7. The build route the dialog polls, as far as a door with no model can be asked it: what it
    //    says before anything asked for a build, and what a build that cannot run does to the app.
    //    The rebuild a *writing turn* starts is past this test's reach — a turn needs a model, and
    //    this install has none — so what is checked here is that the route is wired through the
    //    door at all, and that a failed build is a report rather than a dead app.
    let before = install.call("GET", "/api/apps/notes/build");
    assert!(
        before.starts_with("HTTP/1.1 200"),
        "the build-status route did not answer: {before}"
    );
    assert!(
        before.contains("\"state\":\"idle\""),
        "a status route that has not seen a build must say so: {before}"
    );
    let ran = install.call("POST", "/api/apps/notes/build");
    assert!(
        !ran.starts_with("HTTP/1.1 200"),
        "an app with no front end and no Cargo.toml cannot have built: {ran}"
    );
    // The log is what the person is shown when a build fails, so it has to be there.
    let build_log = install.home.join("logs").join("notes.build.log");
    assert!(
        build_log.is_file(),
        "the failed build left no log at {}",
        build_log.display()
    );
    assert!(
        install.get("/app/notes/").starts_with("HTTP/1.1 200"),
        "a failed build took the app down with it"
    );

    // 8. A second start leaves this one alone: it says where the first one is and exits. This is
    //    also the branch that hands that address to the browser — a second double-click is a person
    //    asking for the page that is already being served — which is why it must not disturb it.
    //    `--no-open` keeps the assertion to what can be seen here: the address it prints.
    let second = Command::new(env!("CARGO_BIN_EXE_deepnest"))
        .arg("--no-open")
        .env("DEEPNEST_HOME", &install.home)
        .stdin(Stdio::null())
        .output()
        .expect("run a second deepnest against the home the first one is using");
    assert!(
        second.status.success(),
        "a second start is not an error: {:?}",
        second.status
    );
    let said = String::from_utf8_lossy(&second.stdout);
    assert!(
        said.contains("already running"),
        "the second start did not say where the first one is: {said}"
    );
    assert!(
        said.contains(&format!("http://127.0.0.1:{PORT}/")),
        "the second start did not give the address to open: {said}"
    );
    assert!(
        install.get("/api/health").starts_with("HTTP/1.1 200"),
        "the second start disturbed the first one"
    );
}

/// `Vec<u8>::windows` will not take a needle of a different type, and the needle here is a byte
/// slice rather than an array.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
