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

//! `/` is `crates/web`'s panel, and the two routes it needs live here: its script, served from the
//! crate's own file so the policy can be `script-src 'self'`, and `GET`/`POST /api/config` — the
//! one route that reads a **request body**. The body and not the URL, because what it carries is an
//! API key: a key in a URL is written into the browser's history and into whatever logs the URL,
//! which is the same reason the control token travels in a header rather than in the path.
//!
//! A save takes effect in this process rather than at the next start. The model name is behind a
//! lock for exactly that reason: a first-run form whose answer is "restart me" is asking the user
//! to do something the program does not need.
//!
//! `/dialog.js` and `/dialog.css` are the floating dialog that `crates/gateway` injects into every
//! page an app answers. They are served without the token for a reason the panel's script does not
//! have: the page that loads them belongs to the app, whose own script shares this origin, so a
//! token written there would be a token the app could read and spend.

use std::fmt::{self, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use deepnest_apps::{
    BuildError, Runner, Slug, StartError, State, StdRunner, Supervisor, TemplateError,
};
use serde_json::{json, Value};

use deepnest_gateway::{Answer, ForwardError, Forwarder, StdForwarder, Target};
use deepnest_packaging::{Probe, StdProbe};
use deepnest_web::Panel;

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
    /// How deepnest finds out whether this machine can compile C at all. A trait object for the
    /// same reason `builder` is one: the check that refuses a build has to be testable on a machine
    /// that happens to have a compiler, and on one that does not.
    pub preflight: Arc<dyn Probe>,
    pub home: PathBuf,
}

