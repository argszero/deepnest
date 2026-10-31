//! Which request gets which answer — and, more importantly, which requests are refused.
//!
//! Two refusals carry the whole security story of a loopback server, and both are here rather
//! than in the socket code because they are policy:
//!
//! - **`Host` must name this server.** A page on `evil.example` can point a name it controls at
//!   `127.0.0.1` (DNS rebinding) and then talk to this port with a same-origin `Host`. The check
//!   is what makes that request a `403` instead of an answer.
//! - **`/api/` needs the token.** Reaching the port proves nothing; holding the token does.
//!   `/api/health` is the one exception, on purpose: it is a liveness probe that carries no user
//!   data, and requiring a probe to read a `0600` file would make the simplest check the hardest.
//!
//! Everything this build does not serve says so, by name. A `404` on `/app/<slug>/` that reads
//! "the app gateway is not part of this build yet" is worth more to the next person than a bare
//! `404`, which looks like a bug in a path they spelled correctly.
//!
//! `/api/apps` is the one route backed by a directory rather than by a value held in this struct:
//! it lists `~/.deepnest/apps` at request time, so an app that appeared a second ago is in the
//! next answer without a restart. What it cannot use it reports rather than drops — see
//! `deepnest_apps::list` — because the panel showing fewer apps than the user created has no other
//! way to say why.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::http::{Method, Request, Response};
use crate::token::Token;

/// The router: what this build can answer.
#[derive(Debug, Clone)]
pub struct Router {
    port: u16,
    /// The model name from `config.toml`, when there was a config to read.
    model: Option<String>,
    /// The control token. `None` only in tests — the binary always has one.
    token: Option<Token>,
    /// `~/.deepnest/apps`, when the caller knows where the home is. `None` reports that the list
    /// is unavailable instead of answering "no apps", which would be a different claim.
    apps_dir: Option<PathBuf>,
}

impl Router {
    /// A router for a server bound on `port`.
    pub fn new(port: u16, model: Option<String>, token: Option<Token>) -> Self {
        Self {
            port,
            model,
            token,
            apps_dir: None,
        }
    }

