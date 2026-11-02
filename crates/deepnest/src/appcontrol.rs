//! The `app` seam, wired to this process's own supervisor.
//!
//! `crates/tools` owns what an `app` call *is*; this file owns what it reaches. Three facts live
//! here and nowhere else, and each is the reason the seam is a seam:
//!
//! - **The supervisor is this process's.** An app's port is allocated per start and never leaves
//!   the process that allocated it, so a second supervisor would start a second copy of an app the
//!   page is already being served from. The tool therefore takes the `Arc` the router holds rather
//!   than building one.
//! - **The app is decided at construction, not by the call.** The dialog builds one of these for
//!   one turn of one app's agent, so the tool has no app argument to get wrong and no name to
//!   validate: the `Slug` in the struct is the only app it can reach.
//! - **The logs are the home's.** `~/.deepnest/logs/<slug>.log` and `<slug>.build.log` are
//!   `crates/apps`' paths under the deepnest home, and the home is the binary's. A tool that
//!   guessed the path would read a file nothing writes.
//!
//! What this does **not** do: it does not touch the sandbox fence. Starting and stopping an app
//! writes no file and runs no shell, so there is no `Policy` question to ask beyond the mode the
//! tool already checked — and the mode check lives in `crates/tools`, where every other refusal
//! about what a call may do is written.

use std::path::PathBuf;
use std::sync::Arc;

use deepnest_apps::logs::{build_log_path, log_path, tail};
use deepnest_apps::{Slug, State, Supervisor};
use deepnest_tools::app::NAME;
use deepnest_tools::MAX_RESULT_CHARS;
use deepnest_tools::{AppAction, AppControl, AppRequest, LogKind, ToolContext, ToolResult};

/// The app this turn is for, reached through the process that supervises it.
pub struct DialogAppControl {
    supervisor: Arc<Supervisor>,
    /// The deepnest home, whose `logs/` holds both of an app's logs.
    home: PathBuf,
    /// The app's directory — where the built binary the supervisor runs would be.
    app_dir: PathBuf,
    /// The one app this call can reach.
    slug: Slug,
}

impl DialogAppControl {
    pub fn new(supervisor: Arc<Supervisor>, home: PathBuf, app_dir: PathBuf, slug: Slug) -> Self {
        Self {
            supervisor,
            home,
            app_dir,
            slug,
        }
    }

    /// The app's own log, or its build's, as `crates/apps` spells both.
    fn log_file(&self, kind: LogKind) -> PathBuf {
        match kind {
            LogKind::App => log_path(&self.home, &self.slug),
            LogKind::Build => build_log_path(&self.home, &self.slug),
        }
    }

    /// What is true about the app, in a sentence a model can act on.
    ///
    /// `Failed` is told apart from `Stopped` on purpose: "it is not running" and "it ended by
    /// itself, and here is what that looked like" call for different next steps, and a model given
    /// only the first would restart the app into the same failure without reading the reason.
    fn describe(&self, state: &State) -> String {
        match state {
            State::Running { port, pid } => {
                format!(
                    "`{}` is running on port {port} as process {pid}.",
                    self.slug
                )
            }
            State::Stopped => format!("`{}` is not running.", self.slug),
            State::Failed { reason } => format!(
                "`{}` is not running: it ended by itself — {reason}",
                self.slug
            ),
        }
    }
}

