//! `deepnest` — the binary.
//!
//! The order of the steps is the design: **config, then token, then the port.** A config that
//! does not parse stops startup before anything is written, so a typo never leaves a token file
//! behind that suggests a run happened. The port comes last because taking it is the admission —
//! the second instance is refused at the bind, after its own token has been read (not minted),
//! and it exits reporting the address of the instance that already owns the port instead of
//! pretending to start.
//!
//! **A start ends by opening the panel in the desktop's browser** ([`deepnest::open`]): the
//! README's first sentence is "one double-click, one page in your browser", and this is the half of
//! it a terminal cannot do by itself. It comes after the banner, so every line this run has to say
//! about itself is on screen before the desktop is asked; a second start opens the address it found
//! already running, because a second double-click is a person asking for that page; and it goes
//! through a seam whose switch is `--no-open`, so the end-to-end tests do not open a window on the
//! machine running them. A browser that will not take the URL is one line on stderr, not a failed
//! start — the panel is this program's whole interface and the address is printed either way.
//!
//! The one piece of state this process owns beyond the port is the **supervisor**: the apps it
//! started. It is built here and lives as long as the process, and it is deliberately *not* run
//! down on the way out, because there is no way out to run it down on — this process waits on a
//! park and ends by signal, so there is no point at which `stop_all` could be called before the
//! operating system ends everything at once. An app that outlives a killed deepnest is therefore
//! reachable only by pid for now; the shutdown path that would close it is `crates/gateway`'s,
//! together with the real server loop. Saying that here is better than a `Drop` that suggests a
//! clean exit this build does not have.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;

use deepnest::config::{self, ConfigError};
use deepnest::open::{Desktop, Opening};
use deepnest::router::Lifecycle;
use deepnest::server::{self, BindError, ServerOptions, StartError};
use deepnest::token::{self, TokenError};
use deepnest::{home, prepare_home};

const USAGE: &str = "\
deepnest — local-first app generator

Usage:
  deepnest                start deepnest, serve http://127.0.0.1:4270/, and open it
  deepnest --no-open      start deepnest and print the address without opening anything
  deepnest --help         this text
  deepnest --version      print the version

Configuration — $DEEPNEST_HOME/config.toml (default ~/.deepnest/config.toml):

  [llm]
  base_url = \"https://api.deepseek.com\"
  api_key  = \"sk-...\"
  model    = \"deepseek-flash\"

  A missing config is not an error: deepnest starts, and the panel reports the model
  endpoint as unconfigured. A config that does not parse stops startup, so a typo is
  reported where it is instead of at the first request.

Environment:
  DEEPNEST_HOME   relocate the deepnest home (default ~/.deepnest)
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure { code, message }) => {
            let _ = writeln!(std::io::stderr(), "deepnest: {message}");
            ExitCode::from(code)
        }
    }
}

struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn run() -> Result<(), Failure> {
    match arguments(std::env::args().skip(1))? {
        Action::Run(opening) => serve(opening),
        Action::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Action::Version => {
            println!("deepnest {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Run(Opening),
    Help,
    Version,
}

/// What this command line asks for.
///
/// The arguments are taken as an iterator rather than read from the process, so that what a flag
/// means is a test of this function instead of a test that has to start a program.
fn arguments<I>(arguments: I) -> Result<Action, Failure>
where
    I: IntoIterator<Item = String>,
{
    let mut action = Action::Run(Opening::Browser);
    for argument in arguments {
        match argument.as_str() {
            "--help" | "-h" => action = Action::Help,
            "--version" | "-V" => action = Action::Version,
            // Kept as its own flag rather than folded into the action because it is not one: a
            // person can ask for help and also not want a window, and what they get is the help.
            "--no-open" => {
                if let Action::Run(_) = action {
                    action = Action::Run(Opening::Never);
                }
            }
            other => {
                return Err(Failure::new(
                    2,
                    format!("unknown argument `{other}` — try `deepnest --help`"),
                ))
            }
        }
    }
    Ok(action)
}

fn serve(opening: Opening) -> Result<(), Failure> {
    let home = home().ok_or_else(|| {
        Failure::new(
            1,
            "cannot find a home directory: neither $DEEPNEST_HOME nor $HOME/$USERPROFILE is set",
        )
    })?;
    prepare_home(&home)
        .map_err(|error| Failure::new(1, format!("cannot prepare {}: {error}", home.display())))?;

    let config_path = home.join(config::FILE_NAME);
    let config = config::load(&config_path).map_err(config_failure)?;
    if config.is_none() {
        // First run. Not a warning to be silenced by writing a stub config the user did not
        // choose: the panel is where the endpoint gets collected.
        let _ = writeln!(
            std::io::stderr(),
            "deepnest: no config at {} — starting with the model endpoint unconfigured",
            config_path.display()
        );
    }

    let token_path = home.join(token::FILE_NAME);
    let token = token::ensure(&token_path).map_err(token_failure)?;

    // Where apps live. It is not created here: an install with no apps yet has no `apps/`
    // directory, and the listing reports that as "none yet" rather than as a fault.
    let apps_dir = deepnest_apps::apps_dir(&home);

    // The supervisor owns every app process this run starts. Built before the port is taken so
    // that a refused second instance has started nothing, and shared rather than moved because
    // the HTTP worker answers state questions from another thread.
    let supervisor = Arc::new(deepnest_apps::Supervisor::new());

    let options = ServerOptions {
        port: server::DEFAULT_PORT,
        token: Some(token),
        token_path: Some(token_path.clone()),
        model: config.as_ref().map(|config| config.model().to_string()),
        apps_dir: Some(apps_dir.clone()),
        lifecycle: Some(Lifecycle::new(Arc::clone(&supervisor), home.clone())),
        config_path: Some(config_path.clone()),
        ..Default::default()
    };

    let running = match server::start(options) {
        Ok(server) => server,
        // The second instance is not an error. It says where the first one is and leaves — and it
        // opens that address, because a second double-click is a person asking for the page that is
        // already being served, and answering it with a line of text on a terminal nobody is looking
        // at is how the second click comes to look like nothing happened.
        Err(StartError::Bind(BindError::InUse { addr, .. })) => {
            let url = format!("http://{addr}/");
            println!("deepnest is already running: {url}");
            report(opening.show(&Desktop, &url), &url);
            return Ok(());
        }
        Err(error) => return Err(Failure::new(1, error.to_string())),
    };

    println!(
        "deepnest {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        running.url()
    );
    println!("  config:    {}", config_path.display());
    println!("  token:     {} (mode 0600)", token_path.display());
    println!("  apps:      {}", apps_dir.display());
    println!(
        "  logs:      {}",
        home.join(deepnest_apps::LOGS_DIR).display()
    );

    // The toolchain directory is read only here, after the port: the bind is the single-instance
    // admission, so by this point no other deepnest can be filling a staging directory, and what is
    // under `toolchain/` is from a run that already ended. Sweeping before the bind would let a
    // second instance delete the staging directory the first one is writing.
    match deepnest_packaging::sweep(&home) {
        Ok(removed) => {
            for path in removed {
                eprintln!(
                    "deepnest: removed a leftover from an interrupted unpack: {}",
                    path.display()
                );
            }
        }
        // A toolchain directory that cannot be read is worth saying and not worth stopping for: the
        // panel is where a user finds out what is wrong, and refusing to start takes the panel away.
        Err(error) => eprintln!("deepnest: cannot inspect the toolchain directory: {error}"),
    }
    let toolchain = deepnest_packaging::status(
        &home,
        deepnest_packaging::VERSION,
        deepnest_packaging::PAYLOAD,
    )
    .map(|status| status.to_string())
    .unwrap_or_else(|error| format!("cannot be read: {error}"));
    println!("  toolchain: {toolchain}");

    let _ = std::io::stdout().flush();

    // Everything this run had to say about itself is printed and flushed, so the address is on
    // screen before the desktop is asked for a window. The listener is already accepting by now:
    // `server::start` returned above, so the page a browser lands on is served rather than raced.
    report(opening.show(&Desktop, &running.url()), &running.url());

    // The listener runs on its own thread; this one waits for the process to be ended. There is
    // no shutdown flag to set from outside: an interrupt ends the process, and the operating
    // system releases the port — which is the same admission the next start relies on.
    loop {
        thread::park();
    }
}

/// A browser that would not open is worth one line and not a failed start.
///
/// The panel is this program's whole interface, so taking the start away would take the interface
/// away too — and the address is printed in either case, which is the only thing a session with no
/// desktop can act on.
fn report(outcome: Result<(), deepnest::open::OpenError>, url: &str) {
    if let Err(error) = outcome {
        eprintln!("deepnest: the panel is at {url} — {error}");
    }
}

fn config_failure(error: ConfigError) -> Failure {
    Failure::new(1, explain_config(&error))
}

/// The message, plus what to do about it. A config error is the one failure a user can fix
/// themselves, so it is the one that gets a suggestion rather than only a diagnosis.
fn explain_config(error: &ConfigError) -> String {
    let hint = match error {
        ConfigError::Unparseable { .. } => {
            "\nthe file must have this shape:\n\n  [llm]\n  base_url = \"https://api.deepseek.com\"\n  api_key  = \"sk-...\"\n  model    = \"deepseek-flash\""
        }
        ConfigError::Unusable { .. } => {
            "\nfix the value and start again; nothing was started and no token was minted"
        }
        ConfigError::Unreadable { .. } => "\ncheck the file's permissions",
        // Startup only ever reads; a write failure comes from the panel's first-run form, which
        // reports it on the page rather than through this message.
        ConfigError::Unwritable { .. } => "\ncheck that the deepnest home is writable",
    };
    format!("{error}{hint}")
}

fn token_failure(error: TokenError) -> Failure {
    Failure::new(
        1,
        format!("{error}\ndeepnest cannot start without a control token"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What this command line asks for — the argument list an iterator would have produced.
    fn parse(given: &[&str]) -> Result<Action, Failure> {
        arguments(given.iter().map(|argument| argument.to_string()))
    }

    #[test]
    fn a_bare_start_opens_the_panel() {
        // The README's promise, at the only place it can be kept: no flag means a browser window.
        assert_eq!(parse(&[]).ok(), Some(Action::Run(Opening::Browser)));
    }

    #[test]
    fn the_no_open_flag_starts_without_opening_anything() {
        // What the end-to-end tests pass so that running the suite does not open a window on the
        // machine running it.
        assert_eq!(
            parse(&["--no-open"]).ok(),
            Some(Action::Run(Opening::Never))
        );
    }

    #[test]
    fn the_opening_flag_does_not_take_the_next_argument_with_it() {
        // It carries no value, so an argument after it is still read as one.
        assert_eq!(
            parse(&["--no-open", "--version"]).ok(),
            Some(Action::Version)
        );
    }

    #[test]
    fn asking_for_help_wins_over_not_wanting_a_window() {
        // Both orders, because "help and no window" is one request however it is spelled.
        for given in [["--no-open", "--help"], ["--help", "--no-open"]] {
            assert_eq!(
                parse(&given).ok(),
                Some(Action::Help),
                "{given:?} is a request for help"
            );
        }
    }

    #[test]
    fn an_unknown_argument_is_refused_by_name() {
        let failure = parse(&["--nope"]).expect_err("an unknown argument is refused");
        assert_eq!(failure.code, 2, "a command line error is a usage error");
        assert!(failure.message.contains("--nope"), "{}", failure.message);
    }

    #[test]
    fn the_usage_text_names_the_flag_that_stops_the_browser() {
        // The flag is the switch the e2e harness depends on, so it has to be discoverable without
        // reading the source.
        assert!(USAGE.contains("--no-open"), "{USAGE}");
        assert!(USAGE.contains("open"), "{USAGE}");
    }
}
