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
//! `POST /api/apps/<slug>/start|stop|restart` are the first routes here that change something
//! outside this process. Three decisions shape them, and each is checked before anything runs:
//! the method is `POST` because a browser prefetches a `GET`; the slug is validated against the
//! same rules that make it a directory name, because it arrives from the URL; and the directory
//! must exist, because "no app named that" and "the app failed" are different answers.
//!
//! `/api/apps` is the one route backed by a directory rather than by a value held in this struct:
//! it lists `~/.deepnest/apps` at request time, so an app that appeared a second ago is in the
//! next answer without a restart. What it cannot use it reports rather than drops — see
//! `deepnest_apps::list` — because the panel showing fewer apps than the user created has no other
//! way to say why.

use std::fmt::{self, Write as _};
use std::path::PathBuf;
use std::sync::Arc;

use deepnest_apps::{
    BuildError, Runner, Slug, StartError, State, StdRunner, Supervisor, TemplateError,
};
use serde_json::{json, Value};

use deepnest_gateway::{ForwardError, Forwarder, StdForwarder, Target};

use crate::http::{Method, Request, Response};
use crate::token::Token;

/// What the router needs in order to make an app real: the supervisor that owns its process, the
/// runner that builds it, and the deepnest home whose `logs/` gets both outputs.
///
/// They travel together because none is useful alone — a supervisor with nowhere to write a log
/// would start an app whose failures nobody could read, a runner with no home would build into the
/// void, and the home alone owns nothing.
#[derive(Clone)]
pub struct Lifecycle {
    pub supervisor: Arc<Supervisor>,
    /// How a build runs its steps. A trait object, so a test can build without a toolchain.
    pub builder: Arc<dyn Runner>,
    /// How a request reaches an app's server. A trait object for the same reason `Spawner` and
    /// `Prober` are: the router's job is deciding *which* app, and a test of that decision should
    /// not need an app listening on the port the kernel happened to allocate.
    pub forwarder: Arc<dyn Forwarder>,
    pub home: PathBuf,
}

impl Lifecycle {
    /// The lifecycle the binary uses: the real supervisor and the real build runner.
    pub fn new(supervisor: Arc<Supervisor>, home: impl Into<PathBuf>) -> Self {
        Self {
            supervisor,
            builder: Arc::new(StdRunner::new()),
            forwarder: Arc::new(StdForwarder),
            home: home.into(),
        }
    }

    /// The same lifecycle with a different forwarder.
    pub fn with_forwarder(mut self, forwarder: Arc<dyn Forwarder>) -> Self {
        self.forwarder = forwarder;
        self
    }

    /// The same lifecycle with a different build runner.
    pub fn with_builder(mut self, builder: Arc<dyn Runner>) -> Self {
        self.builder = builder;
        self
    }
}

impl fmt::Debug for Lifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The supervisor is a map of live processes, and printing it would report state that has
        // moved on by the time anyone read it.
        f.debug_struct("Lifecycle")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

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
    /// The supervisor and the home to log under. `None` in tests that only read: the lifecycle
    /// routes then say this process cannot start anything, rather than pretending they tried.
    lifecycle: Option<Lifecycle>,
}

impl Router {
    /// A router for a server bound on `port`.
    pub fn new(port: u16, model: Option<String>, token: Option<Token>) -> Self {
        Self {
            port,
            model,
            token,
            apps_dir: None,
            lifecycle: None,
        }
    }