impl Lifecycle {
    /// The lifecycle the binary uses: the real supervisor and the real build runner.
    pub fn new(supervisor: Arc<Supervisor>, home: impl Into<PathBuf>) -> Self {
        Self {
            supervisor,
            builder: Arc::new(StdRunner::new()),
            forwarder: Arc::new(StdForwarder),
            preflight: Arc::new(StdProbe),
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

    /// The same lifecycle with a different C-toolchain probe.
    pub fn with_preflight(mut self, preflight: Arc<dyn Probe>) -> Self {
        self.preflight = preflight;
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
    ///
    /// Behind a lock because the panel can change it: `POST /api/config` writes the file *and*
    /// updates this, so what the page and the health probe report is the file that is on disk now.
    /// A `RwLock` and not a `Mutex` because every reader only reads.
    model: Arc<RwLock<Option<String>>>,
    /// The control token. `None` only in tests — the binary always has one.
    token: Option<Token>,
    /// `~/.deepnest/apps`, when the caller knows where the home is. `None` reports that the list
    /// is unavailable instead of answering "no apps", which would be a different claim.
    apps_dir: Option<PathBuf>,
    /// The supervisor and the home to log under. `None` in tests that only read: the lifecycle
    /// routes then say this process cannot start anything, rather than pretending they tried.
    lifecycle: Option<Lifecycle>,
    /// `config.toml`, for the panel's first-run form. `None` means this process has nowhere to
    /// write one, and `POST /api/config` says so instead of succeeding into the void.
    config_path: Option<PathBuf>,
}

impl Router {
    /// A router for a server bound on `port`.
    pub fn new(port: u16, model: Option<String>, token: Option<Token>) -> Self {
        Self {
            port,
            model: Arc::new(RwLock::new(model)),
            token,
            apps_dir: None,
            lifecycle: None,
            config_path: None,
        }
    }

    /// Let `POST /api/config` write `path`.
    ///
    /// Separate from the constructor for the reason the other builders are separate: a router that
    /// only reads (a test, a listing server) must not acquire the ability to rewrite the user's
    /// config by being constructed.
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
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
        self,
        supervisor: Arc<Supervisor>,
        home: impl Into<PathBuf>,
        dir: impl Into<PathBuf>,
    ) -> Self {
        self.with_parts(Lifecycle::new(supervisor, home), dir)
    }

    /// The same, with a lifecycle built elsewhere.
    ///
    /// It takes the lifecycle whole rather than picking its fields apart, so what a caller chose —
    /// a builder, a forwarder — arrives with it. A server that rebuilt a lifecycle from its
    /// supervisor and home would silently drop the other two, and "the forwarder you passed was
    /// not the one used" is a defect that only shows up as a test measuring the wrong thing.
    pub fn with_parts(mut self, lifecycle: Lifecycle, dir: impl Into<PathBuf>) -> Self {
        self.lifecycle = Some(lifecycle);
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

    /// Decide whether a build may start with `preflight` instead of asking this machine.
    pub fn with_preflight(mut self, preflight: Arc<dyn Probe>) -> Self {
        if let Some(lifecycle) = self.lifecycle.take() {
            self.lifecycle = Some(lifecycle.with_preflight(preflight));
        }
        self
    }

    /// Answer one request.
    ///
    /// The answer is one of two things, and the second one is why this is not a `Response`: an app
    /// reached through `/app/<slug>/` may hold the connection open — a WebSocket that just
    /// upgraded, an event stream with no end — and a caller handed a buffer would have to invent
    /// the rest of it. Every route of deepnest's own answers a body; the app's answer depends on
    /// what the app did.
    pub fn respond(&self, request: &Request) -> Answer {
        if request.method == Method::Head {
            // Nothing below distinguishes HEAD from GET: the body is dropped at write time, and
            // that is the correct semantic. Naming it here documents the intent.
        }

        if let Err(refusal) = self.host_is_ours(request) {
            return Answer::Full(refusal);
        }

        let path = request.path.as_str();

        // One route reads a body, and every other route refuses one by name. Reading it and then
        // routing anyway would answer "done" to a request whose content was never looked at, which
        // is the one answer a server must not give about something it did not read.
        if !request.body.is_empty() && path != CONFIG_PATH {
            return Answer::Full(Response::text(
                400,
                format!("{path} takes no request body — send none, or send one to {CONFIG_PATH}\n"),
            ));
        }

        match path {
            "/" | "/index.html" => Answer::Full(
                Response::html(200, self.panel())
                    .header("Content-Security-Policy", deepnest_web::CSP),
            ),
            // The panel's script, from the crate's own file. A route rather than an inline block so
            // the policy can be `script-src 'self'`: a string that reaches the page from the
            // filesystem — an app's name — cannot become code under it.
            "/panel.js" => Answer::Full(Response::new(
                200,
                "text/javascript; charset=utf-8",
                deepnest_web::SCRIPT.as_bytes().to_vec(),
            )),
            // The app page's dialog, from `crates/web`. Both are named by the markup
            // `deepnest_gateway::dialog` injects, and both are served without the token: the page
            // that loads them cannot carry one, because the app's own script shares the origin.
            "/dialog.js" => Answer::Full(Response::new(
                200,
                "text/javascript; charset=utf-8",
                deepnest_web::DIALOG_SCRIPT.as_bytes().to_vec(),
            )),
            "/dialog.css" => Answer::Full(Response::new(
                200,
                "text/css; charset=utf-8",
                deepnest_web::DIALOG_STYLE.as_bytes().to_vec(),
            )),
            "/api/health" => Answer::Full(Response::json(200, self.health())),
            "/api/config" => {
                if !self.authorized(request) {
                    return Answer::Full(unauthorized());
                }
                Answer::Full(self.config_route(request))
            }
            "/api/apps" => {
                if !self.authorized(request) {
                    return Answer::Full(unauthorized());
                }
                let (status, body) = self.apps();
                Answer::Full(Response::json(status, body))
            }
            other if other.starts_with("/api/apps/") => {
                if !self.authorized(request) {
                    return Answer::Full(unauthorized());
                }
                Answer::Full(self.app_action(request))
            }
            other if other.starts_with("/api/") => Answer::Full(if self.authorized(request) {
                Response::json(404, error_body("no such endpoint"))
            } else {
                unauthorized()
            }),
            other if other.starts_with("/app/") => self.app_page(request),
            _ => Answer::Full(Response::json(404, error_body("no such path"))),
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

    /// The panel: `crates/web`'s page, carrying what only this process knows.
    ///
    /// The port, the control token, the endpoint state and a line about the apps directory are
    /// rendered into the markup, so a reader with script off — or with a script that failed to load
    /// — still gets a page that is true. The app list itself is the script's job: it changes
    /// whenever a directory does, and rendering it here would make it stale between requests.
    fn panel(&self) -> String {
        deepnest_web::page(&Panel {
            port: self.port,
            token: self
                .token
                .as_ref()
                .map(|token| token.as_str().to_string())
                .unwrap_or_default(),
            model: self.model(),
            apps: self.apps_summary(),
        })
    }

    /// The model name this process knows, if any.
    fn model(&self) -> Option<String> {
        self.model
            .read()
            .ok()
            .and_then(|model| model.as_ref().cloned())
    }

    /// One plain-text line about the apps directory, for the panel.
    ///
    /// It counts what could not be used and points at the route that explains each one: a page that
    /// silently showed fewer apps than exist would leave the user with nothing to act on. Plain
    /// text rather than markup, because the page escapes what it is given.
    fn apps_summary(&self) -> String {
        let Some(dir) = &self.apps_dir else {
            return "this process was started without an apps directory".to_string();
        };
        let shown = dir.display().to_string();
        match deepnest_apps::list(dir) {
            Ok(listing) if listing.is_empty() && listing.skipped.is_empty() => {
                format!("none yet in {shown}")
            }
            Ok(listing) => {
                let apps = listing.apps.len();
                let skipped = listing.skipped.len();
                let mut line = format!("{apps} {} in {shown}", plural(apps, "app", "apps"));
                if skipped > 0 {
                    let _ = write!(
                        line,
                        "; {skipped} other {} there {} not {} — GET /api/apps says why",
                        plural(skipped, "entry", "entries"),
                        if skipped == 1 { "is" } else { "are" },
                        plural(skipped, "an app", "apps"),
                    );
                }
                line
            }
            Err(error) => format!("cannot be listed — {shown}: {error}"),
        }
    }

    /// `GET` and `POST /api/config`, and a refusal naming both for anything else.
    fn config_route(&self, request: &Request) -> Response {
        match request.method {
            // `HEAD` reads like `GET`: the body is dropped at write time, and the headers are the
            // ones a `GET` would have produced.
            Method::Get | Method::Head => self.config_view(),
            Method::Post => self.save_config(request),
            Method::Delete => Response::json(
                405,
                error_body("the config is read with GET and written with POST"),
            )
            .header("Allow", "GET, POST"),
        }
    }

    /// What the page needs to know about the config — and nothing it does not.
    ///
    /// The key is not in this answer and never will be: the page put it here, and nothing this
    /// server answers has a use for it.
    fn config_view(&self) -> Response {
        let model = self.model();
        Response::json(
            200,
            json!({ "configured": model.is_some(), "model": model }).to_string(),
        )
    }

    /// `POST /api/config` — the first-run form, and the only route that writes a file.
    ///
    /// Everything is checked before anything is written: a body this route cannot read, a body
    /// whose values [`crate::config::save`] would refuse, and a process with no config path all
    /// answer before the file is touched, so a refused form leaves the user's config exactly as it
    /// was.
    fn save_config(&self, request: &Request) -> Response {
        let Some(path) = &self.config_path else {
            return Response::json(
                503,
                error_body(
                    "this process cannot write a config: it was started without a config path, so \
                     the endpoint is set in config.toml by hand",
                ),
            );
        };
        let Some(text) = request.body_text() else {
            return Response::json(400, error_body("the config body must be UTF-8 JSON"));
        };
        if text.trim().is_empty() {
            return Response::json(
                400,
                error_body(
                    "the config body is empty — send {\"base_url\":…,\"api_key\":…,\"model\":…}",
                ),
            );
        }
        let submitted: Submitted = match serde_json::from_str(text) {
            Ok(submitted) => submitted,
            Err(error) => {
                return Response::json(
                    400,
                    error_body(&format!(
                        "the config body is not the JSON this route reads (base_url, api_key, \
                         model): {error}"
                    )),
                )
            }
        };

        let llm = crate::config::Llm {
            base_url: submitted.base_url,
            api_key: submitted.api_key,
            model: submitted.model,
        };
        match crate::config::save(path, &llm) {
            Ok(()) => {
                // The running process picks it up now. Nothing else caches the config, so there is
                // nothing to restart and the answer says nothing about one.
                if let Ok(mut model) = self.model.write() {
                    *model = Some(llm.model.trim().to_string());
                }
                Response::json(
                    200,
                    json!({
                        "configured": true,
                        "model": llm.model.trim(),
                        "path": path.display().to_string(),
                    })
                    .to_string(),
                )
            }
            // A value the binary would refuse to start on. Refusing it here is the whole point of
            // writing through `save` rather than through `toml::to_string`.
            Err(error @ crate::config::ConfigError::Unusable { .. }) => {
                Response::json(400, error_body(&error.to_string()))
            }
            Err(error) => Response::json(500, error_body(&error.to_string())),
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
    ///
    /// The one route that may answer with a connection rather than a body: an app that upgrades or
    /// streams is entitled to keep talking, and every refusal above the forward is still a body,
    /// because "the app is not running" is an answer this layer can finish.
    fn app_page(&self, request: &Request) -> Answer {
        let rest = request.path.strip_prefix("/app/").unwrap_or_default();
        let slug_text = rest.split('/').next().unwrap_or_default();
        // A path that names no app is `404`, not `400`: this spelling arrived from an address bar,
        // and "there is no app at that address" is the reading the person can act on.
        let slug = match Slug::parse(slug_text) {
            Ok(slug) => slug,
            Err(_) => {
                return Answer::Full(Response::json(
                    404,
                    error_body(&format!("no app named `{slug_text}`")),
                ))
            }
        };

        let Some(lifecycle) = &self.lifecycle else {
            return Answer::Full(Response::json(
                503,
                error_body("this process cannot serve apps: it was started without a lifecycle"),
            ));
        };
        let Some(apps_dir) = &self.apps_dir else {
            return Answer::Full(Response::json(
                503,
                error_body(
                    "this process cannot find apps: it was started without an apps directory",
                ),
            ));
        };
        if !apps_dir.join(slug.as_str()).is_dir() {
            return Answer::Full(Response::json(
                404,
                error_body(&format!("no app named `{slug}` in {}", apps_dir.display())),
            ));
        }

        // This layer reads one request per connection and no body, so a request that carried one
        // would arrive at the app saying nothing. Refusing by name is the honest answer; a
        // forward that dropped the content would report success for something it never sent.
        if !deepnest_gateway::proxy::can_forward(request.method) {
            return Answer::Full(
                Response::text(
                    405,
                    format!(
                        "{} is not forwarded to an app: this server carries GET and HEAD only\n",
                        request.method.as_str()
                    ),
                )
                .header("Allow", "GET, HEAD"),
            );
        }

        let target = match lifecycle.supervisor.state(&slug) {
            State::Running { port, .. } => Target::new(slug.as_str(), port),
            State::Stopped => {
                return Answer::Full(Response::text(
                    503,
                    format!("`{slug}` is not running — POST /api/apps/{slug}/start\n"),
                ))
            }
            State::Failed { reason } => {
                return Answer::Full(Response::text(
                    503,
                    format!("`{slug}` is not running: {reason}\n"),
                ))
            }
        };

        match lifecycle.forwarder.forward(&target, request) {
            // Already an answer: a body, or the connection the app is holding.
            Ok(answer) => answer,
            // The app was running a moment ago and its port answers nothing now: from the browser's
            // side that is a gateway whose upstream is gone, and the useful next step is the log.
            Err(error @ ForwardError::Connect { .. }) => Answer::Full(Response::text(
                502,
                format!(
                    "{error}\n\nthe app's log is {}\n",
                    deepnest_apps::log_path(&lifecycle.home, &slug).display()
                ),
            )),
            Err(error) => Answer::Full(Response::text(502, format!("{error}\n"))),
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
                // The README's promise, kept before the first step instead of discovered by a
                // compiler: `crates/apps` reports a step whose program will not start, and that
                // report is truthful but late — `bun install` and `bun run build` have already
                // run, and the user is a minute into a compile that was never going to work.
                // `Undetermined` refuses too, because "cannot tell" is not "yes".
                let preflight = deepnest_packaging::check(lifecycle.preflight.as_ref());
                if !preflight.ready() {
                    return Response::json(
                        412,
                        error_body(&format!("`{slug}` cannot be built yet: {preflight}")),
                    );
                }
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
            "model_endpoint_configured": self.model().is_some(),
            "pid": std::process::id(),
        })
        .to_string()
    }
}

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

/// The one route that reads a request body.
const CONFIG_PATH: &str = "/api/config";

/// The body `POST /api/config` reads.
///
/// `deny_unknown_fields` for the reason [`crate::config::Config`] has it: `apikey` instead of
/// `api_key` is a typo, and a route that read "no key" from a body plainly carrying one would
/// report success for a config the binary then refuses to start on.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Submitted {
    base_url: String,
    api_key: String,
    model: String,
}

/// `one` or `many`, so a line the user reads does not say "1 apps".
fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 {
        one
    } else {
        many
    }
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
            let response = router.respond(&get("/", host)).complete();
            assert_eq!(response.status, 200, "Host: {host}");
            assert!(body(&response).contains("127.0.0.1:4270"));
        }
    }

    #[test]
    fn a_foreign_host_is_refused() {
        // DNS rebinding: the name resolves to loopback, so the port is reachable; the Host
        // header is the one thing the attacking page cannot make truthful.
        let response = router().respond(&get("/", "evil.example")).complete();
        assert_eq!(response.status, 403);
    }

    #[test]
    fn a_request_without_host_is_refused() {
        let request = parse("GET / HTTP/1.1\r\n").unwrap();
        assert_eq!(router().respond(&request).complete().status, 400);
    }

    #[test]
    fn the_dialog_the_gateway_injects_is_served_from_here() {
        // Two things a later rename would break silently: the paths the injected markup names are
        // the paths this router answers, and both are served without the token — the page that
        // loads them is the app's, and the app's own script shares this origin.
        assert_eq!(
            deepnest_gateway::dialog::SCRIPT_PATH,
            deepnest_web::DIALOG_SCRIPT_PATH
        );
        assert_eq!(
            deepnest_gateway::dialog::STYLE_PATH,
            deepnest_web::DIALOG_STYLE_PATH
        );

        let router = router();
        let unwrapped = router
            .respond(&get(
                deepnest_gateway::dialog::SCRIPT_PATH,
                "127.0.0.1:4270",
            ))
            .complete();
        assert_eq!(unwrapped.status, 200);
        assert_eq!(unwrapped.content_type, "text/javascript; charset=utf-8");
        let script = body(&unwrapped);
        assert!(script.contains("deepnest-dialog"), "{script}");
        // The dialog carries no control token: the app's script would be able to read it.
        assert!(!script.contains("deepnest-token"), "{script}");

        let styled = router
            .respond(&get(deepnest_gateway::dialog::STYLE_PATH, "127.0.0.1:4270"))
            .complete();
        assert_eq!(styled.status, 200);
        assert_eq!(styled.content_type, "text/css; charset=utf-8");
        assert!(body(&styled).contains("#deepnest-dialog"));
    }

    #[test]
    fn health_answers_without_the_token() {
        // A probe should not have to read a 0600 file, and this route carries no user data.
        let response = router()
            .respond(&get("/api/health", "127.0.0.1:4270"))
            .complete();
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
        let response = router()
            .respond(&get("/api/apps", "127.0.0.1:4270"))
            .complete();
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
        assert_eq!(router().respond(&request).complete().status, 401);
    }

    #[test]
    fn an_api_route_with_the_token_reaches_the_router_and_finds_nothing() {
        let request = parse(&format!(
            "GET /api/nothing-here HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\n",
            "a".repeat(64)
        ))
        .unwrap();
        // 404, not 401: the token was accepted, and this endpoint genuinely does not exist.
        assert_eq!(router().respond(&request).complete().status, 404);
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
            router
                .respond(&get("/api/apps", "127.0.0.1:4270"))
                .complete()
                .status,
            401
        );
        assert_eq!(
            router
                .respond(&authorized_get("/api/apps"))
                .complete()
                .status,
            200
        );
    }

    #[test]
    fn the_app_list_names_what_is_there() {
        let scratch = Scratch::new("apps-list");
        scratch.app("notes", "Notes");
        scratch.app("budget", "Budget");
        let router = router().with_apps_dir(scratch.apps());

        let response = router.respond(&authorized_get("/api/apps")).complete();
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

        let body = body(&router.respond(&authorized_get("/api/apps")).complete());
        assert!(body.contains("\"slug\":\"good\""), "{body}");
        assert!(body.contains("\"skipped\""), "{body}");
        assert!(body.contains("Notes"), "{body}");
        assert!(body.contains("lowercase"), "{body}");
    }

    #[test]
    fn a_router_without_an_apps_directory_says_so_instead_of_claiming_there_are_none() {
        // "No apps" and "this process does not know where apps live" are different readings, and
        // only one of them is true here.
        let body = body(&router().respond(&authorized_get("/api/apps")).complete());
        assert!(body.contains("\"apps_directory\":null"), "{body}");
        assert!(body.contains("without an apps directory"), "{body}");
    }

    #[test]
    fn an_apps_directory_that_cannot_be_read_is_a_server_error_not_an_empty_list() {
        let scratch = Scratch::new("apps-unreadable");
        // A file where the directory should be: it exists, so this is not "first run".
        std::fs::write(scratch.apps(), "not a directory").unwrap();
        let router = router().with_apps_dir(scratch.apps());

        let response = router.respond(&authorized_get("/api/apps")).complete();
        assert_eq!(response.status, 500);
        assert!(body(&response).contains("cannot list"), "{:?}", response);
    }

    #[test]
    fn the_panel_reports_what_is_on_disk_without_any_script() {
        // The panel's markup has to be true on its own: a browser with script off, or a script
        // that failed to load, still learns how many apps there are and that one entry is not one.
        let scratch = Scratch::new("apps-panel");
        scratch.app("notes", "Notes");
        std::fs::create_dir_all(scratch.apps().join("Broken")).unwrap();
        let router = router().with_apps_dir(scratch.apps());

        let page = body(&router.respond(&get("/", "127.0.0.1:4270")).complete());
        assert!(page.contains("1 app in"), "{page}");
        assert!(page.contains("is not an app"), "{page}");
        assert!(page.contains("/api/apps"), "{page}");
    }

    #[test]
    fn a_non_bearer_scheme_is_not_accepted() {
        let request = parse(&format!(
            "GET /api/apps HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nAuthorization: Basic {}\r\n",
            "a".repeat(64)
        ))
        .unwrap();
        assert_eq!(router().respond(&request).complete().status, 401);
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
        ) -> Result<deepnest_gateway::Answer, deepnest_gateway::ForwardError> {
            self.seen
                .lock()
                .unwrap()
                .push((target.slug.clone(), target.port));
            Ok(deepnest_gateway::Answer::Full(Response::html(
                self.answer,
                "<html><head></head></html>",
            )))
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
            .with_preflight(Arc::new(CcInstalled))
            .with_forwarder(forwarder);
        (router, supervisor)
    }

    #[test]
    fn an_app_page_reaches_the_port_the_supervisor_allocated() {
        let scratch = Scratch::new("page-forward");
        scratch.built_app("notes", "Notes");
        let forwarder = RecordingForwarder::answering(200);
        let (router, supervisor) = supervised_forwarding(&scratch, forwarder.clone());

        let started = router.respond(&post("/api/apps/notes/start")).complete();
        assert_eq!(started.status, 200, "{}", body(&started));

        let response = router
            .respond(&get("/app/notes/", "127.0.0.1:4270"))
            .complete();
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

        let response = router
            .respond(&get("/app/notes/", "127.0.0.1:4270"))
            .complete();
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

        let response = router
            .respond(&get("/app/notes/", "127.0.0.1:4270"))
            .complete();
        assert_eq!(response.status, 404);
        assert!(
            body(&response).contains("no app named `notes`"),
            "{}",
            body(&response)
        );

        // A path that cannot even be a slug is the same reading, not a 400: it came from an
        // address bar, and the reader cannot act on a validation message.
        for path in ["/app/Notes/", "/app/../etc/passwd", "/app/"] {
            let response = router.respond(&get(path, "127.0.0.1:4270")).complete();
            assert_eq!(response.status, 404, "{path}");
        }
    }

    #[test]
    fn a_post_to_an_app_page_is_refused_rather_than_forwarded_without_its_body() {
        let scratch = Scratch::new("page-post");
        scratch.built_app("notes", "Notes");
        let forwarder = RecordingForwarder::answering(200);
        let router = supervised(&scratch).with_forwarder(forwarder.clone());

        let response = router.respond(&post("/app/notes/save")).complete();
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
    fn an_app_answer_that_is_a_connection_is_handed_back_as_one() {
        // The router must not flatten a tunnel into a body. Once an app has upgraded, this layer
        // owns a connection and no longer has an answer to write: turning it into a `Response`
        // would mean inventing bytes the app never sent.
        struct Holding;

        impl Forwarder for Holding {
            fn forward(
                &self,
                _target: &Target,
                _request: &Request,
            ) -> Result<deepnest_gateway::Answer, deepnest_gateway::ForwardError> {
                let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
                let addr = listener.local_addr().unwrap();
                std::thread::spawn(move || {
                    let _ = listener.accept();
                });
                let upstream = std::net::TcpStream::connect(addr).unwrap();
                Ok(deepnest_gateway::Answer::Stream(
                    deepnest_gateway::Stream::new(
                        upstream,
                        b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\r\n".to_vec(),
                        deepnest_gateway::StreamKind::Upgrade,
                    ),
                ))
            }
        }

        let scratch = Scratch::new("page-stream");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch).with_forwarder(Arc::new(Holding));
        assert_eq!(
            router
                .respond(&post("/api/apps/notes/start"))
                .complete()
                .status,
            200
        );

        match router.respond(&get("/app/notes/socket", "127.0.0.1:4270")) {
            deepnest_gateway::Answer::Stream(stream) => assert_eq!(
                stream.head(),
                b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\r\n"
            ),
            other => panic!("a connection was flattened into an answer: {other:?}"),
        }
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
            ) -> Result<deepnest_gateway::Answer, deepnest_gateway::ForwardError> {
                Err(deepnest_gateway::ForwardError::Connect {
                    port: target.port,
                    source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
                })
            }
        }

