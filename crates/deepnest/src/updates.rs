//! The socket under the release check's seam, and the answer the panel's script reads.
//!
//! `crates/skills::release` decides everything about a newer release — which tags are versions, what
//! each answer means, what a refresh would adopt — and `Releases` is where it stops: one method,
//! `owner/name` in and the answer's body out. What it could not do is speak, because that crate has
//! no transport and, per its own module doc, should not grow one. `crates/gateway`'s `get` landed
//! after it, so the socket exists; this module is the wire between them, and it lives here because
//! this is the one crate that depends on both.
//!
//! **Three things this module is careful about.**
//!
//! - **The endpoint is a parameter, not a literal.** `release_url` stays the default, because the
//!   URL is part of the contract and has its own shape test; but a check that could only ever reach
//!   `api.github.com` is a check no test can drive, and the honest alternatives are a fake seam that
//!   proves nothing about the transport or a test that measures GitHub. `Github::at` is the same
//!   decision `TlsHttp::with_connector` made: open the one thing a test has to control.
//! - **A non-`200` is an error, never an empty body.** An empty body would come back from
//!   `release::check` as `NotARelease` — true but vague — while a `404` is a real answer with a real
//!   meaning (the repository publishes no releases). Reporting it as an error puts it in `Why`'s
//!   `Unreachable`, where the detail is the status and the user can read it.
//! - **Nothing here runs while a page is being rendered.** [`report`] is called from a route the
//!   panel's script fetches on its own, never from `Router::skills_report`, which reads the disk and
//!   renders synchronously: one TLS request per skill inside that would make an offline machine
//!   hang the whole panel, and "local-first" is the product.

use std::io;
use std::path::Path;
use std::sync::Arc;

use deepnest_gateway::HttpTransport;
use deepnest_skills::release::{release_url, Releases, Verdict, Why};
use serde_json::{json, Value};

/// What this build calls itself to the forge.
///
/// Not decoration: `api.github.com` refuses a request without a `User-Agent`, so omitting it would
/// make every check fail for a reason that has nothing to do with the question being asked.
pub const USER_AGENT: &str = concat!("deepnest/", env!("CARGO_PKG_VERSION"));

/// The `Accept` value the API fixes for a release read, so a future default cannot silently move it.
const ACCEPT: &str = "application/vnd.github+json";

/// Where a repository's latest release is asked for: `owner/name` in, URL out.
///
/// A closure rather than a string because the interesting part is not the default — it is that a
/// test can replace it, and a trait object is the smallest way to say so.
pub type Endpoint = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// [`Releases`] over the same transport a model call goes through.
pub struct Github {
    transport: HttpTransport,
    endpoint: Endpoint,
}

impl Github {
    /// The real check: `release_url`, over the real transport.
    pub fn new() -> Self {
        Self::at(Arc::new(release_url))
    }

    /// The same check against an endpoint the caller names.
    pub fn at(endpoint: Endpoint) -> Self {
        Self {
            transport: HttpTransport::new(),
            endpoint,
        }
    }

    /// The URL this check would ask, for a report that says where the answer came from.
    fn url(&self, repo: &str) -> String {
        (self.endpoint)(repo)
    }
}

impl Default for Github {
    fn default() -> Self {
        Self::new()
    }
}

impl Releases for Github {
    fn latest(&self, repo: &str) -> io::Result<String> {
        let headers = vec![
            ("User-Agent".to_string(), USER_AGENT.to_string()),
            ("Accept".to_string(), ACCEPT.to_string()),
        ];
        let answer = self
            .transport
            .get(&self.url(repo), &headers)
            .map_err(|error| io::Error::other(error.to_string()))?;
        // `200` is the one status that carries a release. Anything else is the forge answering a
        // different question — a `404` for a repository that publishes nothing, a `403` for a rate
        // limit — and each is more useful named than parsed as a body.
        if answer.status != 200 {
            return Err(io::Error::other(format!(
                "the forge answered {}, not 200, for {}",
                answer.status,
                self.url(repo)
            )));
        }
        Ok(answer.body)
    }
}

/// One skill's answer, as the route returns it and the panel prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The skill's name, as the loader spells it.
    pub name: String,
    /// The repository the check asked about.
    pub repo: String,
    pub verdict: Verdict,
}

impl Row {
    /// The word the page switches on.
    ///
    /// Four states rather than two, because [`Verdict`] went to the trouble: a page that showed only
    /// "update available" would have to print nothing for `CannotTell`, and a blank where a check
    /// failed reads as "up to date".
    fn state(&self) -> &'static str {
        match self.verdict {
            Verdict::Newer { .. } => "newer",
            Verdict::Unknown { .. } => "unknown",
            Verdict::UpToDate { .. } => "current",
            Verdict::CannotTell(_) => "unchecked",
        }
    }

    fn body(&self) -> Value {
        let version = match &self.verdict {
            Verdict::Newer { version }
            | Verdict::Unknown { latest: version }
            | Verdict::UpToDate { version } => Some(version.to_string()),
            Verdict::CannotTell(_) => None,
        };
        json!({
            "name": self.name,
            "repo": self.repo,
            "state": self.state(),
            "version": version,
            // Always present, and `null` when there is nothing wrong: the panel reads one field
            // and never has to decide whether a missing key means "fine" or "not asked".
            "detail": match &self.verdict {
                Verdict::CannotTell(why) => Some(why_text(why)),
                _ => None,
            },
        })
    }
}