    /// List apps from `dir`. Without it, `/api/apps` says the directory is unknown.
    pub fn with_apps_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.apps_dir = Some(dir.into());
        self
    }

    /// Answer one request.
    pub fn respond(&self, request: &Request) -> Response {
        if request.method == Method::Head {
            // Nothing below distinguishes HEAD from GET: the body is dropped at write time, and
            // that is the correct semantic. Naming it here documents the intent.
        }

        if let Err(refusal) = self.host_is_ours(request) {
            return refusal;
        }

        let path = request.path.as_str();
        match path {
            "/" | "/index.html" => Response::html(200, self.panel()).header(
                "Content-Security-Policy",
                "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'",
            ),
            "/api/health" => Response::json(200, self.health()),
            "/api/apps" => {
                if !self.authorized(request) {
                    return unauthorized();
                }
                let (status, body) = self.apps();
                Response::json(status, body)
            }
            other if other.starts_with("/api/") => {
                if self.authorized(request) {
                    Response::json(404, error_body("no such endpoint"))
                } else {
                    unauthorized()
                }
            }
            other if other.starts_with("/app/") => Response::json(
                404,
                error_body("the app gateway is not part of this build yet (crates/gateway)"),
            ),
            _ => Response::json(404, error_body("no such path")),
        }
    }

    /// The request's `Host` must be one this server answers to.
    fn host_is_ours(&self, request: &Request) -> Result<(), Response> {
        let Some(host) = request.header("host") else {
            // HTTP/1.1 requires it. A request without one is a client bug or an attempt to make
            // the server guess which origin it is answering.
            return Err(Response::text(400, "a request must carry a Host header\n"));
        };
        if self
            .allowed_hosts()
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
        {
            return Ok(());
        }
        Err(Response::text(
            403,
            format!(
                "this server answers to 127.0.0.1:{} and localhost:{}, not to that Host\n",
                self.port, self.port
            ),
        ))
    }

    fn allowed_hosts(&self) -> [String; 2] {
        [
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
        ]
    }

    /// Does the request carry the control token?
    fn authorized(&self, request: &Request) -> bool {
        let Some(token) = &self.token else {
            // No token configured means no token to check. The binary always configures one;
            // this arm exists so a test can exercise routing without writing a file.
            return true;
        };
        let Some(value) = request.header("authorization") else {
            return false;
        };
        let Some((scheme, credential)) = value.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("bearer") && token.matches(credential)
    }

    /// The placeholder panel.
    ///
    /// It exists to prove the process is up and to say what is configured, and it says plainly
    /// that the real panel is not here yet. A page that looked finished and did nothing would be
    /// worse than one that states the truth.
    fn panel(&self) -> String {
        let state = match &self.model {
            Some(model) => format!(
                "Model endpoint: configured (<code>{}</code>).",
                escape_html(model)
            ),
            None => "Model endpoint: <strong>not configured</strong> — write <code>config.toml</code> in the deepnest home (see <code>deepnest --help</code>).".to_string(),
        };
        PANEL
            .replace("@@PORT@@", &self.port.to_string())
            .replace("@@STATE@@", &state)
            .replace("@@APPS@@", &self.apps_summary())
    }

    /// One line about the apps directory, for the panel.
    ///
    /// It counts what could not be used and points at the route that explains each one: a page that
    /// silently showed fewer apps than exist would leave the user with nothing to act on.
    fn apps_summary(&self) -> String {
        let Some(dir) = &self.apps_dir else {
            return "Apps: <strong>not configured</strong> — this process was started without an apps directory.".to_string();
        };
        let shown = escape_html(&dir.display().to_string());
        match deepnest_apps::list(dir) {
            Ok(listing) if listing.is_empty() && listing.skipped.is_empty() => {
                format!("Apps: <strong>none yet</strong> in <code>{shown}</code>.")
            }
            Ok(listing) => {
                let mut line = format!(
                    "Apps: <strong>{}</strong> in <code>{shown}</code>.",
                    listing.apps.len()
                );
                if !listing.skipped.is_empty() {
                    let _ = write!(
                        line,
                        " {} entries there are not apps — <code>GET /api/apps</code> says why.",
                        listing.skipped.len()
                    );
                }
                line
            }
            Err(error) => format!(
                "Apps: <strong>cannot be listed</strong> — <code>{}</code>: {}.",
                shown,
                escape_html(&error.to_string())
            ),
        }
    }

    /// The app listing as JSON, and the status that goes with it.
    fn apps(&self) -> (u16, String) {
        let Some(dir) = &self.apps_dir else {
            return (
                200,
                json!({
                    "apps_directory": Value::Null,
                    "apps": [],
                    "skipped": [],
                    "note": "this process was started without an apps directory",
                })
                .to_string(),
            );
        };
        let shown = dir.display().to_string();

        match deepnest_apps::list(dir) {
            Ok(listing) => (
                200,
                json!({
                    "apps_directory": shown,
                    "apps": listing
                        .apps
                        .iter()
                        .map(|app| json!({
                            "slug": app.slug.as_str(),
                            "name": app.display_name(),
                            "template": app.manifest.app.template,
                            "path": app.dir.display().to_string(),
                        }))
                        .collect::<Vec<_>>(),
                    "skipped": listing
                        .skipped
                        .iter()
                        .map(|skipped| json!({
                            "path": skipped.path.display().to_string(),
                            "reason": skipped.reason,
                        }))
                        .collect::<Vec<_>>(),
                })
                .to_string(),
            ),
            // A listing that could not be taken is not an empty listing, and saying "no apps"
            // would send the user looking for the wrong fault.
            Err(error) => (
                500,
                json!({
                    "apps_directory": shown,
                    "error": format!("cannot list {shown}: {error}"),
                })
                .to_string(),
            ),
        }
    }

    fn health(&self) -> String {
        json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "model_endpoint_configured": self.model.is_some(),
            "pid": std::process::id(),
        })
        .to_string()
    }
}

const PANEL: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>deepnest</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 16px/1.55 ui-sans-serif, system-ui, -apple-system, sans-serif; margin: 0; }
  main { max-width: 40rem; margin: 12vh auto 0; padding: 0 1.5rem; }
  h1 { font-size: 1.6rem; margin: 0 0 .25rem; letter-spacing: -.02em; }
  .sub { margin: 0 0 1.75rem; opacity: .65; font-size: .9rem; }
  p { margin: 0 0 .85rem; }
  code { font: .875em ui-monospace, SFMono-Regular, Menlo, monospace; background: rgba(127,127,127,.14); padding: .1em .35em; border-radius: 4px; }
  .todo { margin-top: 1.75rem; padding-top: 1rem; border-top: 1px solid rgba(127,127,127,.3); font-size: .9rem; opacity: .8; }