        let scratch = Scratch::new("page-502");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch).with_forwarder(Arc::new(Refusing));
        assert_eq!(
            router
                .respond(&post("/api/apps/notes/start"))
                .complete()
                .status,
            200
        );

        let response = router
            .respond(&get("/app/notes/", "127.0.0.1:4270"))
            .complete();
        assert_eq!(response.status, 502);
        let text = body(&response);
        assert!(text.contains("notes.log"), "{text}");
    }

    #[test]
    fn an_unknown_path_is_not_found() {
        assert_eq!(
            router()
                .respond(&get("/nothing/here", "127.0.0.1:4270"))
                .complete()
                .status,
            404
        );
    }

    #[test]
    fn an_unconfigured_install_says_so_instead_of_pretending() {
        let router = Router::new(PORT, None, None);
        let response = router.respond(&get("/", "127.0.0.1:4270")).complete();
        assert_eq!(response.status, 200);
        assert!(body(&response).contains("not configured"));

        let health = router
            .respond(&get("/api/health", "127.0.0.1:4270"))
            .complete();
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
    ///
    /// The C-toolchain probe is a fake that always finds one: whether this machine can compile C is
    /// a fact about the machine, and a test of the build *route* must not change its answer between
    /// a laptop and a CI image. The refusal has its own tests, which choose the answer they want.
    fn supervised_with(scratch: &Scratch, builder: Arc<dyn deepnest_apps::Runner>) -> Router {
        let supervisor = Arc::new(deepnest_apps::Supervisor::with_parts(
            Box::new(FakeSpawner),
            Box::new(AlwaysReady),
        ));
        router()
            .with_lifecycle(supervisor, scratch.base.clone(), scratch.apps())
            .with_builder(builder)
            .with_preflight(Arc::new(CcInstalled))
    }

    /// A probe that finds the C toolchain, so a build route runs its steps here.
    struct CcInstalled;

    impl Probe for CcInstalled {
        fn answers(&self, _kind: deepnest_packaging::Kind) -> deepnest_packaging::Finding {
            deepnest_packaging::Finding::Present {
                at: "/usr/bin/cc".to_string(),
            }
        }
    }

    /// A probe that says whatever a test needs it to say.
    struct CcSaying(deepnest_packaging::Finding);

    impl Probe for CcSaying {
        fn answers(&self, _kind: deepnest_packaging::Kind) -> deepnest_packaging::Finding {
            self.0.clone()
        }
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
        let response = supervised(&scratch)
            .respond(&post("/api/apps/notes/start"))
            .complete();
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
        let response = supervised(&scratch)
            .respond(&authorized_get("/api/apps/notes/start"))
            .complete();
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
        let response = supervised(&scratch)
            .respond(&post("/api/apps/notes/start"))
            .complete();
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
        assert_eq!(
            router
                .respond(&post("/api/apps/notes/start"))
                .complete()
                .status,
            200
        );
        let again = router.respond(&post("/api/apps/notes/start")).complete();
        assert_eq!(again.status, 409);
        assert!(body(&again).contains("already running"), "{:?}", again);
    }

    #[test]
    fn a_stop_reports_it_stopped_and_a_restart_runs_it_again() {
        let scratch = Scratch::new("life-stop");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch);

        assert_eq!(
            router
                .respond(&post("/api/apps/notes/start"))
                .complete()
                .status,
            200
        );

        let stopped = router.respond(&post("/api/apps/notes/stop")).complete();
        assert_eq!(stopped.status, 200);
        assert!(
            body(&stopped).contains("\"state\":\"stopped\""),
            "{:?}",
            stopped
        );

        let restarted = router.respond(&post("/api/apps/notes/restart")).complete();
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
        let response = supervised(&scratch)
            .respond(&post("/api/apps/notes/stop"))
            .complete();
        assert_eq!(response.status, 200);
    }

    #[test]
    fn an_app_that_is_not_there_is_not_found() {
        let scratch = Scratch::new("life-missing");
        let response = supervised(&scratch)
            .respond(&post("/api/apps/ghost/start"))
            .complete();
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
            let response = router.respond(&post(path)).complete();
            assert_eq!(response.status, 400, "{path}: {:?}", response);
        }
    }

    #[test]
    fn an_unknown_action_is_not_found_and_the_refusal_lists_the_five_that_exist() {
        let scratch = Scratch::new("life-action");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch);

        let unknown = router.respond(&post("/api/apps/notes/delete")).complete();
        assert_eq!(unknown.status, 404);
        assert!(
            body(&unknown).contains("create, build, start, stop or restart"),
            "{:?}",
            unknown
        );

        // A slug with no action at all is the same refusal one level up: it names the shape that
        // would have worked instead of leaving the caller to guess.
        let bare = router.respond(&post("/api/apps/notes")).complete();
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
        assert_eq!(router.respond(&request).complete().status, 401);
    }

    #[test]
    fn a_server_without_a_supervisor_says_so_instead_of_pretending() {
        let scratch = Scratch::new("life-none");
        scratch.built_app("notes", "Notes");
        // Listing only: this router can say which apps exist and cannot run one.
        let router = router().with_apps_dir(scratch.apps());
        let response = router.respond(&post("/api/apps/notes/start")).complete();
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

        let created = router
            .respond(&post_query("/api/apps/notes/create", "name=Notes"))
            .complete();
        assert_eq!(created.status, 201, "{:?}", created);
        let created_body = body(&created);
        assert!(
            created_body.contains("\"slug\":\"notes\""),
            "{created_body}"
        );
        assert!(created_body.contains("Cargo.toml"), "{created_body}");

        // The listing agrees with what the create said it wrote.
        let listing = body(&router.respond(&authorized_get("/api/apps")).complete());
        assert!(listing.contains("\"slug\":\"notes\""), "{listing}");
        assert!(listing.contains("\"name\":\"Notes\""), "{listing}");
    }

    #[test]
    fn a_create_decodes_the_name_it_was_given() {
        // The name arrives in the URL because this layer reads no request body; a space and a
        // non-ASCII letter have to survive the trip.
        let scratch = Scratch::new("create-encoded");
        let router = supervised(&scratch);
        let response = router
            .respond(&post_query(
                "/api/apps/notes/create",
                "name=My%20Notes%20%E2%80%94%20v2",
            ))
            .complete();
        assert_eq!(response.status, 201, "{:?}", response);

        let listing = body(&router.respond(&authorized_get("/api/apps")).complete());
        assert!(listing.contains("My Notes — v2"), "{listing}");
    }

    #[test]
    fn a_build_without_a_c_toolchain_is_refused_before_any_step_runs() {
        let scratch = Scratch::new("build-no-cc");
        scratch.built_app("notes", "Notes");
        let builder = Arc::new(FakeBuilder::succeeds());
        let response = supervised_with(&scratch, builder.clone())
            .with_preflight(Arc::new(CcSaying(deepnest_packaging::Finding::Missing {
                detail: "none of cc, gcc, clang is on PATH".to_string(),
            })))
            .respond(&post("/api/apps/notes/build"))
            .complete();

        // A precondition that is not met is not a build that failed: nothing was attempted, so
        // `412` says that and the caller can act on it without reading a build log.
        assert_eq!(response.status, 412, "{:?}", response);
        assert!(
            builder.steps().is_empty(),
            "the plan must not start without a C compiler: {:?}",
            builder.steps()
        );

        let answer = body(&response);
        assert!(
            answer.contains(deepnest_packaging::requirement().name),
            "{answer}"
        );
        assert!(
            answer.contains("none of cc, gcc, clang"),
            "the report has to say what the probe saw: {answer}"
        );
        // The promise the README makes is the fix, printed where the user will see it.
        assert!(
            answer.contains(deepnest_packaging::requirement().fix),
            "the refusal has to walk the user through installing it: {answer}"
        );
    }

    #[test]
    fn a_build_whose_toolchain_check_could_not_be_answered_is_refused_too() {
        let scratch = Scratch::new("build-unknown-cc");
        scratch.built_app("notes", "Notes");
        let builder = Arc::new(FakeBuilder::succeeds());
        let response = supervised_with(&scratch, builder.clone())
            .with_preflight(Arc::new(CcSaying(
                deepnest_packaging::Finding::Undetermined {
                    why: "`xcode-select -p` could not be run".to_string(),
                },
            )))
            .respond(&post("/api/apps/notes/build"))
            .complete();

        // Fail closed: "cannot tell" is not "yes", and a build started on it would spend a minute
        // arriving at the same refusal from a compiler instead of from this check.
        assert_eq!(response.status, 412, "{:?}", response);
        assert!(
            builder.steps().is_empty(),
            "an unanswerable check must not let the plan start: {:?}",
            builder.steps()
        );
        let answer = body(&response);
        assert!(answer.contains("could not tell"), "{answer}");
        assert!(answer.contains("xcode-select -p"), "{answer}");
    }

    #[test]
    fn creating_an_app_that_exists_is_a_conflict_and_changes_nothing() {
        let scratch = Scratch::new("create-taken");
        scratch.app("notes", "Notes");
        let before =
            std::fs::read_to_string(scratch.apps().join("notes").join("deepnest.toml")).unwrap();

        let response = supervised(&scratch)
            .respond(&post_query("/api/apps/notes/create", ""))
            .complete();
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
        let response = supervised_with(&scratch, builder.clone())
            .respond(&post("/api/apps/notes/build"))
            .complete();

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
            .respond(&post("/api/apps/notes/build"))
            .complete();

        assert_eq!(response.status, 500, "{:?}", response);
        let answer = body(&response);
        assert!(answer.contains("bun install"), "{answer}");
        assert!(answer.contains("notes.build.log"), "{answer}");
    }

    #[test]
    fn building_an_app_that_is_not_there_is_not_found() {
        let scratch = Scratch::new("build-missing");
        let response = supervised(&scratch)
            .respond(&post("/api/apps/ghost/build"))
            .complete();
        assert_eq!(response.status, 404, "{:?}", response);
    }

    #[test]
    fn a_directory_that_is_not_an_app_cannot_be_built() {
        // A directory with no manifest is the wrong directory, not a build that failed.
        let scratch = Scratch::new("build-not-app");
        std::fs::create_dir_all(scratch.apps().join("junk")).unwrap();
        let response = supervised(&scratch)
            .respond(&post("/api/apps/junk/build"))
            .complete();
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
            assert_eq!(router.respond(&request).complete().status, 401, "{path}");
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
        let page = body(&router.respond(&get("/", "127.0.0.1:4270")).complete());
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>x</script>"));
    }

    // --- the panel's script, and the config it writes ----------------------------------------

    #[test]
    fn the_panel_is_served_with_a_policy_that_allows_only_its_own_script() {
        let response = router().respond(&get("/", "127.0.0.1:4270")).complete();
        assert_eq!(response.status, 200);
        let policy = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-security-policy"))
            .map(|(_, value)| value.as_str())
            .expect("the panel is served without a policy");
        assert!(policy.contains("script-src 'self'"), "{policy}");
        assert!(!policy.contains("script-src 'unsafe-inline'"), "{policy}");
    }

    #[test]
    fn the_script_the_page_asks_for_is_a_route_that_answers_as_a_script() {
        // A policy of `script-src 'self'` refuses the script if its type is wrong, and the panel
        // would then be a page that renders nothing.
        let response = router()
            .respond(&get("/panel.js", "127.0.0.1:4270"))
            .complete();
        assert_eq!(response.status, 200);
        assert!(
            response.content_type.starts_with("text/javascript"),
            "{:?}",
            response.content_type
        );
        assert!(body(&response).contains("deepnest-token"));
    }

    #[test]
    fn the_panel_carries_the_token_its_script_sends_back() {
        // The page is not behind the token — a browser opening the address has none — so the page
        // carries it, and this is the test that says the two spellings agree.
        let page = body(&router().respond(&get("/", "127.0.0.1:4270")).complete());
        let expected = format!(
            r#"<meta name="deepnest-token" content="{}">"#,
            "a".repeat(64)
        );
        assert!(
            page.contains(&expected),
            "the page does not carry the token"
        );
    }

    /// A request that carries the token and a body.
    ///
    /// Built through [`crate::http::read`] and not [`parse`]: `parse` sees a head and nothing else,
    /// and a request assembled from a head alone has no body — which is exactly the case a route
    /// that writes a file must not be tested with.
    fn post_body(path: &str, body: &str) -> Request {
        let wire = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            "a".repeat(64),
            body.len(),
            body
        );
        let mut reader = wire.as_bytes();
        crate::http::read(&mut reader).unwrap()
    }

    /// What the first-run form sends.
    const FORM: &str = r#"{"base_url":"https://api.deepseek.com","api_key":"sk-from-the-form","model":"deepseek-flash"}"#;

    /// A router that starts unconfigured and may write its config into `scratch`.
    fn first_run(scratch: &Scratch) -> Router {
        Router::new(PORT, None, Some(Token::parse(&"a".repeat(64)).unwrap()))
            .with_config_path(scratch.base.join(crate::config::FILE_NAME))
    }

    #[test]
    fn the_first_run_form_writes_the_config_the_running_process_then_reports() {
        let scratch = Scratch::new("config-save");
        let path = scratch.base.join(crate::config::FILE_NAME);
        let router = first_run(&scratch);

        assert!(!path.exists(), "a scratch home starts unconfigured");
        let before = body(&router.respond(&authorized_get("/api/config")).complete());
        assert!(before.contains("\"configured\":false"), "{before}");

        let saved = router.respond(&post_body("/api/config", FORM)).complete();
        assert_eq!(saved.status, 200, "{:?}", saved);
        let answer = body(&saved);
        assert!(answer.contains("\"configured\":true"), "{answer}");
        assert!(
            !answer.contains("sk-from-the-form"),
            "the key was echoed back: {answer}"
        );

        // The file, with the mode that keeps other users out of the key.
        let config = crate::config::load(&path)
            .unwrap()
            .expect("a config was written");
        assert_eq!(config.llm.api_key, "sk-from-the-form");
        assert_eq!(config.model(), "deepseek-flash");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the API key is in that file");
        }

        // No restart: both routes read it back from the process, not from the file.
        let after = body(&router.respond(&authorized_get("/api/config")).complete());
        assert!(after.contains("\"configured\":true"), "{after}");
        assert!(after.contains("deepseek-flash"), "{after}");
        let health = body(
            &router
                .respond(&get("/api/health", "127.0.0.1:4270"))
                .complete(),
        );
        assert!(
            health.contains("\"model_endpoint_configured\":true"),
            "{health}"
        );
        let page = body(&router.respond(&get("/", "127.0.0.1:4270")).complete());
        assert!(page.contains("configured (deepseek-flash)"), "{page}");
    }

    #[test]
    fn a_body_sent_to_a_route_that_reads_none_is_refused_rather_than_routed() {
        // `POST /api/config` is the one route that reads a body. Routing any other with a body would
        // answer "done" to a request whose content this server never looked at.
        let response = router()
            .respond(&post_body("/api/apps/notes/start", "anything"))
            .complete();
        assert_eq!(response.status, 400, "{:?}", response);
        assert!(
            body(&response).contains("/api/config"),
            "{}",
            body(&response)
        );
    }

    #[test]
    fn a_config_that_would_not_boot_is_refused_and_nothing_is_written() {
        // The file is written through `config::save`, so the checks that stop the binary at startup
        // stop the form instead. A form that left behind a config the binary refuses to boot on
        // would be worse than a form that said no.
        let scratch = Scratch::new("config-refused");
        let path = scratch.base.join(crate::config::FILE_NAME);
        let refusal = first_run(&scratch)
            .respond(&post_body(
                "/api/config",
                r#"{"base_url":"api.deepseek.com","api_key":"sk-x","model":"m"}"#,
            ))
            .complete();
        assert_eq!(refusal.status, 400, "{:?}", refusal);
        assert!(
            body(&refusal).contains("llm.base_url"),
            "{}",
            body(&refusal)
        );
        assert!(!path.exists(), "a refused config was written anyway");
    }

    #[test]
    fn a_config_body_that_is_not_what_the_route_reads_is_a_bad_request() {
        let scratch = Scratch::new("config-body");
        let router = first_run(&scratch);
        for body in [
            "",
            "not json",
            "{}",
            // A misspelled field is a typo, not an absent value: reading "no key" from a body that
            // plainly carries one would report success for a config that cannot work.
            r#"{"base_url":"https://x","apikey":"k","model":"m"}"#,
        ] {
            let response = router.respond(&post_body("/api/config", body)).complete();
            assert_eq!(response.status, 400, "{body}: {:?}", response);
        }
        assert!(!scratch.base.join(crate::config::FILE_NAME).exists());
    }

    #[test]
    fn the_config_route_needs_the_token_and_a_method_it_has() {
        let scratch = Scratch::new("config-method");
        let router = first_run(&scratch);

        let anonymous =
            parse("POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nContent-Length: 0\r\n")
                .unwrap();
        assert_eq!(router.respond(&anonymous).complete().status, 401);

        assert_eq!(
            router
                .respond(&authorized_get("/api/config"))
                .complete()
                .status,
            200
        );

        // A `DELETE` would be a way to remove the endpoint by accident; the refusal names the two
        // methods that exist rather than leaving the caller to guess.
        let delete = parse(&format!(
            "DELETE /api/config HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\n",
            "a".repeat(64)
        ))
        .unwrap();
        let response = router.respond(&delete).complete();
        assert_eq!(response.status, 405);
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| *name == "Allow" && *value == "GET, POST"));
    }

    #[test]
    fn a_process_with_no_config_path_says_so_instead_of_saving_into_the_void() {
        let response = router().respond(&post_body("/api/config", FORM)).complete();
        assert_eq!(response.status, 503, "{:?}", response);
        assert!(
            body(&response).contains("config path"),
            "{}",
            body(&response)
        );
    }

    #[test]
    fn the_config_route_never_returns_the_key_it_stored() {
        // The page carries the control token on purpose. The model key is a different secret and
        // nothing this server answers has a use for it.
        let scratch = Scratch::new("config-key");
        let router = first_run(&scratch);
        // The save's own answer, first: it is the one response written by the code that has the key
        // in hand, so it is the one most likely to leak it.
        let saved = router.respond(&post_body("/api/config", FORM)).complete();
        assert_eq!(saved.status, 200, "{:?}", saved);
        assert!(
            !body(&saved).contains("sk-from-the-form"),
            "the save answer returned the key: {}",
            body(&saved)
        );
        for request in [
            authorized_get("/api/config"),
            get("/api/health", "127.0.0.1:4270"),
            get("/", "127.0.0.1:4270"),
        ] {
            let answer = body(&router.respond(&request).complete());
            assert!(
                !answer.contains("sk-from-the-form"),
                "{} returned the key",
                request.path
            );
        }
    }
}