/// What a `CannotTell` says, in the person's terms rather than as a variant name.
///
/// [`Why`] carries the next step in each variant — a catalog to fix, a state file to look at, a
/// network to restore — so the sentence names that rather than reprinting a debug shape.
fn why_text(why: &Why) -> String {
    match why {
        Why::NotChecked => "the catalog names no check for it".to_string(),
        Why::UnknownCheck { kind } => format!("this build does not know the `{kind}` check"),
        Why::NoRepo => "the catalog names no repository".to_string(),
        Why::NotManaged { .. } => {
            "deepnest did not install this one, so it does not offer an update".to_string()
        }
        Why::Unreachable { detail } => format!("the forge could not be reached: {detail}"),
        Why::NotARelease { detail } => format!("the answer was not a release: {detail}"),
        Why::TagNotAVersion { tag } => format!("`{tag}` is not a version this build orders"),
        Why::InstalledNotAVersion { version } => {
            format!("the recorded version `{version}` is not one this build orders")
        }
    }
}

/// What the update check finds for the skills under `home`, one [`Row`] each.
///
/// Only skills that are **both** loaded and described by a catalog entry carrying a check are asked
/// about: a check for a skill that is not on disk answers a question nobody has, and the loader is
/// what decides "on disk". A skill the user wrote themselves has no catalog entry, so it is not here
/// — and that is a reading of the state file, not a guess about intent.
///
/// The verdicts are `crates/skills`' to reach, not this module's: every way an answer can be wrong
/// is decided there, and this function only supplies the two facts it cannot read.
pub fn report(home: &Path, releases: &dyn Releases) -> Vec<Row> {
    let loaded = deepnest_skills::load(home, None);
    let catalog = deepnest_skills::catalog::load(&deepnest_skills::user_root(home));
    let state = deepnest_skills::State::read(&deepnest_skills::state::path(
        &deepnest_skills::user_root(home),
    ));

    let mut rows = Vec::new();
    for skill in &loaded.skills {
        let Some(entry) = catalog.get(&skill.name) else {
            continue;
        };
        if entry.check.is_none() {
            continue;
        }
        let installed = state.get(&skill.name);
        rows.push(Row {
            name: skill.name.clone(),
            repo: entry.repo.clone().unwrap_or_default(),
            verdict: deepnest_skills::check(entry, installed, releases),
        });
    }
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    rows
}

