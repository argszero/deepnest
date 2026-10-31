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
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use deepnest_apps::{
    BuildError, Runner, Slug, StartError, State, StdRunner, Supervisor, TemplateError,
};
use serde_json::{json, Value};

use deepnest_gateway::{Answer, ForwardError, Forwarder, HttpTransport, StdForwarder, Target};
use deepnest_packaging::{Probe, StdProbe};
use deepnest_web::Panel;

use crate::chat;
use crate::escalation;
use crate::grant;
use crate::http::{Method, Request, Response};
use crate::model::Endpoint;
use crate::rebuild;
use crate::refresh;
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
    /// The rebuild queue: what a writing turn starts, and what the page polls until the app serves
    /// the change. Held here because it outlives any one request — a queue rebuilt per request
    /// would forget the build that is already running and start a second one beside it.
    pub rebuild: Arc<rebuild::Rebuilder>,
    /// Where the update check asks the forge. On the lifecycle so a test can point it at a
    /// loopback server: `Github::new` reaches `api.github.com`, and a route test that called it
    /// would be measuring GitHub's answer today instead of this crate's wiring.
    pub releases: Arc<dyn deepnest_skills::Releases>,
    /// How a skill is published. A trait object for the reason `builder` is one: the real publisher
    /// runs the package the catalog names, which is a program this machine may not have.
    pub publisher: Arc<dyn deepnest_skills::Publisher>,
    /// The refresh queue: what a person asked for from the panel, and the version it adopted. Held
    /// here for the reason `rebuild` is — a queue rebuilt per request would forget the refresh that
    /// is already running and start a second writer on the same state file.
    pub refresher: Arc<refresh::Refresher>,
}

impl Lifecycle {
    /// The lifecycle the binary uses: the real supervisor and the real build runner.
    pub fn new(supervisor: Arc<Supervisor>, home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let builder: Arc<dyn Runner> = Arc::new(StdRunner::new());
        let releases: Arc<dyn deepnest_skills::Releases> = Arc::new(crate::updates::Github::new());
        let publisher: Arc<dyn deepnest_skills::Publisher> =
            Arc::new(deepnest_skills::StdPublisher);
        Self {
            refresher: Arc::new(refresh::Refresher::new(
                home.clone(),
                Arc::clone(&publisher),
                Arc::clone(&releases),
            )),
            releases,
            publisher,
            rebuild: Arc::new(rebuild::Rebuilder::new(
                Arc::clone(&supervisor),
                Arc::clone(&builder),
                deepnest_apps::apps_dir(&home),
                home.clone(),
            )),
            supervisor,
            builder,
            forwarder: Arc::new(StdForwarder),
            preflight: Arc::new(StdProbe),
            home,
        }
    }

    /// The same lifecycle with a different forwarder.
    pub fn with_forwarder(mut self, forwarder: Arc<dyn Forwarder>) -> Self {
        self.forwarder = forwarder;
        self
    }

    /// The same lifecycle with a different answer to "is there a newer release".
    ///
    /// The same reason [`Lifecycle::with_builder`] exists: what a test has to control is the answer,
    /// and the real one is a request to somebody else's server today.
    ///
    /// The refresh queue is rebuilt with it, for the reason [`Lifecycle::with_builder`] gives about
    /// the build queue: a check swapped into the lifecycle but not into the thing that records what
    /// a check adopted would have the route answer with one forge and the refresh record another.
    pub fn with_releases(mut self, releases: Arc<dyn deepnest_skills::Releases>) -> Self {
        self.refresher = Arc::new(refresh::Refresher::new(
            self.home.clone(),
            Arc::clone(&self.publisher),
            Arc::clone(&releases),
        ));
        self.releases = releases;
        self
    }

    /// The same lifecycle with a different publisher.
    ///
    /// The one seam a test has to replace to refresh a skill at all: the real publisher runs a
    /// program on this machine, and what that program does is not this crate's to measure.
    pub fn with_publisher(mut self, publisher: Arc<dyn deepnest_skills::Publisher>) -> Self {
        self.refresher = Arc::new(refresh::Refresher::new(
            self.home.clone(),
            Arc::clone(&publisher),
            Arc::clone(&self.releases),
        ));
        self.publisher = publisher;
        self
    }

