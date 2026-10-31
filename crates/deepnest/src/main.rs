//! `deepnest` — the binary.
//!
//! The order of the steps is the design: **config, then token, then the port.** A config that
//! does not parse stops startup before anything is written, so a typo never leaves a token file
//! behind that suggests a run happened. The port comes last because taking it is the admission —
//! the second instance is refused at the bind, after its own token has been read (not minted),
//! and it exits reporting the address of the instance that already owns the port instead of
//! pretending to start.
//!
//! What this build does not do, and says so rather than half-doing: it does not open the browser.
//! The page it would open is a placeholder, and opening one on every start is worth exactly as
//! much as the panel behind it — which arrives with `crates/web`.

use std::io::Write;
use std::process::ExitCode;
use std::thread;

use deepnest::config::{self, ConfigError};
use deepnest::server::{self, BindError, ServerOptions, StartError};
use deepnest::token::{self, TokenError};
use deepnest::{home, prepare_home};

const USAGE: &str = "\
deepnest — local-first app generator

Usage:
  deepnest                start deepnest and serve http://127.0.0.1:4270/
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
    match arguments()? {
        Action::Run => serve(),
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

enum Action {
    Run,
    Help,
    Version,
}

fn arguments() -> Result<Action, Failure> {
    let mut action = Action::Run;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--help" | "-h" => action = Action::Help,
            "--version" | "-V" => action = Action::Version,
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

fn serve() -> Result<(), Failure> {
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

    let options = ServerOptions {
        port: server::DEFAULT_PORT,
        token: Some(token),
        token_path: Some(token_path.clone()),
        model: config.as_ref().map(|config| config.model().to_string()),
        apps_dir: Some(apps_dir.clone()),
        ..Default::default()
    };

    let running = match server::start(options) {
        Ok(server) => server,
        // The second instance is not an error. It says where the first one is and leaves.
        Err(StartError::Bind(BindError::InUse { addr, .. })) => {
            println!("deepnest is already running: http://{addr}/");
            return Ok(());
        }
        Err(error) => return Err(Failure::new(1, error.to_string())),
    };

    println!(
        "deepnest {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        running.url()
    );
    println!("  config: {}", config_path.display());
    println!("  token:  {} (mode 0600)", token_path.display());
    println!("  apps:   {}", apps_dir.display());
    let _ = std::io::stdout().flush();

    // The listener runs on its own thread; this one waits for the process to be ended. There is
    // no shutdown flag to set from outside: an interrupt ends the process, and the operating
    // system releases the port — which is the same admission the next start relies on.
    loop {
        thread::park();
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
    };
    format!("{error}{hint}")
}

fn token_failure(error: TokenError) -> Failure {
    Failure::new(
        1,
        format!("{error}\ndeepnest cannot start without a control token"),
    )
}