    /// List apps from `dir`. Without it, `/api/apps` says the directory is unknown.
    pub fn with_apps_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.apps_dir = Some(dir.into());
        self
    }

    /// Let this router start and stop the apps in `dir`, logging them under `home`.
    ///
    /// Both are needed: `dir` is where the apps are, and `home` is where `logs/` sits — the log
    /// path is spelled in `deepnest_apps::logs` and not a second time here.
    pub fn with_lifecycle(
        mut self,
        supervisor: Arc<Supervisor>,
        home: impl Into<PathBuf>,
        dir: impl Into<PathBuf>,
    ) -> Self {
        self.lifecycle = Some(Lifecycle::new(supervisor, home));
        self.apps_dir = Some(dir.into());
        self
    }

    /// Build apps with `builder` instead of the real toolchain runner.
    ///
    /// Separate from [`Router::with_lifecycle`] because it is only ever the tests that want it: the
    /// binary builds with [`StdRunner`], and a test that needed bun and a C compiler to check which
    /// status a failed build returns would be measuring the toolchain.
    pub fn with_builder(mut self, builder: Arc<dyn Runner>) -> Self {
        if let Some(lifecycle) = self.lifecycle.take() {
            self.lifecycle = Some(lifecycle.with_builder(builder));
        }
        self
    }

    /// Forward app requests with `forwarder` instead of over a real socket.
    pub fn with_forwarder(mut self, forwarder: Arc<dyn Forwarder>) -> Self {
        if let Some(lifecycle) = self.lifecycle.take() {
            self.lifecycle = Some(lifecycle.with_forwarder(forwarder));
        }
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
            other if other.starts_with("/api/apps/") => {
                if !self.authorized(request) {
                    return unauthorized();
                }
                self.app_action(request)
            }
            other if other.starts_with("/api/") => {
                if self.authorized(request) {
                    Response::json(404, error_body("no such endpoint"))
                } else {
                    unauthorized()
                }
            }
            other if other.starts_with("/app/") => self.app_page(request),
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

    /// `POST /api/apps/<slug>/<action>` — the verbs that change a process.
    ///
    /// Every refusal below comes before anything is spawned, and each says which of the four
    /// things is wrong: the action, the method, the slug, or the app.
    /// Serve an app under `/app/<slug>/`.
    ///
    /// The one route on this port a browser navigates to rather than a program calls, so it never
    /// asks for the token: the token guards deepnest's own API, and a page the user opened by
    /// clicking a link that then demanded a header is a page that never loads. What keeps it from
    /// being an open door is that it can only reach an app's own loopback port — the app's
    /// authorisation, its own, arrives with the request.
    fn app_page(&self, request: &Request) -> Response {
        let rest = request.path.strip_prefix("/app/").unwrap_or_default();
        let slug_text = rest.split('/').next().unwrap_or_default();
        // A path that names no app is `404`, not `400`: this spelling arrived from an address bar,
        // and "there is no app at that address" is the reading the person can act on.
        let slug = match Slug::parse(slug_text) {
            Ok(slug) => slug,
            Err(_) => {
                return Response::json(404, error_body(&format!("no app named `{slug_text}`")))
            }
        };

        let Some(lifecycle) = &self.lifecycle else {
            return Response::json(
                503,
                error_body("this process cannot serve apps: it was started without a lifecycle"),
            );
        };
        let Some(apps_dir) = &self.apps_dir else {
            return Response::json(
                503,
                error_body(
                    "this process cannot find apps: it was started without an apps directory",
                ),
            );
        };
        if !apps_dir.join(slug.as_str()).is_dir() {
            return Response::json(
                404,
                error_body(&format!("no app named `{slug}` in {}", apps_dir.display())),
            );
        }

        // This layer reads one request per connection and no body, so a request that carried one
        // would arrive at the app saying nothing. Refusing by name is the honest answer; a
        // forward that dropped the content would report success for something it never sent.
        if !deepnest_gateway::proxy::can_forward(request.method) {
            return Response::text(
                405,
                format!(
                    "{} is not forwarded to an app: this server carries GET and HEAD only\n",
                    request.method.as_str()
                ),
            )
            .header("Allow", "GET, HEAD");
        }

        let target = match lifecycle.supervisor.state(&slug) {
            State::Running { port, .. } => Target::new(slug.as_str(), port),
            State::Stopped => {
                return Response::text(
                    503,
                    format!("`{slug}` is not running — POST /api/apps/{slug}/start\n"),
                )
            }
            State::Failed { reason } => {
                return Response::text(503, format!("`{slug}` is not running: {reason}\n"))
            }
        };

        match lifecycle.forwarder.forward(&target, request) {
            Ok(response) => response,
            // The app was running a moment ago and its port answers nothing now: from the browser's
            // side that is a gateway whose upstream is gone, and the useful next step is the log.
            Err(error @ ForwardError::Connect { .. }) => Response::text(
                502,
                format!(
                    "{error}\n\nthe app's log is {}\n",
                    deepnest_apps::log_path(&lifecycle.home, &slug).display()
                ),
            ),
            Err(error) => Response::text(502, format!("{error}\n")),
        }
    }

    fn app_action(&self, request: &Request) -> Response {
        let Some(rest) = request.path.strip_prefix("/api/apps/") else {
            return Response::json(404, error_body("no such endpoint"));
        };
        let Some((slug_text, action_text)) = rest.split_once('/') else {
            return Response::json(
                404,
                error_body(&format!(
                    "`/api/apps/{rest}` is not an endpoint — the actions are \
                     POST /api/apps/{rest}/<{}>",
                    Action::VERBS
                )),
            );
        };
        let action = match Action::parse(action_text) {
            Some(action) => action,
            None => {
                return Response::json(
                    404,
                    error_body(&format!(
                        "`{action_text}` is not an app action — use {}",
                        Action::VERBS
                    )),
                )
            }
        };
        // `POST` and nothing else. A state change behind `GET` is one a browser may make by
        // prefetching a link, and a `405` naming the method is what stops a client guessing.
        if request.method != Method::Post {
            return Response::json(
                405,
                error_body(&format!(
                    "{action_text} changes state: POST /api/apps/{slug_text}/{action_text}"
                )),
            )
            .header("Allow", "POST");
        }
        let slug = match Slug::parse(slug_text) {
            Ok(slug) => slug,
            Err(error) => {
                return Response::json(
                    400,
                    error_body(&format!("`{slug_text}` is not an app name: {error}")),
                )
            }
        };
        let Some(lifecycle) = &self.lifecycle else {
            return Response::json(
                503,
                error_body(
                    "this process cannot create, build, start or stop apps: it was started \
                     without a lifecycle",
                ),
            );
        };
        let Some(apps_dir) = &self.apps_dir else {
            return Response::json(
                503,
                error_body(
                    "this process cannot find apps: it was started without an apps directory",
                ),
            );
        };

        // A directory that is not there is "no such app", which is a different answer from any
        // failure to start one. What *makes* it an app is its manifest, and that is deliberately
        // not re-checked here: an app whose manifest stopped parsing must still be stoppable, and
        // `GET /api/apps` is where its parse failure is reported.
        let dir = apps_dir.join(slug.as_str());

        // `create` is the one action whose directory must *not* be there yet, so it is answered
        // before the check the other four share.
        if action == Action::Create {
            let name = query_parameter(&request.query, "name").unwrap_or_default();
            return match deepnest_apps::create(&slug, &dir, &name) {
                Ok(created) => Response::json(
                    201,
                    json!({
                        "slug": slug.as_str(),
                        "dir": created.dir.display().to_string(),
                        "files": created
                            .files
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>(),
                    })
                    .to_string(),
                ),
                // Not a fault in deepnest: the name is taken, and taking it would destroy what is
                // already there.
                Err(TemplateError::Occupied { .. }) => Response::json(
                    409,
                    error_body(&format!(
                        "`{slug}` already exists in {} — an app directory is never overwritten",
                        apps_dir.display()
                    )),
                ),
                Err(error) => Response::json(
                    500,
                    error_body(&format!("`{slug}` could not be created: {error}")),
                ),
            };
        }

        if !dir.is_dir() {
            return Response::json(
                404,
                error_body(&format!("no app named `{slug}` in {}", apps_dir.display())),
            );
        }

        match action {
            // Answered above, before the directory check it must not be subject to.
            Action::Create => unreachable!("create is answered before this match"),
            Action::Start => {
                let log = deepnest_apps::log_path(&lifecycle.home, &slug);
                started(&slug, lifecycle.supervisor.start(&slug, &dir, log))
            }
            Action::Stop => match lifecycle.supervisor.stop(&slug) {
                Ok(state) => state_body(&slug, &state),
                Err(error) => Response::json(
                    500,
                    error_body(&format!("`{slug}` could not be stopped: {error}")),
                ),
            },
            Action::Restart => {
                let log = deepnest_apps::log_path(&lifecycle.home, &slug);
                started(&slug, lifecycle.supervisor.restart(&slug, &dir, log))
            }
            Action::Build => {
                let log = deepnest_apps::build_log_path(&lifecycle.home, &slug);
                match deepnest_apps::build(&dir, log, lifecycle.builder.as_ref()) {
                    Ok(built) => Response::json(
                        200,
                        json!({
                            "slug": slug.as_str(),
                            "binary": built.binary.display().to_string(),
                            "log": built.log.display().to_string(),
                        })
                        .to_string(),
                    ),
                    // A directory that is not an app is the same reading as an app that is not
                    // there: the manifest is what makes it one.
                    Err(error @ BuildError::NotApp { .. }) => {
                        Response::json(404, error_body(&error.to_string()))
                    }
                    // A build that failed is a build that failed, however it failed: the report
                    // names the step and the file the step's output is in.
                    Err(error) => Response::json(
                        500,
                        json!({ "slug": slug.as_str(), "error": error.to_string() }).to_string(),
                    ),
                }
            }
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

/// The three state changes an app has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Write the files that make a directory an app — the only action whose directory must be new.
    Create,
    /// Run the toolchain over those files.
    Build,
    Start,
    Stop,
    Restart,
}

impl Action {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "create" => Some(Self::Create),
            "build" => Some(Self::Build),
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "restart" => Some(Self::Restart),
            _ => None,
        }
    }

    /// The verbs, for a refusal that tells the caller what would have worked.
    const VERBS: &str = "create, build, start, stop or restart";
}