    /// The same lifecycle with a different build runner.
    ///
    /// The queue is rebuilt with it: a runner swapped into the lifecycle but not into the thing
    /// that builds after a turn would answer the panel's button with one toolchain and a writing
    /// turn with another, and the test that chose a fake would then measure the real one.
    pub fn with_builder(mut self, builder: Arc<dyn Runner>) -> Self {
        self.rebuild = Arc::new(rebuild::Rebuilder::new(
            Arc::clone(&self.supervisor),
            Arc::clone(&builder),
            deepnest_apps::apps_dir(&self.home),
            self.home.clone(),
        ));
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
    /// What a page without the control token may hold. Always present, because it needs no file:
    /// minting into it is what makes the app page's dialog able to send a message at all.
    grants: grant::Grants,
    /// The widening requests a turn is waiting on, and the pages answer. Always present for the
    /// same reason `grants` is: the dialog is where an approval is given, and a router that could
    /// not hold a request would answer every one of them with "nobody to ask".
    requests: escalation::Requests,
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
            grants: grant::Grants::new(),
            requests: escalation::Requests::new(),
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

    /// Answer the update check with `releases` instead of asking the forge.
    ///
    /// The same shape as the two around it, and for the same reason: the lifecycle this router
    /// already holds is the one that changes, so a test that calls two of these builders does not
    /// silently lose the first.
    pub fn with_releases(mut self, releases: Arc<dyn deepnest_skills::Releases>) -> Self {
        if let Some(lifecycle) = self.lifecycle.take() {
            self.lifecycle = Some(lifecycle.with_releases(releases));
        }
        self
    }

    /// Publish skills with `publisher` instead of running the package.
    ///
    /// The same shape as the builders around it, and the same reason: what a test has to control is
    /// the program's answer, and the real one is somebody else's binary on this machine.
    pub fn with_publisher(mut self, publisher: Arc<dyn deepnest_skills::Publisher>) -> Self {
        if let Some(lifecycle) = self.lifecycle.take() {
            self.lifecycle = Some(lifecycle.with_publisher(publisher));
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

        // Four routes read a body, and every other route refuses one by name. Reading it and
        // then routing anyway would answer "done" to a request whose content was never looked at,
        // which is the one answer a server must not give about something it did not read.
        if !request.body.is_empty() && !takes_a_body(path) {
            return Answer::Full(Response::text(
                400,
                format!(
                    "{path} takes no request body — send none, or send one to {CONFIG_PATH}, \
                     {SKILL_REFRESH}, POST /api/apps/<slug>/{} or POST /api/apps/<slug>/{}\n",
                    chat::ACTION,
                    escalation::ACTION
                ),
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
            SKILL_UPDATES => {
                if !self.authorized(request) {
                    return Answer::Full(unauthorized());
                }
                Answer::Full(self.skill_updates(request))
            }
            SKILL_REFRESH => {
                if !self.authorized(request) {
                    return Answer::Full(unauthorized());
                }
                Answer::Full(self.skill_refresh(request))
            }
            other if other.starts_with("/api/apps/") => {
                // Two ways in, and only one of them is a general credential: the control token
                // opens every route, and an app page's grant opens exactly one. Anything that is
                // not that one route stays behind the token.
                if !self.authorized(request) && !self.grant_admits(request) {
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

    /// Does the request carry a grant, and is it this app's route of the same name it asks for?
    ///
    /// The path is compared against [`grant::path`] — the same spelling that names the cookie —
    /// rather than re-derived here, so there is one place in the codebase that decides which route
    /// a capability opens, and [`grant::cookie_name`] decides which actions have one at all. A
    /// grant is never a credential for anything else: not the app's other verbs, not another app,
    /// not the config. The value must also have been minted for *this* action, so the cookie that
    /// sends a message cannot answer a widening request.
    fn grant_admits(&self, request: &Request) -> bool {
        // A grant opens the verb the page needs and not the route in every verb it has. `build`
        // is the one route whose other verb spends a compiler: the dialog watches the rebuild a
        // turn started, and starting one is the panel's, behind the token.
        if request.path.ends_with(&format!("/{}", rebuild::ACTION)) && request.method != Method::Get
        {
            return false;
        }
        let Some(rest) = request.path.strip_prefix("/api/apps/") else {
            return false;
        };
        let Some((slug_text, action)) = rest.split_once('/') else {
            return false;
        };
        let Ok(slug) = Slug::parse(slug_text) else {
            return false;
        };
        let Some(name) = grant::cookie_name(action) else {
            return false;
        };
        if request.path != grant::path(slug.as_str(), action) {
            return false;
        }
        let Some(value) = grant::presented(request, name) else {
            return false;
        };
        self.grants.admits(slug.as_str(), action, value)
    }

    /// Put a grant on an app's page, so its dialog can send a message.
    ///
    /// Only an HTML answer: the dialog is injected into pages, so a page that is not one has no box
    /// to send from, and a cookie minted for it would be a capability nobody uses. A page that
    /// already presents a grant it can still use keeps it — minting again on every load would
    /// churn the store and hand a second tab's value to the first.
    fn with_a_dialog_grant(&self, request: &Request, slug: &Slug, response: Response) -> Response {
        if !response.content_type.starts_with("text/html") {
            return response;
        }
        // One cookie per route the dialog may reach, and a value per cookie: the capability to
        // send a message must not be the capability to approve a widening, or to watch a build.
        // All three are minted here, because serving the page is the moment this server can tell
        // "this browser loaded the app from here" from "something else on this machine asked".
        let mut response = response;
        for action in [chat::ACTION, escalation::ACTION, rebuild::ACTION] {
            let name =
                grant::cookie_name(action).expect("every route the dialog may reach has one");
            let presented = grant::presented(request, name)
                .is_some_and(|value| self.grants.admits(slug.as_str(), action, value));
            // A page that already presents a grant it can still use keeps it — minting again on
            // every load would churn the store and hand a second tab's value to the first.
            if presented {
                continue;
            }
            // This machine's random source failed: the page still renders and the ball still
            // opens, and the dialog declines with a reason. Inventing a value would be worse.
            if let Some(value) = self.grants.mint(slug.as_str(), action) {
                response = response.header(
                    "Set-Cookie",
                    grant::cookie(name, slug.as_str(), action, &value),
                );
            }
        }
        response
    }

    /// The panel: `crates/web`'s page, carrying what only this process knows.
    ///
    /// The port, the control token, the endpoint state, a line about the apps directory and the
    /// skills report are rendered into the markup, so a reader with script off — or with a script
    /// that failed to load — still gets a page that is true. The app list itself is the script's
    /// job: it changes whenever a directory does, and rendering it here would make it stale between
    /// requests. The skills report goes the other way and is rendered, because a user edits a skill
    /// file and then looks at this page — the round trip is the part that would hide the edit.
    fn panel(&self) -> String {
        let skills = self.skills_report();
        deepnest_web::page(&Panel {
            port: self.port,
            token: self
                .token
                .as_ref()
                .map(|token| token.as_str().to_string())
                .unwrap_or_default(),
            model: self.model(),
            apps: self.apps_summary(),
            skills: skills.summary,
            skill_list: skills.found,
            skill_problems: skills.skipped,
            skill_offered: skills.offered,
            catalog_problems: skills.catalog_problems,
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

    /// What the panel says about the skills on disk.
    ///
    /// `None` for the app, and that is the scope worth stating rather than assuming: this page
    /// belongs to no app, so its list is the user's skills only. A skill living inside an app is
    /// invisible here by design, which the page has to say out loud — otherwise "my app's skill
    /// does not show up" is the next bug report about a deliberate read.
    ///
    /// The loader cannot fail loudly: a root that cannot be read comes back as a `skipped` entry
    /// with its reason, and that entry is exactly what the second list is for.
    fn skills_report(&self) -> SkillsReport {
        let Some(home) = self
            .lifecycle
            .as_ref()
            .map(|lifecycle| lifecycle.home.as_path())
        else {
            return SkillsReport {
                summary: "this process was started without a deepnest home".to_string(),
                found: Vec::new(),
                skipped: Vec::new(),
                offered: Vec::new(),
                catalog_problems: Vec::new(),
            };
        };

        let root = deepnest_skills::user_root(home);
        let shown = root.display().to_string();
        let loaded = deepnest_skills::load(home, None);
        let skills = loaded.skills.len();
        let skipped = loaded.skipped.len();

        // What the catalog offers and this home does not have. Read here rather than behind a route
        // of its own because it is a file on disk, like the list above it: the facts that cost a
        // network, or run a program, are the ones that wait for the page's script. A skill nobody
        // has been told about is one nobody can ask for, and the catalog ships for exactly that.
        let on_disk: Vec<String> = loaded
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
        let catalog = deepnest_skills::catalog::load(&root);
        let offered = catalog
            .entries
            .iter()
            .filter(|entry| !on_disk.contains(&entry.name))
            .map(|entry| deepnest_web::Offered {
                name: entry.name.clone(),
                description: entry.description.clone(),
                why_not: not_fetchable(entry),
            })
            .collect();

        SkillsReport {
            summary: match (skills, skipped) {
                // Sorted by name by the loader, so the page and the prompt list skills in the
                // same order and a user comparing the two is not left wondering about a diff.
                (0, 0) => format!("none yet in {shown}"),
                (0, _) => format!("none usable in {shown}"),
                (count, _) => format!("{count} {} in {shown}", plural(count, "skill", "skills")),
            },
            found: loaded
                .skills
                .into_iter()
                .map(|skill| (skill.name, skill.description))
                .collect(),
            skipped: loaded
                .skipped
                .into_iter()
                .map(|skipped| (skipped.path.display().to_string(), skipped.reason))
                .collect(),
            offered,
            catalog_problems: catalog.rejected.iter().map(ToString::to_string).collect(),
        }
    }

    /// `GET /api/skills/updates` — whether a newer release of any installed skill is published.
    ///
    /// A route of its own rather than a line in [`Router::skills_report`], and that is the whole
    /// design decision here. The skills report reads the disk and is rendered into the markup, which
    /// is right for a list that changes only when the user edits a file. An update check is the
    /// opposite kind of fact: one request per skill to somebody else's server, which on an offline
    /// machine takes until it times out. Rendering that would make the panel — the product's only
    /// interface — wait on a network it does not need, and the page can have the local answer now
    /// and this one a moment later.
    ///
    /// `GET` only: it asks a question and changes nothing. The refresh is a route of its own —
    /// [`SKILL_REFRESH`] — because a `POST` here would put a program that runs on the user's machine
    /// behind the route the page calls on load.
    fn skill_updates(&self, request: &Request) -> Response {
        if !matches!(request.method, Method::Get | Method::Head) {
            return Response::json(
                405,
                error_body("the update check is read with GET: it asks the forge a question and changes nothing here"),
            );
        }
        let Some(lifecycle) = self.lifecycle.as_ref() else {
            return Response::json(
                503,
                error_body("this process was started without a deepnest home, so there are no skills to check"),
            );
        };
        let rows = crate::updates::report(&lifecycle.home, lifecycle.releases.as_ref());
        Response::json(200, crate::updates::body(&rows).to_string())
    }

    /// `POST /api/skills/refresh` — rewrite one managed skill from its own package and record the
    /// version the release check adopted with it; `GET` reports what the last refresh came to.
    ///
    /// `POST` and not `GET`, because this runs a program on the user's machine and replaces a file
    /// there: a browser prefetches links, and a side effect behind `GET` is one it makes by pointing
    /// at it. `202` and not `200`, because the answer is "it started" — the publisher returns when
    /// the package decides to, which is longer than a page holds a request — so the page polls this
    /// same route for the outcome. The refusal for a second request is `409` rather than a queue
    /// behind the first, and the reason is in [`crate::refresh`]: the state file is one file.
    fn skill_refresh(&self, request: &Request) -> Response {
        let Some(lifecycle) = self.lifecycle.as_ref() else {
            return Response::json(
                503,
                error_body(
                    "this process was started without a deepnest home, so there are no skills to \
                     refresh",
                ),
            );
        };
        match request.method {
            Method::Get | Method::Head => {
                let last = lifecycle.refresher.status();
                Response::json(
                    200,
                    json!({ "refresh": last.as_ref().map(refresh::Status::body) }).to_string(),
                )
            }
            Method::Post => {
                let Some(text) = request.body_text() else {
                    return Response::json(400, error_body("the refresh body must be UTF-8 JSON"));
                };
                let submitted: RefreshRequest = match serde_json::from_str(text) {
                    Ok(submitted) => submitted,
                    Err(error) => {
                        return Response::json(
                            400,
                            error_body(&format!(
                                "the refresh body is not the JSON this route reads (name): {error}"
                            )),
                        )
                    }
                };
                let name = submitted.name.trim();
                if name.is_empty() {
                    return Response::json(
                        400,
                        error_body("the refresh body names no skill — send {\"name\":\"…\"}"),
                    );
                }
                match lifecycle.refresher.enqueue(name) {
                    refresh::Started::Started => Response::json(
                        202,
                        json!({ "name": name, "state": "refreshing" }).to_string(),
                    ),
                    refresh::Started::Busy { name: other } => Response::json(
                        409,
                        error_body(&format!(
                            "a refresh of `{other}` is running; one at a time, because the state \
                             file is one file and two writers would lose one of the records"
                        )),
                    ),
                }
            }
            _ => Response::json(
                405,
                error_body("a refresh is asked for with POST and read with GET"),
            )
            .header("Allow", "GET, POST"),
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
    ///
    /// It is also **the one route that hands out a capability**: an HTML answer carries a cookie
    /// naming that app's chat route ([`crate::grant`]), which is what lets the dialog in the page
    /// send a message without the page ever holding the control token. Serving the page is the
    /// moment the server can tell "this browser loaded the app from here" from "something else on
    /// this machine asked", and that is why the grant is minted here rather than as its own route.
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
            // Already an answer: a body, or the connection the app is holding. The body is where the
            // dialog's capability goes — and a stream is not, because there is no page in it to put
            // a box on.
            Ok(Answer::Full(response)) => {
                Answer::Full(self.with_a_dialog_grant(request, &slug, response))
            }
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
        // `POST` and nothing else, for every verb whose job is to change something — a state
        // change behind `GET` is one a browser may make by prefetching a link, and a `405` naming
        // the method is what stops a client guessing. `escalation` is exempt because half of it is
        // a read: a page polls it for the question it is meant to answer.
        // `build` is exempt in `GET` for the same reason `escalation` is: half of it is a read.
        // `POST` builds now and answers when the build is over; `GET` reports what the rebuild a
        // writing turn started is doing, which is what the page polls without holding anything.
        let build_status_read = action == Action::Build && request.method == Method::Get;
        if request.method != Method::Post && action != Action::Escalation && !build_status_read {
            // `chat` is the one verb here that does not change a process, so it is the one that
            // must not claim to: what a caller needs to know is the method, and the wording only
            // has to be true.
            let what = if action == Action::Chat {
                "takes a message"
            } else {
                "changes state"
            };
            return Response::json(
                405,
                error_body(&format!(
                    "{action_text} {what}: POST /api/apps/{slug_text}/{action_text}"
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
        // Answered before the lifecycle is looked for: a widening request belongs to a turn, not
        // to the app's process, and a page whose app is stopped may still be the page that has to
        // say no to one.
        if action == Action::Escalation {
            return self.escalation_route(request, &slug);
        }

        let Some(lifecycle) = &self.lifecycle else {
            return Response::json(
                503,
                error_body(
                    "this process cannot create, build, start, stop or answer an app: it was \
                     started without a lifecycle",
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
            // Answered above, before the checks they must not be subject to.
            Action::Create => unreachable!("create is answered before this match"),
            Action::Escalation => unreachable!("escalation is answered before this match"),
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
            Action::Chat => self.chat(&request.body, &dir, &slug, lifecycle),
            Action::Build if request.method == Method::Get => {
                // The same route read rather than run: what the page polls after a turn reported
                // `changed`, so it can say "building" and then reload onto the new binary — or show
                // the compiler's last lines when there is no new binary to reload onto.
                Response::json(200, lifecycle.rebuild.status(&slug).body(&slug).to_string())
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

    /// `GET`/`POST /api/apps/<slug>/escalation` — the question a turn is waiting on, and the answer.
    ///
    /// One resource and two verbs, because they are two halves of one act: a page polls for a
    /// question it is there to ask a person, and posts the person's answer back. `GET` answers
    /// `{"pending": null}` when nothing is waiting — not a `404`, because the route exists and
    /// "nobody is asking right now" is what the page needs to know to stop polling.
    ///
    /// The page reaches this without the control token, through a cookie scoped to this path and
    /// this app ([`crate::grant`]), minted when the page was served. That is the same narrowing the
    /// chat route uses, kept to a second value so an app's app script cannot spend one as the other.
    fn escalation_route(&self, request: &Request, slug: &Slug) -> Response {
        match request.method {
            Method::Get | Method::Head => Response::json(200, self.pending_body(slug)),
            Method::Post => self.answer_escalation(&request.body, slug),
            other => Response::json(
                405,
                error_body(&format!(
                    "escalation reads with GET and answers with POST, not {}",
                    other.as_str()
                )),
            )
            .header("Allow", "GET, POST"),
        }
    }

    /// What is being asked of `slug` right now, in the shape the dialog reads.
    fn pending_body(&self, slug: &Slug) -> String {
        match self.requests.pending(slug.as_str()) {
            Some(pending) => json!({
                "pending": {
                    "app": slug.as_str(),
                    "from": pending.from().as_str(),
                    "to": pending.to().as_str(),
                    "justification": pending.justification(),
                }
            })
            .to_string(),
            None => json!({ "pending": Value::Null }).to_string(),
        }
    }

    /// The person's answer, turned into the wake-up the waiting turn is in.
    ///
    /// The reply names what was answered, not only that something was: an approval whose record
    /// does not say what it approved of is a record of nothing, and the dialog shows it either way.
    fn answer_escalation(&self, body: &[u8], slug: &Slug) -> Response {
        let text = match std::str::from_utf8(body) {
            Ok(text) => text,
            Err(_) => return Response::json(400, error_body("the body must be UTF-8 JSON")),
        };
        if text.trim().is_empty() {
            return Response::json(
                400,
                error_body(
                    "the body must be `{\"approve\": true}` or `{\"approve\": false}` — it was \
                     empty",
                ),
            );
        }
        let verdict: Verdict = match serde_json::from_str(text) {
            Ok(verdict) => verdict,
            Err(error) => {
                return Response::json(
                    400,
                    error_body(&format!(
                        "the body is not a verdict ({error}). Send `{{\"approve\": true}}` or \
                         `{{\"approve\": false, \"reason\": \"...\"}}`"
                    )),
                )
            }
        };
        let resolution = if verdict.approve {
            escalation::Resolution::Approved
        } else {
            escalation::Resolution::Denied(verdict.reason.clone())
        };
        match self.requests.answer(slug.as_str(), resolution) {
            Ok(answered) => Response::json(
                200,
                json!({
                    "answered": {
                        "app": slug.as_str(),
                        "from": answered.from().as_str(),
                        "to": answered.to().as_str(),
                        "justification": answered.justification(),
                        "approved": verdict.approve,
                        "reason": verdict.reason,
                    }
                })
                .to_string(),
            ),
            // Nothing waiting is a conflict with the page's own state rather than a bad request or
            // a missing app: the page answered a question that is no longer being asked, and the
            // one thing it must not do is believe the answer landed.
            Err(reason) => Response::json(409, error_body(&reason)),
        }
    }

    /// `POST /api/apps/<slug>/chat` — one message, answered by that app's own agent.
    ///
    /// Three refusals come before any turn starts, and each names a different next step: a body
    /// that is not a message, a process with no config file to read, and a config that has no model
    /// in it yet (`412`, because the first-run form is the fix and it is one request away).
    ///
    /// The turn itself is `crate::chat`'s; what is here is the half that only the router can do —
    /// reading the endpoint out of the file the panel writes, and turning a `Reply` into a
    /// response. The model is built per request rather than held: `POST /api/config` can change the
    /// endpoint at any moment, and an agent assembled at startup would answer the old one until the
    /// next start.
    fn chat(&self, body: &[u8], dir: &Path, slug: &Slug, lifecycle: &Lifecycle) -> Response {
        let message = match chat::message_from(body) {
            Ok(message) => message,
            Err(reason) => return Response::json(400, error_body(&reason)),
        };
        let Some(config_path) = &self.config_path else {
            return Response::json(
                503,
                error_body(
                    "this process has no config file to read: it was started without one, so \
                     there is no model endpoint to ask",
                ),
            );
        };
        let llm = match crate::config::load(config_path) {
            Ok(Some(config)) => config.llm,
            // First run, and the answer is the form rather than a fault.
            Ok(None) => {
                return Response::json(
                    412,
                    error_body(&format!(
                        "no model is configured yet — POST {CONFIG_PATH} with base_url, api_key \
                         and model, then send the message again"
                    )),
                )
            }
            Err(error) => {
                return Response::json(
                    500,
                    error_body(&format!("the config could not be read: {error}")),
                )
            }
        };

        // Both schemes: `http://` for a local model server, `https://` for anything else, verified
        // against this machine's own trust store by `crates/gateway::transport`.
        let model: Arc<dyn deepnest_agent::Model> = Arc::new(Endpoint::new(
            llm.base_url,
            llm.api_key,
            llm.model,
            Arc::new(HttpTransport::new()),
        ));
        // The approver comes from this router's own queue, so a request registered inside the
        // turn is the one `GET`/`POST .../escalation` reads and answers on another connection.
        let reply = chat::answer(
            model,
            &lifecycle.home,
            dir,
            slug,
            &message,
            Some(self.requests.approver(slug.as_str())),
            // A build that failed last time is part of what the model must know before it writes
            // again: without it the next turn adds code on top of a tree that does not compile.
            lifecycle.rebuild.note(slug),
        );
        starts_a_rebuild_if_the_app_changed(&reply.body, &lifecycle.rebuild, slug);
        Response::json(reply.status, reply.body.to_string())
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
    /// Send one message to the app's agent, in the body.
    Chat,
    /// Read, or answer, the widening request a turn is waiting on.
    ///
    /// The one app route that is not a `POST`: `GET` reports what is being asked (a page polling
    /// for a question), `POST` answers it. Not a state change to the app, and it needs no
    /// lifecycle — which is why it is answered before the routes that do.
    Escalation,
}

impl Action {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "create" => Some(Self::Create),
            rebuild::ACTION => Some(Self::Build),
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "restart" => Some(Self::Restart),
            chat::ACTION => Some(Self::Chat),
            escalation::ACTION => Some(Self::Escalation),
            _ => None,
        }
    }

    /// The verbs, for a refusal that tells the caller what would have worked.
    const VERBS: &str = "create, build, start, stop, restart, chat or escalation";
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

/// Whether this turn starts a rebuild — the seam between a turn and a build.
///
/// A function rather than two lines inside [`Router::chat`] because this is the rule the whole
/// loop turns on, and a rule buried in a sixty-line route is a rule no test reaches without a live
/// model. The turn returns now and the build happens on its own thread: holding one connection for
/// a minute of `bun install` is a request longer than any browser waits for, which is why the
/// rebuild was a second button until now.
fn starts_a_rebuild_if_the_app_changed(
    body: &Value,
    rebuild: &Arc<rebuild::Rebuilder>,
    slug: &Slug,
) -> bool {
    if body["changed"] != Value::Bool(true) {
        return false;
    }
    rebuild.enqueue(slug);
    true
}

/// The one answer for a `/api/` request that does not carry the token.
fn unauthorized() -> Response {
    Response::json(
        401,
        error_body("this endpoint needs the control token — it is in the deepnest home's deepnest.token file"),
    )
    .header("WWW-Authenticate", "Bearer realm=\"deepnest\"")
}

/// The routes that read a request body, and no others.
///
/// Three, for one reason: `/api/config` carries an API key, `chat` carries what the user typed, and
/// `escalation` carries the answer to a question a turn is waiting on. None belongs in a URL — it
/// is written into the browser's history and into whatever logs the URL. Everything else refuses a
/// body by name rather than routing past it, because reading a request's content and then ignoring
/// it is answering about something that was never looked at.
fn takes_a_body(path: &str) -> bool {
    path == CONFIG_PATH
        || path == SKILL_REFRESH
        || path
            .strip_prefix("/api/apps/")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, action)| action == chat::ACTION || action == escalation::ACTION)
}

const CONFIG_PATH: &str = "/api/config";

/// Where the update check answers.
///
/// A constant because two sides name it: this router and the panel's script, and a test in
/// `crates/web` that reads the script back.
pub const SKILL_UPDATES: &str = "/api/skills/updates";

/// Where a refresh is asked for, and where its outcome is read.
///
/// A constant for the reason [`SKILL_UPDATES`] is one: this router and the panel's script name it,
/// and a test in `crates/web` reads the script back.
pub const SKILL_REFRESH: &str = "/api/skills/refresh";

/// The body `POST /api/apps/<slug>/escalation` reads: the person's answer, and why when it is no.
///
/// `deny_unknown_fields` for the reason every body here has it: `approved` instead of `approve`
/// would otherwise be read as a body carrying no answer at all, and the page would be told it
/// answered when it had not.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Verdict {
    approve: bool,
    /// The host's own words, when they refused. Empty when they approved.
    #[serde(default)]
    reason: String,
}

/// The body `POST /api/skills/refresh` reads: which skill to rewrite.
///
/// `deny_unknown_fields` for the reason every body here has it: `skill` instead of `name` would
/// otherwise be read as a body naming nothing, and the page would be told it had asked for a
/// refresh of nothing at all.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRequest {
    name: String,
}

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
/// What the panel says about the skills on disk, in the three shapes the page renders.
///
/// Assembled by [`Router::skills_report`] and handed to `crates/web` as text: the page escapes what
/// it is given, and this crate is the one that knows where the skills live.
struct SkillsReport {
    /// One plain-text line: how many there are, or why there are none to show.
    summary: String,
    /// The skills the loader accepted, as `(name, description)`.
    found: Vec<(String, String)>,
    /// The entries it refused, as `(path, reason)`.
    skipped: Vec<(String, String)>,
    /// What the catalog offers that is not on disk here yet.
    offered: Vec<deepnest_web::Offered>,
    /// The catalog's own unusable lines, one sentence each.
    catalog_problems: Vec<String>,
}

/// Why deepnest will not fetch a catalog entry, or `None` when it will.
///
/// The three fields [`Entry::is_installable`] asks for, named one at a time: a hand-edited catalog
/// that dropped `repo` is told which field, which is the difference between a fixable line and a
/// mystery. The sentence is the page's, so it says what a person can do rather than what is false.
fn not_fetchable(entry: &deepnest_skills::Entry) -> Option<String> {
    if entry.is_installable() {
        return None;
    }
    let mut missing = Vec::new();
    if entry.repo.is_none() {
        missing.push("`repo`");
    }
    if entry.install.is_none() {
        missing.push("`install`");
    }
    if entry.dest.is_none() {
        missing.push("`dest`");
    }
    Some(format!(
        "it names no {}, so deepnest cannot fetch it — the files are yours to supply",
        missing.join(", no ")
    ))
}

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

    /// What the person on the other end actually reads: the `error` field, decoded.
    ///
    /// A refusal quotes the toolchain advice verbatim, and on Linux and Windows that advice names
    /// a workload in quotes — `"Desktop development with C++"`. Those arrive JSON-escaped, so
    /// comparing the raw body against the message it carries fails on exactly the platforms whose
    /// advice matters most.
    fn error_text(response: &Response) -> String {
        let raw = body(response);
        match serde_json::from_str::<Value>(&raw) {
            Ok(value) => value
                .get("error")
                .and_then(Value::as_str)
                .map_or(raw.clone(), ToString::to_string),
            Err(_) => raw,
        }
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

        /// A user-level skill: `~/.deepnest/skills/<name>/SKILL.md` under this scratch home.
        fn skill(&self, name: &str, description: &str) {
            let dir = deepnest_skills::user_root(&self.base).join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(deepnest_skills::SKILL_FILE),
                format!("---\nname: {name}\ndescription: {description}\n---\n\nBody.\n"),
            )
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
    fn the_panel_lists_the_skills_the_loader_found_and_not_an_apps_own() {
        // The page is the loader's own answer, rendered: no route, no fetch, nothing a reader has
        // to run. `None` for the app, because this page belongs to no app — and a skill inside one
        // must therefore be absent rather than listed under a heading that claims otherwise.
        let scratch = Scratch::new("skills-panel");
        scratch.skill("notes", "Use when writing a note");
        let own = deepnest_skills::app_root(&scratch.apps().join("notes")).join("apponly");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(
            own.join(deepnest_skills::SKILL_FILE),
            "---\nname: apponly\ndescription: an app's own\n---\n\nBody.\n",
        )
        .unwrap();
        let router = supervised(&scratch);

        // The count and the directory it counts, because the line a user acts on names the path
        // they have to open.
        let root = deepnest_skills::user_root(&scratch.base)
            .display()
            .to_string();
        let page = body(&router.respond(&get("/", "127.0.0.1:4270")).complete());
        assert!(page.contains(&format!("1 skill in {root}")), "{page}");
        assert!(page.contains("<code>notes</code>"), "{page}");
        assert!(page.contains("Use when writing a note"), "{page}");
        assert!(!page.contains("apponly"), "{page}");
    }

    #[test]
    fn the_panel_reports_a_skill_that_could_not_be_used_beside_the_ones_that_could() {
        // The reading this exists to keep separate: "no skills yet" and "your skill has a typo"
        // are different pages, and a user with a broken header needs to see the one that loaded.
        let scratch = Scratch::new("skills-broken");
        scratch.skill("notes", "Use when writing a note");
        // A directory with no `SKILL.md` is the shape the loader cannot use.
        std::fs::create_dir_all(deepnest_skills::user_root(&scratch.base).join("broken")).unwrap();
        let router = supervised(&scratch);

        let page = body(&router.respond(&get("/", "127.0.0.1:4270")).complete());
        assert!(
            page.contains("1 entry under the skills root is not a skill"),
            "{page}"
        );
        assert!(page.contains("broken"), "{page}");
        assert!(page.contains("<code>notes</code>"), "{page}");
    }

    #[test]
    fn the_panel_says_when_it_has_no_home_to_read_skills_from() {
        // A router built without a lifecycle cannot answer this, and says so rather than reporting
        // an empty list — "no skills" and "nowhere to look" are not the same claim.
        let page = body(&router().respond(&get("/", "127.0.0.1:4270")).complete());
        assert!(page.contains("without a deepnest home"), "{page}");
    }

    #[test]
    fn the_panel_of_a_home_with_no_skills_says_there_are_none_yet() {
        let scratch = Scratch::new("skills-none");
        let page = body(
            &supervised(&scratch)
                .respond(&get("/", "127.0.0.1:4270"))
                .complete(),
        );
        // The whole line, not the phrase: the apps line says "none yet in" too, so an assertion
        // matching either would pass whichever one the code got wrong.
        assert!(page.contains("Skills: none yet in"), "{page}");
        assert!(!page.contains("Skills: none usable"), "{page}");
        assert!(!page.contains("is not a skill"), "{page}");
    }

    #[test]
    fn a_skills_root_where_nothing_could_be_used_says_that_and_not_none_yet() {
        // What the two lists exist for: a user whose only skill is broken must not be told they
        // have no skills, and must not be told their skill is fine either.
        let scratch = Scratch::new("skills-only-broken");
        std::fs::create_dir_all(deepnest_skills::user_root(&scratch.base).join("broken")).unwrap();
        let page = body(
            &supervised(&scratch)
                .respond(&get("/", "127.0.0.1:4270"))
                .complete(),
        );
        assert!(page.contains("Skills: none usable in"), "{page}");
        assert!(!page.contains("Skills: none yet in"), "{page}");
        assert!(
            page.contains("1 entry under the skills root is not a skill"),
            "{page}"
        );
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
    fn an_unknown_action_is_not_found_and_the_refusal_lists_the_seven_that_exist() {
        let scratch = Scratch::new("life-action");
        scratch.built_app("notes", "Notes");
        let router = supervised(&scratch);

        let unknown = router.respond(&post("/api/apps/notes/delete")).complete();
        assert_eq!(unknown.status, 404);
        assert!(
            body(&unknown).contains("create, build, start, stop, restart, chat or escalation"),
            "{:?}",
            unknown
        );

        // A slug with no action at all is the same refusal one level up: it names the shape that
        // would have worked instead of leaving the caller to guess.
        let bare = router.respond(&post("/api/apps/notes")).complete();
        assert_eq!(bare.status, 404);
        assert!(
            body(&bare).contains("create, build, start, stop, restart, chat or escalation"),
            "{:?}",
            bare
        );
    }

    #[test]
    fn the_chat_route_needs_the_token_like_every_other_api_route() {
        let scratch = Scratch::new("chat-token");
        scratch.app("notes", "Notes");
        let router = supervised(&scratch);
        let payload = r#"{"message":"hi"}"#;
        let wire = format!(
            "POST /api/apps/notes/chat HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let mut reader = wire.as_bytes();
        let request = crate::http::read(&mut reader).unwrap();

        // A message is the user's own text and the answer quotes their app: reaching the port is
        // not enough to spend a model call on it.
        assert_eq!(router.respond(&request).complete().status, 401);
    }

    #[test]
    fn a_message_says_the_model_is_not_configured_before_it_says_anything_else() {
        let scratch = Scratch::new("chat-first-run");
        scratch.app("notes", "Notes");
        // The config path is set and the file is not there: first run, and the answer is the form
        // rather than a fault. `412` and not `500`, because nothing here is broken.
        let router = supervised(&scratch).with_config_path(scratch.base.join("config.toml"));

        let response = router
            .respond(&post_body(
                "/api/apps/notes/chat",
                r#"{"message":"what is on my list?"}"#,
            ))
            .complete();
        assert_eq!(response.status, 412, "{:?}", response);
        assert!(
            error_text(&response).contains("POST /api/config"),
            "{:?}",
            response
        );
    }

    #[test]
    fn the_routes_that_read_a_body_are_named_and_every_other_route_refuses_one() {
        let scratch = Scratch::new("chat-body");
        scratch.app("notes", "Notes");
        let router = supervised(&scratch).with_config_path(scratch.base.join("config.toml"));

        // Past the body check and on to the agent: the config refusal is how this test knows the
        // body was accepted rather than dropped at the door.
        let chat = router
            .respond(&post_body("/api/apps/notes/chat", r#"{"message":"hi"}"#))
            .complete();
        assert_eq!(chat.status, 412, "{:?}", chat);

        // The same body on a route that does not read one is refused by name — answering "done"
        // to content nobody looked at is the answer a server must not give.
        let start = router
            .respond(&post_body("/api/apps/notes/start", r#"{"message":"hi"}"#))
            .complete();
        assert_eq!(start.status, 400, "{:?}", start);
        assert!(
            error_text(&start).contains("takes no request body"),
            "{:?}",
            start
        );
        // And it names every route that does read one — three of them. A hint listing two of
        // three is a hint that sends the caller to a route which will refuse it.
        for named in [CONFIG_PATH, chat::ACTION, escalation::ACTION] {
            assert!(
                error_text(&start).contains(named),
                "the refusal does not name {named}: {:?}",
                start
            );
        }
    }

    #[test]
    fn a_chat_sent_as_a_get_is_told_the_method_it_needs() {
        let scratch = Scratch::new("chat-method");
        scratch.app("notes", "Notes");
        let router = supervised(&scratch);

        let request = parse(&format!(
            "GET /api/apps/notes/chat HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\
             authorization: bearer {}\r\n",
            "a".repeat(64)
        ))
        .unwrap();
        let response = router.respond(&request).complete();
        assert_eq!(response.status, 405, "{:?}", response);
        assert!(
            response
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("allow") && value == "POST"),
            "{:?}",
            response
        );
        // `chat` does not change a process, so it must not claim to: a message saying it "changes
        // state" would be the wrong reassurance about what a `POST` here does.
        assert!(
            error_text(&response).contains("takes a message"),
            "{:?}",
            response
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

        let answer = error_text(&response);
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
        let answer = error_text(&response);
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

    // --- The dialog's capability -------------------------------------------------------------

    /// A forwarder that answers with JSON rather than a page, for the test that says no grant is
    /// minted where there is no page to put a box on.
    struct JsonAnswering;

    impl Forwarder for JsonAnswering {
        fn forward(&self, _target: &Target, _request: &Request) -> Result<Answer, ForwardError> {
            Ok(Answer::Full(Response::json(200, "{}")))
        }
    }

    /// Every cookie a response installed, in the order it wrote them.
    fn set_cookies(response: &Response) -> Vec<String> {
        response
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// The grant cookie a response installed, if it installed one.
    fn set_cookie(response: &Response) -> Option<String> {
        response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.clone())
    }

    /// The value from a `Set-Cookie`, for handing back as a `Cookie`.
    fn grant_value(cookie: &str) -> String {
        cookie
            .split(';')
            .next()
            .and_then(|pair| pair.split_once('='))
            .map(|(_, value)| value.to_string())
            .unwrap_or_default()
    }

    /// A request that carries one named cookie and no body.
    fn with_named_grant(method: &str, path: &str, name: &str, cookie: &str) -> Request {
        parse(&format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\
             Cookie: {name}={cookie}\r\nContent-Length: 0\r\n\r\n"
        ))
        .unwrap()
    }

    /// A `POST` carrying one named cookie and a JSON body, read the way a real request is.
    fn post_named_grant(path: &str, name: &str, cookie: &str, body: &str) -> Request {
        let wire = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nCookie: {name}={cookie}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut reader = wire.as_bytes();
        crate::http::read(&mut reader).unwrap()
    }

    /// The same page, returning every capability the dialog holds.
    fn app_page_with_grants(scratch: &Scratch, slug: &str) -> (Router, String, String, String) {
        let router = supervised_forwarding(scratch, RecordingForwarder::answering(200)).0;
        let started = router
            .respond(&post(&format!("/api/apps/{slug}/start")))
            .complete();
        assert_eq!(started.status, 200, "{}", body(&started));
        let page = router
            .respond(&get(&format!("/app/{slug}/"), "127.0.0.1:4270"))
            .complete();
        assert_eq!(page.status, 200);
        let cookies = set_cookies(&page);
        assert_eq!(
            cookies.len(),
            3,
            "a page carries one cookie per route: {cookies:?}"
        );
        let chat = cookies
            .iter()
            .find(|raw| raw.starts_with("deepnest-chat="))
            .expect("the message cookie")
            .clone();
        let escalation = cookies
            .iter()
            .find(|raw| raw.starts_with("deepnest-escalation="))
            .expect("the question cookie")
            .clone();
        let build = cookies
            .iter()
            .find(|raw| raw.starts_with("deepnest-build="))
            .expect("the build-status cookie")
            .clone();
        assert_ne!(chat, escalation, "one value must not open both routes");
        assert_ne!(
            build, chat,
            "nor is watching a build the same value as sending one"
        );
        assert_ne!(build, escalation, "nor is it the capability to approve");
        (
            router,
            grant_value(&chat),
            grant_value(&escalation),
            grant_value(&build),
        )
    }

    /// A `POST` that carries a cookie and a real message body instead of the token.
    fn post_with_grant(path: &str, cookie: &str, message: &str) -> Request {
        let body = format!("{{\"message\":\"{message}\"}}");
        let wire = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nCookie: deepnest-chat={cookie}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        // `read`, not `parse`: `parse` builds the head and leaves the body empty, and it is the
        // body the route under test exists to read.
        let mut reader = wire.as_bytes();
        crate::http::read(&mut reader).unwrap()
    }

    /// A request that carries only the cookie.
    ///
    /// No body on purpose: `respond` refuses one on every route but two, and that refusal arrives
    /// before authorisation — so a body here would test the wrong thing and answer `400`.
    fn with_grant(method: &str, path: &str, cookie: &str) -> Request {
        parse(&format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\
             Cookie: deepnest-chat={cookie}\r\nContent-Length: 0\r\n\r\n"
        ))
        .unwrap()
    }

    /// Start `slug` and serve its page, returning the router and the grant the page received.
    fn app_page_with_grant(scratch: &Scratch, slug: &str) -> (Router, String) {
        let router = supervised_forwarding(scratch, RecordingForwarder::answering(200)).0;
        let started = router
            .respond(&post(&format!("/api/apps/{slug}/start")))
            .complete();
        assert_eq!(started.status, 200, "{}", body(&started));
        let page = router
            .respond(&get(&format!("/app/{slug}/"), "127.0.0.1:4270"))
            .complete();
        assert_eq!(page.status, 200);
        let cookie = set_cookie(&page).expect("an app page must carry the dialog's capability");
        (router, grant_value(&cookie))
    }

    #[test]
    fn an_app_page_hands_out_only_the_route_the_dialog_needs() {
        let scratch = Scratch::new("grant-page");
        scratch.built_app("notes", "Notes");
        let router = supervised_forwarding(&scratch, RecordingForwarder::answering(200)).0;
        let started = router.respond(&post("/api/apps/notes/start")).complete();
        assert_eq!(started.status, 200, "{}", body(&started));

        let page = router
            .respond(&get("/app/notes/", "127.0.0.1:4270"))
            .complete();
        let raw = set_cookie(&page).expect("an app page must carry the dialog's capability");
        // The cookie names one path, carries no lifetime the browser enforces, cannot be read by
        // the app's script, and is not sent on a cross-site request.
        assert!(raw.contains("Path=/api/apps/notes/chat"), "{raw}");
        assert!(raw.contains("HttpOnly"), "{raw}");
        assert!(raw.contains("SameSite=Strict"), "{raw}");
        assert!(!raw.contains("Max-Age"), "{raw}");
        assert!(!raw.contains("Path=/api/apps/notes/ "), "{raw}");
    }

    #[test]
    fn the_capability_sends_that_apps_message_and_opens_nothing_else() {
        let scratch = Scratch::new("grant-scope");
        scratch.built_app("notes", "Notes");
        let (router, cookie) = app_page_with_grant(&scratch, "notes");

        // It is accepted on the one route it names. There is no config file in this fixture, so
        // `503` is the message-path answer that proves the request got past authorisation.
        let asked = router
            .respond(&post_with_grant("/api/apps/notes/chat", &cookie, "hello"))
            .complete();
        assert_eq!(asked.status, 503, "{}", body(&asked));

        // And on nothing else: not the app's other verbs, not the listing, not the config.
        for (method, path) in [
            ("POST", "/api/apps/notes/build"),
            ("POST", "/api/apps/notes/start"),
            ("GET", "/api/apps"),
            ("GET", "/api/config"),
        ] {
            let refused = router
                .respond(&with_grant(method, path, &cookie))
                .complete();
            assert_eq!(refused.status, 401, "{method} {path}: {}", body(&refused));
        }
    }

    #[test]
    fn one_apps_capability_does_not_send_another_apps_message() {
        let scratch = Scratch::new("grant-other");
        scratch.built_app("notes", "Notes");
        scratch.built_app("budget", "Budget");
        let (router, cookie) = app_page_with_grant(&scratch, "notes");
        let started = router.respond(&post("/api/apps/budget/start")).complete();
        assert_eq!(started.status, 200, "{}", body(&started));

        let refused = router
            .respond(&post_with_grant("/api/apps/budget/chat", &cookie, "hello"))
            .complete();
        assert_eq!(refused.status, 401, "{}", body(&refused));
    }

    #[test]
    fn a_message_with_no_capability_is_refused() {
        let scratch = Scratch::new("grant-absent");
        scratch.built_app("notes", "Notes");
        let (router, _) = app_page_with_grant(&scratch, "notes");
        let refused = router
            .respond(&post_with_grant("/api/apps/notes/chat", "", "hello"))
            .complete();
        assert_eq!(refused.status, 401, "{}", body(&refused));
    }

    #[test]
    fn the_token_still_opens_the_chat_route_without_any_cookie() {
        // The grant is an addition, not a replacement: a program holding the token keeps working
        // exactly as it did.
        let scratch = Scratch::new("grant-token");
        scratch.built_app("notes", "Notes");
        let (router, _) = app_page_with_grant(&scratch, "notes");
        let mut request = post("/api/apps/notes/chat");
        request.body = b"{\"message\":\"hello\"}".to_vec();
        let asked = router.respond(&request).complete();
        assert_eq!(asked.status, 503, "{}", body(&asked));
    }

    #[test]
    fn a_page_that_still_holds_good_capabilities_is_not_given_more() {
        // Reloading an app page must not churn the store, and must not take the capability a
        // second tab is already using.
        let scratch = Scratch::new("grant-reload");
        scratch.built_app("notes", "Notes");
        let (router, chat, escalation, build) = app_page_with_grants(&scratch, "notes");
        let again = parse(&format!(
            "GET /app/notes/ HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\
             Cookie: deepnest-chat={chat}; deepnest-escalation={escalation}; \
             deepnest-build={build}\r\n\r\n"
        ))
        .unwrap();
        let reloaded = router.respond(&again).complete();
        assert_eq!(reloaded.status, 200);
        assert_eq!(
            set_cookies(&reloaded).len(),
            0,
            "a capability was minted twice"
        );
    }

    #[test]
    fn a_page_holding_one_capability_is_given_the_other_and_keeps_the_one_it_has() {
        // The values are separate, so a page that has only one is given exactly the other two.
        let scratch = Scratch::new("grant-half");
        scratch.built_app("notes", "Notes");
        let (router, chat, _, _) = app_page_with_grants(&scratch, "notes");
        let again = parse(&format!(
            "GET /app/notes/ HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\
             Cookie: deepnest-chat={chat}\r\n\r\n"
        ))
        .unwrap();
        let reloaded = router.respond(&again).complete();
        let minted = set_cookies(&reloaded);
        assert_eq!(minted.len(), 2, "{minted:?}");
        assert!(
            minted
                .iter()
                .any(|raw| raw.starts_with("deepnest-escalation=")),
            "{minted:?}"
        );
        assert!(
            minted.iter().any(|raw| raw.starts_with("deepnest-build=")),
            "{minted:?}"
        );
        assert!(
            !minted
                .iter()
                .any(|raw| raw.contains(&format!("deepnest-chat={chat}"))),
            "the capability it already held was handed back to it: {minted:?}"
        );
    }

    #[test]
    fn a_page_hands_out_one_capability_per_route_the_dialog_opens() {
        let scratch = Scratch::new("grant-two-routes");
        scratch.built_app("notes", "Notes");
        let router = supervised_forwarding(&scratch, RecordingForwarder::answering(200)).0;
        let started = router.respond(&post("/api/apps/notes/start")).complete();
        assert_eq!(started.status, 200, "{}", body(&started));
        let page = router
            .respond(&get("/app/notes/", "127.0.0.1:4270"))
            .complete();

        let cookies = set_cookies(&page);
        assert_eq!(cookies.len(), 3, "{cookies:?}");
        let chat = cookies
            .iter()
            .find(|raw| raw.starts_with("deepnest-chat="))
            .expect("the message cookie");
        let escalation = cookies
            .iter()
            .find(|raw| raw.starts_with("deepnest-escalation="))
            .expect("the question cookie");
        let build = cookies
            .iter()
            .find(|raw| raw.starts_with("deepnest-build="))
            .expect("the build-status cookie");
        // Each names its own route and nothing wider, and none names another's.
        assert!(chat.contains("Path=/api/apps/notes/chat"), "{chat}");
        assert!(!chat.contains("/escalation"), "{chat}");
        assert!(!chat.contains("/build"), "{chat}");
        assert!(
            escalation.contains("Path=/api/apps/notes/escalation"),
            "{escalation}"
        );
        assert!(!escalation.contains("/chat"), "{escalation}");
        assert!(!escalation.contains("/build"), "{escalation}");
        assert!(build.contains("Path=/api/apps/notes/build"), "{build}");
        assert!(!build.contains("/chat"), "{build}");
        assert!(!build.contains("/escalation"), "{build}");
        assert_ne!(grant_value(chat), grant_value(escalation));
        assert_ne!(grant_value(build), grant_value(chat));
        assert_ne!(grant_value(build), grant_value(escalation));
    }

    #[test]
    fn a_turn_that_changed_the_app_starts_a_rebuild_and_a_turn_that_only_read_does_not() {
        // The seam the whole loop turns on, and the one a live model would be needed to reach
        // through the route: what a turn did is its `changed` flag, and what that flag means is
        // here.
        let scratch = Scratch::new("changed-starts-a-build");
        scratch.built_app("notes", "Notes");
        let router = supervised_with(&scratch, Arc::new(FakeBuilder::succeeds()));
        let lifecycle = router.lifecycle.as_ref().expect("this test built one");
        let slug = Slug::parse("notes").unwrap();

        assert!(!starts_a_rebuild_if_the_app_changed(
            &json!({ "changed": false }),
            &lifecycle.rebuild,
            &slug,
        ));
        assert_eq!(
            lifecycle.rebuild.status(&slug),
            rebuild::Status::Idle,
            "a turn that only read the app must not start a compiler"
        );

        assert!(starts_a_rebuild_if_the_app_changed(
            &json!({ "changed": true }),
            &lifecycle.rebuild,
            &slug,
        ));
        assert_ne!(
            lifecycle.rebuild.status(&slug),
            rebuild::Status::Idle,
            "a turn that wrote to the app left nothing building"
        );
    }

    /// A `Releases` that answers from a script, so the route runs with no socket.
    struct ScriptedReleases {
        body: String,
    }

    impl deepnest_skills::Releases for ScriptedReleases {
        fn latest(&self, _repo: &str) -> std::io::Result<String> {
            Ok(self.body.clone())
        }
    }

    #[test]
    fn the_update_check_reaches_the_route_and_says_which_skill_has_a_newer_release() {
        let scratch = Scratch::new("skill-updates");
        // The real bundled catalog names `browser-harness` with a `github_release` check, so the
        // entry the report matches against is the one the binary actually ships rather than a
        // fixture that could drift from it.
        std::fs::create_dir_all(deepnest_skills::user_root(&scratch.base)).unwrap();
        std::fs::write(
            deepnest_skills::catalog::path(&deepnest_skills::user_root(&scratch.base)),
            deepnest_skills::BUNDLED,
        )
        .unwrap();
        scratch.skill("browser-harness", "drives a browser");

        let router = router()
            .with_lifecycle(
                Arc::new(deepnest_apps::Supervisor::with_parts(
                    Box::new(FakeSpawner),
                    Box::new(AlwaysReady),
                )),
                scratch.base.clone(),
                scratch.apps(),
            )
            .with_releases(Arc::new(ScriptedReleases {
                body: r#"{"tag_name":"v0.4.0"}"#.to_string(),
            }));

        let response = router
            .respond(&get(SKILL_UPDATES, "127.0.0.1:4270"))
            .complete();
        // Behind the token like every route that is not a page: the panel's script has it, and a
        // page an app serves does not.
        assert_eq!(response.status, 401, "{:?}", response);

        let answer = router
            .respond(
                &parse(&format!(
                    "GET {SKILL_UPDATES} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\
                     authorization: bearer {}\r\n\r\n",
                    "a".repeat(64)
                ))
                .unwrap(),
            )
            .complete();
        assert_eq!(answer.status, 200, "{}", body(&answer));
        let said: Value = serde_json::from_str(&body(&answer)).unwrap();
        assert_eq!(said["checked"], 1, "{said}");
        // Nothing is recorded about what is installed, so there is nothing to compare with — and
        // that is `unknown`, not `up to date`. Reporting it as current would be the silent failure
        // the four-answer check exists to prevent.
        assert_eq!(said["unchecked"], 0, "{said}");
        assert_eq!(said["skills"][0]["name"], "browser-harness", "{said}");
        assert_eq!(said["skills"][0]["state"], "unknown", "{said}");
        assert_eq!(said["skills"][0]["version"], "0.4.0", "{said}");
    }

    #[test]
    fn the_update_check_is_read_and_not_written_through_the_same_path() {
        // The refresh has a route of its own, so this one stays read-only — and the refusal names
        // the method rather than answering 404 as if the route were not there.
        let scratch = Scratch::new("skill-updates-post");
        let router = router().with_lifecycle(
            Arc::new(deepnest_apps::Supervisor::with_parts(
                Box::new(FakeSpawner),
                Box::new(AlwaysReady),
            )),
            scratch.base.clone(),
            scratch.apps(),
        );
        let refused = router.respond(&post(SKILL_UPDATES)).complete();
        assert_eq!(refused.status, 405, "{}", body(&refused));
        assert!(body(&refused).contains("GET"), "{}", body(&refused));
    }

    /// A publisher that prints a usable skill, so a refresh through the route runs no program.
    struct Prints;

    impl deepnest_skills::Publisher for Prints {
        fn publish(&self, program: &str) -> std::io::Result<deepnest_skills::Report> {
            Ok(deepnest_skills::Report {
                status: Some(0),
                // A description the seeded file does not have, so a test can tell a rewritten file
                // from one that was left alone.
                stdout: format!(
                    "---\nname: {program}\ndescription: published again\n---\n\nBody.\n"
                ),
                stderr: String::new(),
            })
        }
    }

    /// A home with the bundled catalog and one managed skill, so the route refreshes the entry the
    /// binary actually ships rather than a fixture that could drift from it.
    fn refreshable(name: &str) -> Scratch {
        let scratch = Scratch::new(name);
        let root = deepnest_skills::user_root(&scratch.base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            deepnest_skills::catalog::path(&root),
            deepnest_skills::BUNDLED,
        )
        .unwrap();
        scratch.skill("browser-harness", "drives a browser");
        let mut state = deepnest_skills::State::new();
        state.record_install("browser-harness", Some("0.1.0".to_string()), "2026-10-31");
        state.write(&deepnest_skills::state::path(&root)).unwrap();
        scratch
    }

    /// The router these tests drive: a real lifecycle so the queue lives as long as the router, a
    /// scripted forge, and a publisher that is not a program.
    fn refresh_router(scratch: &Scratch, body: &str) -> Router {
        router()
            .with_lifecycle(
                Arc::new(deepnest_apps::Supervisor::with_parts(
                    Box::new(FakeSpawner),
                    Box::new(AlwaysReady),
                )),
                scratch.base.clone(),
                scratch.apps(),
            )
            .with_releases(Arc::new(ScriptedReleases {
                body: body.to_string(),
            }))
            .with_publisher(Arc::new(Prints))
    }

    /// A `GET` as the panel's script sends it — the token, which the page has and an app's page does
    /// not.
    fn get_as_panel(path: &str) -> Request {
        parse(&format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\n\r\n",
            "a".repeat(64)
        ))
        .unwrap()
    }

    fn post_json(path: &str, payload: &str) -> Request {
        let mut request = parse(&format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            "a".repeat(64),
            payload.len()
        ))
        .unwrap();
        // `parse` reads the head and stops at the blank line: the body is what the connection reader
        // buffers afterwards, so a test attaches it here rather than pasting it after the header.
        request.body = payload.as_bytes().to_vec();
        request
    }

    #[test]
    fn a_refresh_asked_for_from_the_panel_records_the_version_it_adopted() {
        let scratch = refreshable("skill-refresh");
        let router = refresh_router(&scratch, r#"{"tag_name":"v0.4.0"}"#);

        // Nothing asked for yet, and that is not the same as "the last one succeeded".
        let before = router.respond(&get_as_panel(SKILL_REFRESH)).complete();
        assert_eq!(before.status, 200, "{}", body(&before));
        let said: Value = serde_json::from_str(&body(&before)).unwrap();
        assert_eq!(said["refresh"], Value::Null, "{said}");

        let asked = router
            .respond(&post_json(SKILL_REFRESH, r#"{"name":"browser-harness"}"#))
            .complete();
        assert_eq!(asked.status, 202, "{}", body(&asked));
        assert!(
            body(&asked).contains("refreshing"),
            "the route did not say it had started: {}",
            body(&asked)
        );

        // The publisher is somebody else's program, so the answer is "it started" and the page
        // polls this route for the outcome.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let finished = loop {
            let answer = router.respond(&get_as_panel(SKILL_REFRESH)).complete();
            let said: Value = serde_json::from_str(&body(&answer)).unwrap();
            if !said["refresh"].is_null() && said["refresh"]["state"] != "refreshing" {
                break said;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the refresh never reported: {said}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(finished["refresh"]["name"], "browser-harness", "{finished}");
        assert_eq!(finished["refresh"]["state"], "refreshed", "{finished}");
        assert_eq!(finished["refresh"]["version"], "0.4.0", "{finished}");
        assert_eq!(finished["refresh"]["replaced"], true, "{finished}");

        // The reading that matters: the state file the next update check reads, not a copy the
        // route kept.
        let recorded = deepnest_skills::State::read(&deepnest_skills::state::path(
            &deepnest_skills::user_root(&scratch.base),
        ));
        assert_eq!(
            recorded.get("browser-harness").unwrap().version.as_deref(),
            Some("0.4.0"),
            "the adopted version never reached the state file"
        );
    }

    #[test]
    fn the_refresh_route_is_behind_the_token_and_asks_for_a_post() {
        let scratch = refreshable("skill-refresh-guard");
        let router = refresh_router(&scratch, r#"{"tag_name":"v0.4.0"}"#);

        // Behind the token like every route that is not a page: the capability to run a program on
        // the user's machine is not one a page an app serves may hold.
        let anonymous = router
            .respond(&get(SKILL_REFRESH, "127.0.0.1:4270"))
            .complete();
        assert_eq!(anonymous.status, 401, "{}", body(&anonymous));

        // A state change behind GET would be one a browser makes by prefetching a link.
        let deleted = parse(&format!(
            "DELETE {SKILL_REFRESH} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer \
             {}\r\n\r\n",
            "a".repeat(64)
        ))
        .unwrap();
        let refused = router.respond(&deleted).complete();
        assert_eq!(refused.status, 405, "{}", body(&refused));
        assert!(body(&refused).contains("POST"), "{}", body(&refused));
    }

    #[test]
    fn a_refresh_body_that_names_no_skill_is_refused_rather_than_guessed() {
        let scratch = refreshable("skill-refresh-body");
        let router = refresh_router(&scratch, r#"{"tag_name":"v0.4.0"}"#);

        let empty = router
            .respond(&post_json(SKILL_REFRESH, r#"{"name":"  "}"#))
            .complete();
        assert_eq!(empty.status, 400, "{}", body(&empty));

        // `skill` instead of `name` is a typo, and a route that read "no name" from a body plainly
        // carrying one would report a refresh of nothing as if it had been asked for.
        let typo = router
            .respond(&post_json(SKILL_REFRESH, r#"{"skill":"browser-harness"}"#))
            .complete();
        assert_eq!(typo.status, 400, "{}", body(&typo));
    }

    #[test]
    fn the_page_can_watch_a_build_and_cannot_start_one() {
        let scratch = Scratch::new("build-status");
        scratch.built_app("notes", "Notes");
        let (router, _, _, build) = app_page_with_grants(&scratch, "notes");
        let path = "/api/apps/notes/build";

        // The route is not open to a page that presents nothing, and it is not open to the
        // capability that sends messages either.
        let bare = router.respond(&get(path, "127.0.0.1:4270")).complete();
        assert_eq!(bare.status, 401, "{:?}", bare);

        // With its own cookie it answers, and what it answers before anything asked for a build is
        // `idle` rather than a guess about a build that never ran.
        let read = router
            .respond(&with_named_grant("GET", path, grant::BUILD_COOKIE, &build))
            .complete();
        assert_eq!(read.status, 200, "{}", body(&read));
        assert!(
            body(&read).contains("\"state\":\"idle\""),
            "{}",
            body(&read)
        );

        // The same cookie does not spend a compiler: watching a build and starting one are
        // different capabilities, and the second stays behind the token.
        let start = router
            .respond(&with_named_grant("POST", path, grant::BUILD_COOKIE, &build))
            .complete();
        assert_eq!(start.status, 401, "{:?}", start);

        // The token opens it too, as it opens every route: the cookie is a narrowing, not a second
        // door that replaces the first.
        let with_token = router
            .respond(
                &parse(&format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nauthorization: bearer {}\r\n\r\n",
                    "a".repeat(64)
                ))
                .unwrap(),
            )
            .complete();
        assert_eq!(with_token.status, 200, "{:?}", with_token);
    }

    #[test]
    fn the_question_route_is_not_opened_by_the_capability_that_sends_messages() {
        // The two are separate values for exactly this: a page that can spend an agent turn must
        // not be able to answer its own widening request with the same cookie.
        let scratch = Scratch::new("escalation-scope");
        scratch.built_app("notes", "Notes");
        let (router, chat, escalation, _) = app_page_with_grants(&scratch, "notes");

        let path = "/api/apps/notes/escalation";
        let wrong = router
            .respond(&with_named_grant("GET", path, grant::COOKIE, &chat))
            .complete();
        assert_eq!(wrong.status, 401, "{:?}", wrong);
        let bare = router.respond(&get(path, "127.0.0.1:4270")).complete();
        assert_eq!(bare.status, 401, "{:?}", bare);
        // And the escalation value does not send a message either.
        let chat_route = router
            .respond(&post_named_grant(
                "/api/apps/notes/chat",
                grant::ESCALATION_COOKIE,
                &escalation,
                r#"{"message":"hi"}"#,
            ))
            .complete();
        assert_eq!(chat_route.status, 401, "{:?}", chat_route);
        // The one that matches is admitted, and says nothing is being asked.
        let open = router
            .respond(&with_named_grant(
                "GET",
                path,
                grant::ESCALATION_COOKIE,
                &escalation,
            ))
            .complete();
        assert_eq!(open.status, 200, "{}", body(&open));
        assert!(
            body(&open).contains(r#"{"pending":null}"#),
            "{}",
            body(&open)
        );
    }

    #[test]
    fn an_answer_with_no_question_waiting_is_a_conflict_and_not_a_success() {
        let scratch = Scratch::new("escalation-idle");
        scratch.built_app("notes", "Notes");
        let (router, _, escalation, _) = app_page_with_grants(&scratch, "notes");
        let answered = router
            .respond(&post_named_grant(
                "/api/apps/notes/escalation",
                grant::ESCALATION_COOKIE,
                &escalation,
                r#"{"approve":true}"#,
            ))
            .complete();
        // A page that believed this landed would tell the user they approved something that never
        // happened. `409` states the disagreement between the page and this process.
        assert_eq!(answered.status, 409, "{}", body(&answered));
        assert!(
            body(&answered).contains("nothing to answer"),
            "{}",
            body(&answered)
        );
    }

    #[test]
    fn a_body_that_does_not_name_an_answer_is_refused_rather_than_read_as_a_no() {
        let scratch = Scratch::new("escalation-body");
        scratch.built_app("notes", "Notes");
        let (router, _, escalation, _) = app_page_with_grants(&scratch, "notes");
        let post = |body: &str| {
            router
                .respond(&post_named_grant(
                    "/api/apps/notes/escalation",
                    grant::ESCALATION_COOKIE,
                    &escalation,
                    body,
                ))
                .complete()
        };
        // `approved` is the obvious typo, and reading it as "no answer" would turn a page's
        // approval into a refusal the user never gave.
        for wrong in [r#"{"approved":true}"#, "{}", "not json"] {
            let response = post(wrong);
            assert_eq!(response.status, 400, "{wrong}: {}", body(&response));
        }
        let empty = router
            .respond(&with_named_grant(
                "POST",
                "/api/apps/notes/escalation",
                grant::ESCALATION_COOKIE,
                &escalation,
            ))
            .complete();
        assert_eq!(empty.status, 400, "{}", body(&empty));
        assert!(body(&empty).contains("empty"), "{}", body(&empty));
    }

    #[test]
    fn escalation_reads_with_get_and_answers_with_post() {
        let scratch = Scratch::new("escalation-method");
        scratch.built_app("notes", "Notes");
        let (router, _, escalation, _) = app_page_with_grants(&scratch, "notes");
        let response = router
            .respond(&with_named_grant(
                "DELETE",
                "/api/apps/notes/escalation",
                grant::ESCALATION_COOKIE,
                &escalation,
            ))
            .complete();
        assert_eq!(response.status, 405, "{:?}", response);
        assert!(
            response
                .headers
                .iter()
                .any(|(name, value)| *name == "Allow" && *value == "GET, POST"),
            "{:?}",
            response.headers
        );
    }

    #[test]
    fn the_page_reads_the_question_and_its_answer_wakes_the_call_that_asked() {
        // The whole hop, with a thread standing in for the turn and the route standing in for the
        // page: a question appears, the page polls it, the page answers, and the asking thread —
        // which is blocked inside a turn in the real thing — wakes with the person's answer.
        let scratch = Scratch::new("escalation-roundtrip");
        scratch.built_app("notes", "Notes");
        let (router, _, escalation, _) = app_page_with_grants(&scratch, "notes");
        let path = "/api/apps/notes/escalation";

        let approver = router.requests.approver("notes");
        let (sent, received) = std::sync::mpsc::channel();
        let asker = std::thread::spawn(move || {
            let answer = approver.ask(&deepnest_sandbox::EscalationRequest::new(
                deepnest_sandbox::Mode::WorkspaceWrite,
                deepnest_sandbox::Mode::DangerFullAccess,
                "the build writes outside the app",
            ));
            let _ = sent.send(answer);
        });

        let mut shown = String::new();
        for _ in 0..2000 {
            let polled = router
                .respond(&with_named_grant(
                    "GET",
                    path,
                    grant::ESCALATION_COOKIE,
                    &escalation,
                ))
                .complete();
            shown = body(&polled);
            if !shown.contains(r#""pending":null"#) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(shown.contains("danger-full-access"), "{shown}");
        assert!(shown.contains("workspace-write"), "{shown}");
        assert!(
            shown.contains("the build writes outside the app"),
            "{shown}"
        );

        let answered = router
            .respond(&post_named_grant(
                path,
                grant::ESCALATION_COOKIE,
                &escalation,
                r#"{"approve":true}"#,
            ))
            .complete();
        assert_eq!(answered.status, 200, "{}", body(&answered));
        assert!(
            body(&answered).contains(r#""approved":true"#),
            "{}",
            body(&answered)
        );
        assert!(
            body(&answered).contains("the build writes outside the app"),
            "the answer names what it answered: {}",
            body(&answered)
        );
        assert_eq!(
            received
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the turn woke"),
            deepnest_agent::Answer::Approved
        );
        asker.join().unwrap();

        // And the question is gone: an answer is not a standing permission.
        let after = router
            .respond(&with_named_grant(
                "GET",
                path,
                grant::ESCALATION_COOKIE,
                &escalation,
            ))
            .complete();
        assert!(
            body(&after).contains(r#""pending":null"#),
            "{}",
            body(&after)
        );
    }

    #[test]
    fn an_answer_that_is_not_a_page_gets_no_capability() {
        // There is no box on a JSON answer, so a cookie minted for it would be a capability
        // nobody uses — and every one of them costs an entry in the store.
        let scratch = Scratch::new("grant-json");
        scratch.built_app("notes", "Notes");
        let router = supervised_forwarding(&scratch, Arc::new(JsonAnswering)).0;
        let started = router.respond(&post("/api/apps/notes/start")).complete();
        assert_eq!(started.status, 200, "{}", body(&started));
        let answer = router
            .respond(&get("/app/notes/", "127.0.0.1:4270"))
            .complete();
        assert_eq!(answer.status, 200);
        assert_eq!(set_cookie(&answer), None, "JSON is not a page");
    }
}