</style>
</head>
<body>
<main>
  <h1>deepnest</h1>
  <p class="sub">running on <code>127.0.0.1:@@PORT@@</code></p>
  <p>@@STATE@@</p>
  <p>@@APPS@@</p>
  <div class="todo">
    <p>This is the process, not the product. The control panel arrives with <code>crates/web</code>,
    and app pages at <code>/app/&lt;slug&gt;/</code> with <code>crates/gateway</code>.</p>
    <p>Liveness: <code>GET /api/health</code>. Apps: <code>GET /api/apps</code>.</p>
  </div>
</main>
</body>
</html>
"#;

fn error_body(message: &str) -> String {
    json!({ "error": message }).to_string()
}

/// The one answer for a `/api/` request that does not carry the token.
fn unauthorized() -> Response {
    Response::json(
        401,
        error_body("this endpoint needs the control token — it is in the deepnest home's deepnest.token file"),
    )
    .header("WWW-Authenticate", "Bearer realm=\"deepnest\"")
}

/// Escape the five characters that would otherwise end the text they sit in.
///
/// The model name comes from the user's own config, so this is not a defence against a hostile
/// value — it is the difference between a page that renders a name containing `<` and a page
/// that silently loses half of it.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::parse;

    const PORT: u16 = 4270;

    fn router() -> Router {
        Router::new(
            PORT,
            Some("deepseek-flash".to_string()),
            Some(Token::parse(&"a".repeat(64)).unwrap()),
        )
    }

    fn get(path: &str, host: &str) -> Request {
        parse(&format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n")).unwrap()
    }

    fn body(response: &Response) -> String {
        String::from_utf8(response.body.clone()).unwrap()
    }

    #[test]
    fn the_root_serves_the_panel_on_both_spellings_of_loopback() {
        let router = router();
        for host in ["127.0.0.1:4270", "localhost:4270", "LOCALHOST:4270"] {
            let response = router.respond(&get("/", host));
            assert_eq!(response.status, 200, "Host: {host}");
            assert!(body(&response).contains("127.0.0.1:4270"));
        }
    }

    #[test]
    fn a_foreign_host_is_refused() {
        // DNS rebinding: the name resolves to loopback, so the port is reachable; the Host
        // header is the one thing the attacking page cannot make truthful.
        let response = router().respond(&get("/", "evil.example"));
        assert_eq!(response.status, 403);
    }

    #[test]
    fn a_request_without_host_is_refused() {
        let request = parse("GET / HTTP/1.1\r\n").unwrap();
        assert_eq!(router().respond(&request).status, 400);
    }

    #[test]
    fn health_answers_without_the_token() {
        // A probe should not have to read a 0600 file, and this route carries no user data.
        let response = router().respond(&get("/api/health", "127.0.0.1:4270"));
        assert_eq!(response.status, 200);
        let body = body(&response);
        assert!(body.contains("\"status\":\"ok\""), "{body}");
        assert!(
            body.contains("\"model_endpoint_configured\":true"),
            "{body}"
        );
        assert!(
            !body.contains("deepseek-flash"),
            "health names no model: {body}"
        );
    }

    #[test]
    fn an_api_route_without_the_token_is_unauthorized() {
        let response = router().respond(&get("/api/apps", "127.0.0.1:4270"));
        assert_eq!(response.status, 401);
        assert!(response
            .headers
            .iter()
            .any(|(name, _)| *name == "WWW-Authenticate"));
    }

    #[test]
    fn an_api_route_with_the_wrong_token_is_unauthorized() {
        let request = parse(&format!(
            "GET /api/apps HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nAuthorization: Bearer {}\r\n",
            "b".repeat(64)
        ))
        .unwrap();
        assert_eq!(router().respond(&request).status, 401);
    }

    #[test]
    fn an_api_route_with_the_token_reaches_the_router_and_finds_nothing() {
        let request = parse(&format!(
            "GET /api/nothing-here HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\n",
            "a".repeat(64)
        ))
        .unwrap();
        // 404, not 401: the token was accepted, and this endpoint genuinely does not exist.
        assert_eq!(router().respond(&request).status, 404);
    }

    /// A request that carries the token.
    fn authorized_get(path: &str) -> Request {
        parse(&format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\n",
            "a".repeat(64)
        ))
        .unwrap()
    }

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-router-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn apps(&self) -> PathBuf {
            self.base.join("apps")
        }

        fn app(&self, slug: &str, name: &str) {
            deepnest_apps::Manifest::new(name)
                .save(&self.apps().join(slug))
                .unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn the_app_list_needs_the_token_like_every_other_api_route() {
        let scratch = Scratch::new("apps-token");
        scratch.app("notes", "Notes");
        let router = router().with_apps_dir(scratch.apps());

        // The listing names the user's apps and the paths they live at; reaching the port is not
        // enough to read that.
        assert_eq!(
            router.respond(&get("/api/apps", "127.0.0.1:4270")).status,
            401
        );
        assert_eq!(router.respond(&authorized_get("/api/apps")).status, 200);
    }

    #[test]
    fn the_app_list_names_what_is_there() {
        let scratch = Scratch::new("apps-list");
        scratch.app("notes", "Notes");
        scratch.app("budget", "Budget");
        let router = router().with_apps_dir(scratch.apps());

        let response = router.respond(&authorized_get("/api/apps"));
        assert_eq!(response.status, 200);
        let body = body(&response);
        assert!(body.contains("\"slug\":\"notes\""), "{body}");
        assert!(body.contains("\"name\":\"Budget\""), "{body}");
        // Sorted, so a panel does not reorder itself between two reads.
        assert!(
            body.find("budget").unwrap() < body.find("notes").unwrap(),
            "{body}"
        );
    }

    #[test]
    fn an_app_that_could_not_be_read_is_reported_rather_than_dropped() {
        let scratch = Scratch::new("apps-skipped");
        scratch.app("good", "Good");
        std::fs::create_dir_all(scratch.apps().join("Notes")).unwrap();
        let router = router().with_apps_dir(scratch.apps());

        let body = body(&router.respond(&authorized_get("/api/apps")));
        assert!(body.contains("\"slug\":\"good\""), "{body}");
        assert!(body.contains("\"skipped\""), "{body}");
        assert!(body.contains("Notes"), "{body}");
        assert!(body.contains("lowercase"), "{body}");
    }

    #[test]
    fn a_router_without_an_apps_directory_says_so_instead_of_claiming_there_are_none() {
        // "No apps" and "this process does not know where apps live" are different readings, and
        // only one of them is true here.
        let body = body(&router().respond(&authorized_get("/api/apps")));
        assert!(body.contains("\"apps_directory\":null"), "{body}");
        assert!(body.contains("without an apps directory"), "{body}");
    }

    #[test]
    fn an_apps_directory_that_cannot_be_read_is_a_server_error_not_an_empty_list() {
        let scratch = Scratch::new("apps-unreadable");
        // A file where the directory should be: it exists, so this is not "first run".
        std::fs::write(scratch.apps(), "not a directory").unwrap();
        let router = router().with_apps_dir(scratch.apps());

        let response = router.respond(&authorized_get("/api/apps"));
        assert_eq!(response.status, 500);
        assert!(body(&response).contains("cannot list"), "{:?}", response);
    }

    #[test]
    fn the_panel_counts_the_apps_and_points_at_the_route_that_explains_the_rest() {
        let scratch = Scratch::new("apps-panel");
        scratch.app("notes", "Notes");
        std::fs::create_dir_all(scratch.apps().join("Broken")).unwrap();
        let router = router().with_apps_dir(scratch.apps());

        let page = body(&router.respond(&get("/", "127.0.0.1:4270")));
        assert!(page.contains("<strong>1</strong>"), "{page}");
        assert!(page.contains("/api/apps"), "{page}");
        assert!(page.contains("are not apps"), "{page}");
    }

    #[test]
    fn a_non_bearer_scheme_is_not_accepted() {
        let request = parse(&format!(
            "GET /api/apps HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nAuthorization: Basic {}\r\n",
            "a".repeat(64)
        ))
        .unwrap();
        assert_eq!(router().respond(&request).status, 401);
    }

    #[test]
    fn the_unbuilt_gateway_says_it_is_unbuilt() {
        let response = router().respond(&get("/app/notes/", "127.0.0.1:4270"));
        assert_eq!(response.status, 404);
        assert!(body(&response).contains("crates/gateway"));
    }

    #[test]
    fn an_unknown_path_is_not_found() {
        assert_eq!(
            router()
                .respond(&get("/nothing/here", "127.0.0.1:4270"))
                .status,
            404
        );
    }

    #[test]
    fn an_unconfigured_install_says_so_instead_of_pretending() {
        let router = Router::new(PORT, None, None);
        let response = router.respond(&get("/", "127.0.0.1:4270"));
        assert_eq!(response.status, 200);
        assert!(body(&response).contains("not configured"));

        let health = router.respond(&get("/api/health", "127.0.0.1:4270"));
        assert!(body(&health).contains("\"model_endpoint_configured\":false"));
    }

    #[test]
    fn a_model_name_with_markup_is_escaped() {
        let router = Router::new(PORT, Some("<script>x</script>".to_string()), None);
        let page = body(&router.respond(&get("/", "127.0.0.1:4270")));
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>x</script>"));
    }
}