/// The value of one query parameter, percent-decoded.
///
/// This layer reads no request body, so what a route has to be told arrives in the URL. `+` is a
/// space because that is what a browser sends for one in a query, and a malformed escape is left as
/// written rather than dropped: the name it produces then fails the manifest's non-blank check,
/// which is a reportable failure instead of a silently mangled name.
fn query_parameter(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (key, value) = match pair.split_once('=') {
            Some(pair) => pair,
            None => (pair, ""),
        };
        if key == name {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                let high = bytes.get(index + 1).copied().and_then(hex_value);
                let low = bytes.get(index + 2).copied().and_then(hex_value);
                match (high, low) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    // A caller's name is text, not bytes: anything that is not valid UTF-8 becomes the replacement
    // character rather than refusing the whole request.
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The answer to a start or a restart.
fn started(slug: &Slug, outcome: Result<State, StartError>) -> Response {
    match outcome {
        Ok(state) => state_body(slug, &state),
        // Both of these say the app is not in a state where "start" applies — it is already
        // running, or nothing has been built — and neither is a fault in deepnest. A `409` keeps
        // them apart from the three failures below, where something went wrong.
        Err(error @ (StartError::AlreadyRunning { .. } | StartError::NotBuilt { .. })) => {
            Response::json(
                409,
                json!({ "slug": slug.as_str(), "error": error.to_string() }).to_string(),
            )
        }
        // A program that would not start, died on the way up, or never answered. All three are
        // reported with the log's path, because the app is the one that knows why.
        Err(error) => Response::json(
            500,
            json!({ "slug": slug.as_str(), "error": error.to_string() }).to_string(),
        ),
    }
}

/// What is true about an app, as JSON.
fn state_body(slug: &Slug, state: &State) -> Response {
    let (port, pid, reason) = match state {
        State::Stopped => (Value::Null, Value::Null, Value::Null),
        State::Running { port, pid } => (json!(port), json!(pid), Value::Null),
        State::Failed { reason } => (Value::Null, Value::Null, json!(reason)),
    };
    Response::json(
        200,
        json!({
            "slug": slug.as_str(),
            "state": state.word(),
            "port": port,
            "pid": pid,
            "reason": reason,
        })
        .to_string(),
    )
}

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

        /// An app that has been built: a manifest, and a binary at the path the supervisor runs.
        fn built_app(&self, slug: &str, name: &str) {
            self.app(slug, name);
            let binary = self.apps().join(slug).join(deepnest_apps::EXPECTED_BINARY);
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, b"a stand-in for the app the template will build").unwrap();
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

    /// A forwarder that records where it was asked to go, and answers a page.
    struct RecordingForwarder {
        seen: std::sync::Mutex<Vec<(String, u16)>>,
        answer: u16,
    }

    impl RecordingForwarder {
        fn answering(status: u16) -> Arc<Self> {
            Arc::new(Self {
                seen: std::sync::Mutex::new(Vec::new()),
                answer: status,
            })
        }

        fn seen(&self) -> Vec<(String, u16)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Forwarder for RecordingForwarder {
        fn forward(
            &self,
            target: &Target,
            _request: &Request,
        ) -> Result<Response, deepnest_gateway::ForwardError> {
            self.seen
                .lock()
                .unwrap()
                .push((target.slug.clone(), target.port));
            Ok(Response::html(self.answer, "<html><head></head></html>"))
        }
    }

    /// The router and the supervisor behind it, so a test can read back what it allocated.
    fn supervised_forwarding(
        scratch: &Scratch,
        forwarder: Arc<dyn Forwarder>,
    ) -> (Router, Arc<deepnest_apps::Supervisor>) {
        let supervisor = Arc::new(deepnest_apps::Supervisor::with_parts(
            Box::new(FakeSpawner),
            Box::new(AlwaysReady),
        ));
        let router = router()
            .with_lifecycle(
                Arc::clone(&supervisor),
                scratch.base.clone(),
                scratch.apps(),
            )
            .with_builder(Arc::new(FakeBuilder::succeeds()))
            .with_forwarder(forwarder);
        (router, supervisor)
    }

    #[test]
    fn an_app_page_reaches_the_port_the_supervisor_allocated() {
        let scratch = Scratch::new("page-forward");
        scratch.built_app("notes", "Notes");
        let forwarder = RecordingForwarder::answering(200);
        let (router, supervisor) = supervised_forwarding(&scratch, forwarder.clone());

        let started = router.respond(&post("/api/apps/notes/start"));
        assert_eq!(started.status, 200, "{}", body(&started));

        let response = router.respond(&get("/app/notes/", "127.0.0.1:4270"));
        assert_eq!(response.status, 200);
        assert_eq!(forwarder.seen().len(), 1, "the forward did not happen");

        // The port is the one the supervisor handed out, not one the router guessed.
        let slug = Slug::parse("notes").unwrap();
        let port = match supervisor.state(&slug) {
            State::Running { port, .. } => port,
            other => panic!("{other:?}"),
        };
        assert_eq!(forwarder.seen()[0], ("notes".to_string(), port));
    }

    #[test]
    fn an_app_that_is_not_running_says_so_instead_of_forwarding() {
        let scratch = Scratch::new("page-stopped");
        scratch.built_app("notes", "Notes");
        let forwarder = RecordingForwarder::answering(200);
        let router = supervised(&scratch).with_forwarder(forwarder.clone());

        let response = router.respond(&get("/app/notes/", "127.0.0.1:4270"));
        assert_eq!(response.status, 503);
        let text = body(&response);
        assert!(text.contains("is not running"), "{text}");
        // The next step is a request, so the answer names it.
        assert!(text.contains("POST /api/apps/notes/start"), "{text}");
        assert!(
            forwarder.seen().is_empty(),
            "nothing was running to forward to"
        );
    }

    #[test]
    fn an_app_page_names_an_app_that_is_not_there_as_absent() {
        let scratch = Scratch::new("page-absent");
        let router = supervised(&scratch);

        let response = router.respond(&get("/app/notes/", "127.0.0.1:4270"));
        assert_eq!(response.status, 404);
        assert!(
            body(&response).contains("no app named `notes`"),
            "{}",
            body(&response)
        );

        // A path that cannot even be a slug is the same reading, not a 400: it came from an
        // address bar, and the reader cannot act on a validation message.
        for path in ["/app/Notes/", "/app/../etc/passwd", "/app/"] {
            let response = router.respond(&get(path, "127.0.0.1:4270"));
            assert_eq!(response.status, 404, "{path}");
        }
    }

    #[test]
    fn a_post_to_an_app_page_is_refused_rather_than_forwarded_without_its_body() {
        let scratch = Scratch::new("page-post");
        scratch.built_app("notes", "Notes");
        let forwarder = RecordingForwarder::answering(200);
        let router = supervised(&scratch).with_forwarder(forwarder.clone());

        let response = router.respond(&post("/app/notes/save"));
        assert_eq!(response.status, 405);
        assert_eq!(
            response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("allow"))
                .map(|(_, value)| value.as_str()),
            Some("GET, HEAD")
        );
        assert!(
            forwarder.seen().is_empty(),
            "a body-less POST reached the app"
        );
    }

    #[test]
    fn a_failed_forward_is_a_bad_gateway_naming_the_log() {
        // The app answered a moment ago and its port is silent now. That is not a 404 — the app
        // exists — and it is not a 500 either: the failure is upstream of this server.
        struct Refusing;
        impl Forwarder for Refusing {
            fn forward(
                &self,
                target: &Target,
                _request: &Request,
            ) -> Result<Response, deepnest_gateway::ForwardError> {
                Err(deepnest_gateway::ForwardError::Connect {
                    port: target.port,
                    source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
                })
            }
        }

        let scratch = Scratch::new("page-502");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch).with_forwarder(Arc::new(Refusing));
        assert_eq!(router.respond(&post("/api/apps/notes/start")).status, 200);

        let response = router.respond(&get("/app/notes/", "127.0.0.1:4270"));
        assert_eq!(response.status, 502);
        let text = body(&response);
        assert!(text.contains("notes.log"), "{text}");
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

    // --- the lifecycle routes ---------------------------------------------------------------

    /// A process that never ends on its own, so a start in a test is deterministic.
    struct NeverExits;

    impl deepnest_apps::Handle for NeverExits {
        fn id(&self) -> u32 {
            4242
        }

        fn poll(&mut self) -> std::io::Result<Option<deepnest_apps::Exit>> {
            Ok(None)
        }

        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FakeSpawner;

    impl deepnest_apps::Spawner for FakeSpawner {
        fn spawn(
            &self,
            _spec: &deepnest_apps::Spec,
        ) -> std::io::Result<Box<dyn deepnest_apps::Handle>> {
            Ok(Box::new(NeverExits))
        }
    }

    /// Answers on the first probe: what a start waits for is exercised in `deepnest-apps`, and
    /// what this module tests is which request gets which answer.
    struct AlwaysReady;

    impl deepnest_apps::Prober for AlwaysReady {
        fn ready(&self, _port: deepnest_apps::Port) -> bool {
            true
        }
    }

    /// The router, pointed at a scratch home, able to start and stop what is in it.
    fn supervised(scratch: &Scratch) -> Router {
        supervised_with(scratch, Arc::new(FakeBuilder::succeeds()))
    }

    /// The router with the build runner a test chooses.
    fn supervised_with(scratch: &Scratch, builder: Arc<dyn deepnest_apps::Runner>) -> Router {
        let supervisor = Arc::new(deepnest_apps::Supervisor::with_parts(
            Box::new(FakeSpawner),
            Box::new(AlwaysReady),
        ));
        router()
            .with_lifecycle(supervisor, scratch.base.clone(), scratch.apps())
            .with_builder(builder)
    }

    /// A build runner that records the steps it was asked to run.
    ///
    /// The steps are the app crate's business (`deepnest-apps` tests them against their own fake);
    /// what this module tests is which request gets which answer, so the runner only has to succeed
    /// or fail on demand.
    struct FakeBuilder {
        steps: std::sync::Mutex<Vec<String>>,
        succeed: bool,
    }

    impl FakeBuilder {
        fn succeeds() -> Self {
            Self {
                steps: std::sync::Mutex::new(Vec::new()),
                succeed: true,
            }
        }

        fn fails() -> Self {
            Self {
                steps: std::sync::Mutex::new(Vec::new()),
                succeed: false,
            }
        }

        fn steps(&self) -> Vec<String> {
            self.steps.lock().unwrap().clone()
        }
    }

    impl deepnest_apps::Runner for FakeBuilder {
        fn run(
            &self,
            step: &deepnest_apps::Step,
            _log: &std::path::Path,
        ) -> std::io::Result<deepnest_apps::Outcome> {
            self.steps.lock().unwrap().push(step.label.to_string());
            Ok(if self.succeed {
                deepnest_apps::Outcome {
                    code: Some(0),
                    detail: "exited with 0".to_string(),
                }
            } else {
                deepnest_apps::Outcome {
                    code: Some(101),
                    detail: "exited with 101".to_string(),
                }
            })
        }
    }

    fn post(path: &str) -> Request {
        parse(&format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\nContent-Length: 0\r\n",
            "a".repeat(64)
        ))
        .unwrap()
    }

    #[test]
    fn a_start_runs_the_app_and_reports_the_port_it_took() {
        let scratch = Scratch::new("life-start");
        scratch.built_app("notes", "Notes");
        let response = supervised(&scratch).respond(&post("/api/apps/notes/start"));
        assert_eq!(response.status, 200, "{:?}", response);
        let body = body(&response);
        assert!(body.contains("\"state\":\"running\""), "{body}");
        assert!(body.contains("\"pid\":4242"), "{body}");
        // A port was allocated for this start and is reported; it is not one the router chose.
        assert!(!body.contains("\"port\":null"), "{body}");
    }

    #[test]
    fn a_get_cannot_start_an_app() {
        // A browser prefetches links. A state change behind GET would be one it makes by looking.
        let scratch = Scratch::new("life-get");
        scratch.built_app("notes", "Notes");
        let response = supervised(&scratch).respond(&authorized_get("/api/apps/notes/start"));
        assert_eq!(response.status, 405);
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| *name == "Allow" && *value == "POST"));
    }

    #[test]
    fn starting_an_app_that_was_never_built_is_a_conflict_and_not_a_failure() {
        // Nothing is wrong with deepnest here: the step before this one has not been run.
        let scratch = Scratch::new("life-unbuilt");
        scratch.app("notes", "Notes");
        let response = supervised(&scratch).respond(&post("/api/apps/notes/start"));
        assert_eq!(response.status, 409, "{:?}", response);
        assert!(
            body(&response).contains("build the app first"),
            "{:?}",
            response
        );
    }

    #[test]
    fn starting_an_app_twice_says_it_is_already_running() {
        let scratch = Scratch::new("life-twice");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch);
        assert_eq!(router.respond(&post("/api/apps/notes/start")).status, 200);
        let again = router.respond(&post("/api/apps/notes/start"));
        assert_eq!(again.status, 409);
        assert!(body(&again).contains("already running"), "{:?}", again);
    }

    #[test]
    fn a_stop_reports_it_stopped_and_a_restart_runs_it_again() {
        let scratch = Scratch::new("life-stop");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch);

        assert_eq!(router.respond(&post("/api/apps/notes/start")).status, 200);

        let stopped = router.respond(&post("/api/apps/notes/stop"));
        assert_eq!(stopped.status, 200);
        assert!(
            body(&stopped).contains("\"state\":\"stopped\""),
            "{:?}",
            stopped
        );

        let restarted = router.respond(&post("/api/apps/notes/restart"));
        assert_eq!(restarted.status, 200);
        assert!(
            body(&restarted).contains("\"state\":\"running\""),
            "{:?}",
            restarted
        );
    }

    #[test]
    fn stopping_an_app_that_is_not_running_is_not_an_error() {
        // The caller's intent is the postcondition, and it already holds.
        let scratch = Scratch::new("life-stop-idle");
        scratch.built_app("notes", "Notes");
        let response = supervised(&scratch).respond(&post("/api/apps/notes/stop"));
        assert_eq!(response.status, 200);
    }

    #[test]
    fn an_app_that_is_not_there_is_not_found() {
        let scratch = Scratch::new("life-missing");
        let response = supervised(&scratch).respond(&post("/api/apps/ghost/start"));
        assert_eq!(response.status, 404);
        assert!(body(&response).contains("no app named"), "{:?}", response);
    }

    #[test]
    fn a_name_that_is_not_a_slug_is_refused_before_anything_runs() {
        // The slug arrives from the URL and becomes a path segment; it is checked against the same
        // rules that make it a directory name, so `..` never reaches the filesystem.
        let scratch = Scratch::new("life-bad-slug");
        let router = supervised(&scratch);
        for path in ["/api/apps/../start", "/api/apps/Notes/start"] {
            let response = router.respond(&post(path));
            assert_eq!(response.status, 400, "{path}: {:?}", response);
        }
    }

    #[test]
    fn an_unknown_action_is_not_found_and_the_refusal_lists_the_five_that_exist() {
        let scratch = Scratch::new("life-action");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch);

        let unknown = router.respond(&post("/api/apps/notes/delete"));
        assert_eq!(unknown.status, 404);
        assert!(
            body(&unknown).contains("create, build, start, stop or restart"),
            "{:?}",
            unknown
        );

        // A slug with no action at all is the same refusal one level up: it names the shape that
        // would have worked instead of leaving the caller to guess.
        let bare = router.respond(&post("/api/apps/notes"));
        assert_eq!(bare.status, 404);
        assert!(
            body(&bare).contains("create, build, start, stop or restart"),
            "{:?}",
            bare
        );
    }

    #[test]
    fn the_lifecycle_routes_need_the_token_like_every_other_api_route() {
        let scratch = Scratch::new("life-token");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch);
        let request =
            parse("POST /api/apps/notes/start HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n").unwrap();
        assert_eq!(router.respond(&request).status, 401);
    }

    #[test]
    fn a_server_without_a_supervisor_says_so_instead_of_pretending() {
        let scratch = Scratch::new("life-none");
        scratch.built_app("notes", "Notes");
        // Listing only: this router can say which apps exist and cannot run one.
        let router = router().with_apps_dir(scratch.apps());
        let response = router.respond(&post("/api/apps/notes/start"));
        assert_eq!(response.status, 503);
        assert!(
            body(&response).contains("without a lifecycle"),
            "{:?}",
            response
        );
    }

    // --- creating and building --------------------------------------------------------------

    fn post_query(path: &str, query: &str) -> Request {
        parse(&format!(
            "POST {path}?{query} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\n",
            "a".repeat(64)
        ))
        .unwrap()
    }

    #[test]
    fn a_create_writes_an_app_the_listing_then_shows() {
        let scratch = Scratch::new("create");
        let router = supervised(&scratch);

        let created = router.respond(&post_query("/api/apps/notes/create", "name=Notes"));
        assert_eq!(created.status, 201, "{:?}", created);
        let created_body = body(&created);
        assert!(
            created_body.contains("\"slug\":\"notes\""),
            "{created_body}"
        );
        assert!(created_body.contains("Cargo.toml"), "{created_body}");

        // The listing agrees with what the create said it wrote.
        let listing = body(&router.respond(&authorized_get("/api/apps")));
        assert!(listing.contains("\"slug\":\"notes\""), "{listing}");
        assert!(listing.contains("\"name\":\"Notes\""), "{listing}");
    }

    #[test]
    fn a_create_decodes_the_name_it_was_given() {
        // The name arrives in the URL because this layer reads no request body; a space and a
        // non-ASCII letter have to survive the trip.
        let scratch = Scratch::new("create-encoded");
        let router = supervised(&scratch);
        let response = router.respond(&post_query(
            "/api/apps/notes/create",
            "name=My%20Notes%20%E2%80%94%20v2",
        ));
        assert_eq!(response.status, 201, "{:?}", response);

        let listing = body(&router.respond(&authorized_get("/api/apps")));
        assert!(listing.contains("My Notes — v2"), "{listing}");
    }

    #[test]
    fn creating_an_app_that_exists_is_a_conflict_and_changes_nothing() {
        let scratch = Scratch::new("create-taken");
        scratch.app("notes", "Notes");
        let before =
            std::fs::read_to_string(scratch.apps().join("notes").join("deepnest.toml")).unwrap();

        let response = supervised(&scratch).respond(&post_query("/api/apps/notes/create", ""));
        assert_eq!(response.status, 409, "{:?}", response);
        assert!(
            body(&response).contains("never overwritten"),
            "{:?}",
            response
        );

        let after =
            std::fs::read_to_string(scratch.apps().join("notes").join("deepnest.toml")).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn a_build_runs_the_steps_and_reports_the_binary() {
        let scratch = Scratch::new("build-ok");
        scratch.built_app("notes", "Notes");
        let builder = Arc::new(FakeBuilder::succeeds());
        let response =
            supervised_with(&scratch, builder.clone()).respond(&post("/api/apps/notes/build"));

        assert_eq!(response.status, 200, "{:?}", response);
        let answer = body(&response);
        assert!(answer.contains("target/release/app"), "{answer}");
        assert!(answer.contains(".build.log"), "{answer}");
        assert_eq!(builder.steps().len(), 3, "{:?}", builder.steps());
    }

    #[test]
    fn a_build_that_fails_names_the_step_and_the_log() {
        let scratch = Scratch::new("build-fail");
        scratch.built_app("notes", "Notes");
        let response = supervised_with(&scratch, Arc::new(FakeBuilder::fails()))
            .respond(&post("/api/apps/notes/build"));

        assert_eq!(response.status, 500, "{:?}", response);
        let answer = body(&response);
        assert!(answer.contains("bun install"), "{answer}");
        assert!(answer.contains("notes.build.log"), "{answer}");
    }

    #[test]
    fn building_an_app_that_is_not_there_is_not_found() {
        let scratch = Scratch::new("build-missing");
        let response = supervised(&scratch).respond(&post("/api/apps/ghost/build"));
        assert_eq!(response.status, 404, "{:?}", response);
    }

    #[test]
    fn a_directory_that_is_not_an_app_cannot_be_built() {
        // A directory with no manifest is the wrong directory, not a build that failed.
        let scratch = Scratch::new("build-not-app");
        std::fs::create_dir_all(scratch.apps().join("junk")).unwrap();
        let response = supervised(&scratch).respond(&post("/api/apps/junk/build"));
        assert_eq!(response.status, 404, "{:?}", response);
        assert!(body(&response).contains("not an app"), "{:?}", response);
    }

    #[test]
    fn create_and_build_need_the_token_like_every_other_api_route() {
        let scratch = Scratch::new("create-token");
        let router = supervised(&scratch);
        for path in ["/api/apps/notes/create", "/api/apps/notes/build"] {
            let request =
                parse(&format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n")).unwrap();
            assert_eq!(router.respond(&request).status, 401, "{path}");
        }
    }

    #[test]
    fn a_query_parameter_is_read_once_and_decoded() {
        assert_eq!(
            query_parameter("name=Notes&other=1", "name"),
            Some("Notes".to_string())
        );
        assert_eq!(query_parameter("other=1", "name"), None);
        assert_eq!(query_parameter("", "name"), None);
        // `+` is a space in a query, and `%20` is the same space spelled differently.
        assert_eq!(
            query_parameter("name=My+Notes", "name"),
            Some("My Notes".to_string())
        );
        assert_eq!(
            query_parameter("name=My%20Notes", "name"),
            Some("My Notes".to_string())
        );
        // A malformed escape is left as written: the name it produces then fails the manifest's
        // non-blank check, which is reportable, instead of quietly becoming something else.
        assert_eq!(
            query_parameter("name=100%", "name"),
            Some("100%".to_string())
        );
    }

    #[test]
    fn a_model_name_with_markup_is_escaped() {
        let router = Router::new(PORT, Some("<script>x</script>".to_string()), None);
        let page = body(&router.respond(&get("/", "127.0.0.1:4270")));
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>x</script>"));
    }
}