impl AppControl for DialogAppControl {
    fn act(&self, request: &AppRequest, _ctx: &ToolContext) -> ToolResult {
        match request.action {
            AppAction::Status => {
                let state = self.supervisor.state(&self.slug);
                ToolResult::ok(NAME, self.describe(&state))
            }
            AppAction::Start => {
                match self
                    .supervisor
                    .start(&self.slug, &self.app_dir, self.log_file(LogKind::App))
                {
                    Ok(state) => ToolResult::ok(NAME, self.describe(&state)),
                    // Every `StartError` already says which of five things went wrong, and the ones
                    // that involve the log name it. Prefixing it with "did not start" is the only
                    // thing a model needs on top: the rest is the diagnosis.
                    Err(error) => {
                        ToolResult::error(NAME, format!("`{}` did not start: {error}", self.slug))
                    }
                }
            }
            AppAction::Stop => match self.supervisor.stop(&self.slug) {
                Ok(state) => ToolResult::ok(NAME, self.describe(&state)),
                Err(error) => ToolResult::error(
                    NAME,
                    format!("`{}` could not be stopped: {error}", self.slug),
                ),
            },
            AppAction::Restart => match self.supervisor.restart(
                &self.slug,
                &self.app_dir,
                self.log_file(LogKind::App),
            ) {
                Ok(state) => ToolResult::ok(NAME, self.describe(&state)),
                Err(error) => {
                    ToolResult::error(NAME, format!("`{}` did not come back: {error}", self.slug))
                }
            },
            AppAction::Logs => {
                let path = self.log_file(request.log);
                match tail(&path, request.lines as usize) {
                    Ok(Some(text)) if text.trim().is_empty() => {
                        // Told apart from "no log": an empty file is a program that started and
                        // said nothing, and a model that read it as a missing log would go looking
                        // for a path that is already right.
                        ToolResult::ok(NAME, format!("{} is empty.", path.display()))
                    }
                    Ok(Some(text)) => ToolResult::ok(NAME, text).truncate(MAX_RESULT_CHARS),
                    Ok(None) => ToolResult::ok(
                        NAME,
                        match request.log {
                            LogKind::App => format!(
                                "`{}` has no log yet: {} does not exist. An app writes one when it \
                                 starts, so start it and ask again.",
                                self.slug,
                                path.display()
                            ),
                            LogKind::Build => format!(
                                "`{}` has not been built yet: {} does not exist.",
                                self.slug,
                                path.display()
                            ),
                        },
                    ),
                    Err(error) => ToolResult::error(
                        NAME,
                        format!("{} could not be read: {error}", path.display()),
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_apps::{apps_dir, create, EXPECTED_BINARY};

    /// A home with one app in it and no built binary, which is the state a fresh app is in.
    struct Scratch {
        home: PathBuf,
        slug: Slug,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let home = std::env::temp_dir()
                .join(format!("deepnest-appcontrol-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(home.join("apps")).unwrap();
            let slug = Slug::parse("notes").unwrap();
            let dir = apps_dir(&home).join("notes");
            create(&slug, &dir, "Notes").unwrap();
            Self { home, slug }
        }

        fn app_dir(&self) -> PathBuf {
            apps_dir(&self.home).join(self.slug.as_str())
        }

        fn control(&self) -> DialogAppControl {
            DialogAppControl::new(
                Arc::new(Supervisor::new()),
                self.home.clone(),
                self.app_dir(),
                self.slug.clone(),
            )
        }

        fn log_file(&self, name: &str) -> PathBuf {
            let path = self.home.join("logs").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            path
        }
    }

    fn ctx() -> ToolContext {
        let base =
            std::env::temp_dir().join(format!("deepnest-appcontrol-ctx-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        ToolContext::new(
            deepnest_sandbox::Policy::from_roots(
                deepnest_sandbox::Mode::WorkspaceWrite,
                base.clone(),
                base.clone(),
                base.clone(),
            ),
            base,
        )
    }

    fn request(action: AppAction, log: LogKind, lines: u64) -> AppRequest {
        AppRequest { action, log, lines }
    }

    #[test]
    fn an_app_nobody_started_is_reported_as_not_running() {
        let scratch = Scratch::new("status");
        let result = scratch
            .control()
            .act(&request(AppAction::Status, LogKind::App, 100), &ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("not running"), "{}", result.content);
        assert!(result.content.contains("notes"), "{}", result.content);
    }

    #[test]
    fn stopping_an_app_that_is_not_running_is_not_a_failure() {
        // The caller's intent is the postcondition, and a stop button that failed because the app
        // had already died would report the wrong thing.
        let scratch = Scratch::new("stop");
        let result = scratch
            .control()
            .act(&request(AppAction::Stop, LogKind::App, 100), &ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("not running"), "{}", result.content);
    }

    #[test]
    fn starting_one_that_was_never_built_says_to_build_it_and_spawns_nothing() {
        let scratch = Scratch::new("start");
        // The app template makes no binary; `run_command` looks for exactly this path.
        assert!(!scratch.app_dir().join(EXPECTED_BINARY).exists());
        let result = scratch
            .control()
            .act(&request(AppAction::Start, LogKind::App, 100), &ctx());
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("build the app first"),
            "{}",
            result.content
        );
    }

    #[test]
    fn a_missing_app_log_says_which_file_and_what_to_do_about_it() {
        let scratch = Scratch::new("nolog");
        let result = scratch
            .control()
            .act(&request(AppAction::Logs, LogKind::App, 100), &ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("notes.log"), "{}", result.content);
        assert!(result.content.contains("start it"), "{}", result.content);
    }

    #[test]
    fn a_missing_build_log_is_a_different_sentence_because_it_is_a_different_state() {
        let scratch = Scratch::new("nobuildlog");
        let result = scratch
            .control()
            .act(&request(AppAction::Logs, LogKind::Build, 100), &ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("build.log"), "{}", result.content);
        assert!(
            result.content.contains("has not been built"),
            "{}",
            result.content
        );
    }

    #[test]
    fn the_log_is_read_from_its_end_and_the_two_logs_are_two_files() {
        let scratch = Scratch::new("logs");
        std::fs::write(scratch.log_file("notes.log"), "first\nsecond\nthird\n").unwrap();
        std::fs::write(
            scratch.log_file("notes.build.log"),
            "error: the compiler said no\n",
        )
        .unwrap();
        let control = scratch.control();

        let app = control.act(&request(AppAction::Logs, LogKind::App, 2), &ctx());
        assert!(!app.is_error, "{}", app.content);
        assert!(app.content.contains("third"), "{}", app.content);
        assert!(
            !app.content.contains("first"),
            "two lines from the end must not include the first: {}",
            app.content
        );

        let build = control.act(&request(AppAction::Logs, LogKind::Build, 100), &ctx());
        assert!(!build.is_error, "{}", build.content);
        assert!(
            build.content.contains("the compiler said no"),
            "{}",
            build.content
        );
    }

    #[test]
    fn an_empty_log_says_it_is_empty_rather_than_missing() {
        let scratch = Scratch::new("empty");
        std::fs::write(scratch.log_file("notes.log"), "").unwrap();
        let result = scratch
            .control()
            .act(&request(AppAction::Logs, LogKind::App, 100), &ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("is empty"), "{}", result.content);
    }

    #[test]
    fn every_action_answers_with_the_name_the_model_called() {
        // A result pasted into the conversation says which call it answers; the seam is the one
        // place that could get that wrong without any test noticing.
        let scratch = Scratch::new("names");
        let control = scratch.control();
        for action in [
            AppAction::Status,
            AppAction::Stop,
            AppAction::Logs,
            AppAction::Start,
            AppAction::Restart,
        ] {
            let result = control.act(&request(action, LogKind::App, 10), &ctx());
            assert_eq!(result.name, NAME, "{}", result.content);
        }
    }
}