/// The route's body: every row, and a count of the ones that need saying out loud.
///
/// `newer` and `unchecked` are separate counts because they are separate next steps — refresh, or
/// find out why the question went unanswered — and one number that added them would tell a person
/// to look without saying at what.
pub fn body(rows: &[Row]) -> Value {
    let newer = rows.iter().filter(|row| row.state() == "newer").count();
    let unchecked = rows.iter().filter(|row| row.state() == "unchecked").count();
    json!({
        "checked": rows.len(),
        "newer": newer,
        "unchecked": unchecked,
        "skills": rows.iter().map(Row::body).collect::<Vec<Value>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A one-shot HTTP server: it answers `response` to whatever arrives and hands the request back.
    ///
    /// A real socket rather than a fake transport, because what this module adds to
    /// `crates/skills` is exactly the part a fake would replace: the request as it goes out and the
    /// body as it comes back. The URL is `http`, so the plain half of the transport carries it and
    /// no certificate is needed to test the wiring — the TLS half already has its own tests.
    fn serve(response: &'static [u8]) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut seen = Vec::new();
                let mut buf = [0u8; 1024];
                // Read to the blank line that ends the head: a `GET` promises no body, so there is
                // nothing after it to wait for and a second read would block until the timeout.
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            seen.extend_from_slice(&buf[..read]);
                            if seen.windows(4).any(|window| window == b"\r\n\r\n") {
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
        (format!("http://127.0.0.1:{port}"), rx)
    }

    fn release_json(tag: &str) -> String {
        format!("{{\"tag_name\":\"{tag}\",\"name\":\"v{tag}\",\"prerelease\":false}}")
    }

    fn answered(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// A check pointed at a named base, as a test named the loopback server.
    fn at(base: &str) -> Github {
        let base = base.to_string();
        Github::at(Arc::new(move |repo: &str| {
            format!("{base}/repos/{repo}/releases/latest")
        }))
    }

    #[test]
    fn the_request_the_check_sends_carries_the_url_the_forge_requires_and_the_body_comes_back() {
        let body = release_json("v0.4.0");
        let (base, seen) = serve(Box::leak(answered(&body).into_boxed_slice()));
        let releases = at(&base);

        let answer = releases.latest("argszero/browser-harness").unwrap();
        assert_eq!(answer, body, "the body did not come back unchanged");

        let request = seen.recv().unwrap();
        assert!(
            request.starts_with("GET /repos/argszero/browser-harness/releases/latest HTTP/1.1"),
            "the endpoint parameter did not reach the wire: {request}"
        );
        // Without this header the API refuses the request, so a check that omitted it would fail
        // for a reason unrelated to the question — and it would fail the same way on every skill.
        assert!(
            request
                .to_lowercase()
                .contains(&format!("user-agent: {}", USER_AGENT.to_lowercase())),
            "the request named no user agent, which the API requires: {request}"
        );
        assert!(
            request
                .to_lowercase()
                .contains("accept: application/vnd.github+json"),
            "the request did not ask for the release representation: {request}"
        );
    }

    #[test]
    fn the_default_endpoint_is_the_one_the_contract_names() {
        // `release_url` is part of the contract and has its own shape test in `crates/skills`; this
        // asserts the default *uses* it rather than inventing a second spelling.
        assert_eq!(
            Github::new().url("argszero/browser-harness"),
            release_url("argszero/browser-harness")
        );
        assert!(Github::new()
            .url("a/b")
            .ends_with("/repos/a/b/releases/latest"));
    }

    /// A home with the real catalog and one installed skill, so `report` walks the same files the
    /// binary would.
    fn home(name: &str, version: Option<&str>) -> std::path::PathBuf {
        let base =
            std::env::temp_dir().join(format!("deepnest-updates-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = deepnest_skills::user_root(&base);
        std::fs::create_dir_all(root.join("browser-harness")).unwrap();
        std::fs::write(
            root.join("browser-harness")
                .join(deepnest_skills::SKILL_FILE),
            "---\nname: browser-harness\ndescription: drives a browser\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            deepnest_skills::catalog::path(&root),
            deepnest_skills::BUNDLED,
        )
        .unwrap();
        let mut state = deepnest_skills::State::new();
        state.record_install("browser-harness", version.map(str::to_string), "2026-11-01");
        state.write(&deepnest_skills::state::path(&root)).unwrap();
        base
    }

    struct Answers(&'static str);

    impl Releases for Answers {
        fn latest(&self, _repo: &str) -> io::Result<String> {
            Ok(self.0.to_string())
        }
    }

    #[test]
    fn a_recorded_version_older_than_the_release_is_reported_as_newer() {
        let base = home("newer", Some("0.1.0"));
        let rows = report(&base, &Answers(r#"{"tag_name":"v0.4.0"}"#));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].name, "browser-harness");
        assert_eq!(rows[0].state(), "newer");
        assert_eq!(
            rows[0].verdict.adoptable().map(|v| v.to_string()),
            Some("0.4.0".to_string())
        );

        let route = body(&rows);
        assert_eq!(route["newer"], 1, "{route}");
        assert_eq!(route["unchecked"], 0, "{route}");
        assert_eq!(route["skills"][0]["detail"], Value::Null, "{route}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_answer_that_could_not_be_read_is_unchecked_rather_than_up_to_date() {
        let base = home("unchecked", Some("0.1.0"));
        let rows = report(&base, &Answers("<html>a proxy's page</html>"));
        assert_eq!(rows.len(), 1, "{rows:?}");
        // The whole point of the four answers: a body that is not a release must not pass as the
        // reassuring one, because the reassuring one means there is nothing to do.
        assert_eq!(rows[0].state(), "unchecked");
        let route = body(&rows);
        assert_eq!(route["newer"], 0, "{route}");
        assert_eq!(route["unchecked"], 1, "{route}");
        assert!(
            route["skills"][0]["detail"]
                .as_str()
                .unwrap()
                .contains("not a release"),
            "{route}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_skill_the_catalog_does_not_name_is_not_asked_about() {
        // The loader finds it, the catalog says nothing about it, so there is no repository to ask
        // and no release to compare — and a row for it would be a request to nobody.
        let base = home("stranger", Some("1.0.0"));
        let mine = deepnest_skills::user_root(&base).join("my-own");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(
            mine.join("SKILL.md"),
            "---\nname: my-own\ndescription: mine\n---\n\nBody.\n",
        )
        .unwrap();

        let rows = report(&base, &Answers(r#"{"tag_name":"v0.4.0"}"#));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0].name, "browser-harness",
            "a skill outside the catalog was checked"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_status_that_is_not_200_is_unreachable_and_never_an_empty_body() {
        let (base, _seen) = serve(
            b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 21\r\n\r\n{\"message\":\"Not Found\"}",
        );
        let error = at(&base).latest("argszero/notes").unwrap_err();
        let said = error.to_string();
        assert!(said.contains("404"), "{said}");
        assert!(said.contains("releases/latest"), "{said}");
    }
}
