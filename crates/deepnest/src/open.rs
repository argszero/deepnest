//! Handing the panel to the desktop's browser.
//!
//! `README.md` opens with "one double-click, one page in your browser", and the host's decision is
//! that same sentence: deepnest opens its own page in the default browser, and driving a browser
//! beyond that is the `browser-harness` skill's job, not the core's. This module is that one call.
//!
//! Four things it gets right:
//!
//! - **No dependency, because three lines do not need one.** Handing a URL to the desktop is one
//!   program per platform — `open` on macOS, `start` on Windows (a `cmd` builtin, so it is reached
//!   through `cmd /C`), `xdg-open` elsewhere — and a crate for it would be a build input and a
//!   supply chain to audit in exchange for those three lines.
//! - **A browser that will not open is one line, not a failed start.** The panel is this program's
//!   whole interface, so refusing to run because the desktop would not take a URL would take the
//!   interface away with it. The URL is printed either way, which is also all a session with no
//!   desktop can use.
//! - **The second start opens it too.** A double-click with deepnest already running is a person
//!   asking for the page that is already being served; printing its address and opening nothing
//!   makes the second click look like it did nothing at all.
//! - **The opener is a seam, and `--no-open` is its switch.** The end-to-end tests start the real
//!   binary, and a test that opened a browser window would open one on every machine running the
//!   suite — three platforms at once in CI. [`Opening::Never`] is what that harness asks for;
//!   [`Opener`] is what these tests answer instead of spawning anything.

use std::fmt;
use std::io;
use std::process::{Command, Stdio};

/// Something that can hand a URL to this machine's desktop.
pub trait Opener: Sync {
    /// Ask the desktop to show `url`, or say why it could not be asked.
    fn open(&self, url: &str) -> Result<(), OpenError>;
}

/// The desktop this build is running on.
pub struct Desktop;

impl Opener for Desktop {
    fn open(&self, url: &str) -> Result<(), OpenError> {
        let mut command = command(url);
        // Nothing to read and nothing to show: the opener takes a URL and tells the desktop about
        // it. Standard error is left alone on purpose — a program that is there but cannot reach a
        // browser says so there, and that is the one explanation worth passing on.
        command.stdin(Stdio::null()).stdout(Stdio::null());
        let mut child = command.spawn().map_err(|source| OpenError {
            command: describe(),
            source,
        })?;

        // The opener's whole job is to hand the URL over, and waiting for it here would hold the
        // banner behind a program the desktop may take a moment to answer. Not waiting at all
        // would leave the child for nobody to reap, so a thread waits: the start goes on, and
        // nothing accumulates. A program that ran and then failed gets its one line from there.
        let command = describe();
        let _ = std::thread::Builder::new()
            .name("deepnest-open".to_string())
            .spawn(move || match child.wait() {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    eprintln!("deepnest: `{command}` could not open the panel ({status})");
                }
                Err(error) => {
                    eprintln!("deepnest: could not wait for `{command}`: {error}");
                }
            });
        Ok(())
    }
}

/// Whether this run hands the panel to the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opening {
    /// Ask the desktop to show the panel — what a double-click is asking for.
    Browser,
    /// Print the address and open nothing: `--no-open`.
    Never,
}

impl Opening {
    /// Hand `url` over, or to nobody when this run was told not to open anything.
    ///
    /// `Ok(())` covers both "the desktop took it" and "nobody was asked", because the caller's
    /// question is the same either way — was there anything to report? The opener's own failure is
    /// returned rather than printed here, so the caller can say where the panel is in the same
    /// breath as saying the browser did not take it.
    pub fn show(&self, opener: &dyn Opener, url: &str) -> Result<(), OpenError> {
        match self {
            Opening::Browser => opener.open(url),
            Opening::Never => Ok(()),
        }
    }
}

/// A browser that could not be asked, and the program that would have asked it.
#[derive(Debug)]
pub struct OpenError {
    /// The command as a person would type it.
    command: &'static str,
    /// What went wrong when it was run.
    source: io::Error,
}

impl OpenError {
    /// The command that failed, as a person would type it.
    pub fn command(&self) -> &'static str {
        self.command
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "could not run `{}`: {}",
            self.command, self.source
        )
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The command that hands `url` to this platform's default browser.
pub fn command(url: &str) -> Command {
    platform(url)
}

/// The command, spelled the way a person would type it.
///
/// A separate answer from [`command`] because the program name is not the whole command on
/// Windows, and a message that said "could not run `cmd`" would send the reader looking for a
/// shell they never asked for.
pub fn describe() -> &'static str {
    spelled()
}

#[cfg(target_os = "macos")]
fn platform(url: &str) -> Command {
    let mut command = Command::new("open");
    command.arg(url);
    command
}

#[cfg(target_os = "macos")]
fn spelled() -> &'static str {
    "open"
}

#[cfg(target_os = "windows")]
fn platform(url: &str) -> Command {
    let mut command = Command::new("cmd");
    // `start` is a `cmd` builtin rather than a program, and its first quoted argument is a window
    // *title* — so the empty string is what stops the URL from being read as one. Without it the
    // browser is asked to open a window called `http://127.0.0.1:4270/` and opens nothing.
    command.args(["/C", "start", ""]).arg(url);
    command
}

#[cfg(target_os = "windows")]
fn spelled() -> &'static str {
    "cmd /C start"
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform(url: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(url);
    command
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn spelled() -> &'static str {
    "xdg-open"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    const PANEL: &str = "http://127.0.0.1:4270/";

    /// What this platform's opener program is called.
    #[cfg(target_os = "macos")]
    fn program() -> &'static str {
        "open"
    }

    #[cfg(target_os = "windows")]
    fn program() -> &'static str {
        "cmd"
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn program() -> &'static str {
        "xdg-open"
    }

    /// The arguments this platform's opener needs to be handed a URL.
    #[cfg(target_os = "macos")]
    fn arguments(url: &str) -> Vec<OsString> {
        vec![url.into()]
    }

    #[cfg(target_os = "windows")]
    fn arguments(url: &str) -> Vec<OsString> {
        ["/C", "start", "", url]
            .into_iter()
            .map(OsString::from)
            .collect()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn arguments(url: &str) -> Vec<OsString> {
        vec![url.into()]
    }

    /// An opener that records what it was asked, or fails without asking anything.
    struct Watched {
        asked: Mutex<Vec<String>>,
        fails: bool,
    }

    impl Watched {
        fn new(fails: bool) -> Self {
            Self {
                asked: Mutex::new(Vec::new()),
                fails,
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked
                .lock()
                .expect("the recorder is not poisoned")
                .clone()
        }
    }

    impl Opener for Watched {
        fn open(&self, url: &str) -> Result<(), OpenError> {
            if self.fails {
                return Err(OpenError {
                    command: describe(),
                    source: io::Error::new(io::ErrorKind::NotFound, "no such program"),
                });
            }
            self.asked
                .lock()
                .expect("the recorder is not poisoned")
                .push(url.to_string());
            Ok(())
        }
    }

    #[test]
    fn the_url_is_the_whole_of_what_this_platforms_opener_is_told() {
        let command = command(PANEL);
        assert_eq!(command.get_program(), program());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            arguments(PANEL),
            "the opener is handed the URL and nothing else"
        );
    }

    #[test]
    fn the_name_in_a_failure_is_the_command_a_person_would_type() {
        // Windows is the case this is for: the program is `cmd`, and the command a reader needs to
        // recognise is `cmd /C start`. Every platform's answer has to be a command they could run.
        let spelled = describe();
        assert_eq!(spelled, spelled.trim(), "no padding in a message");
        assert!(spelled.starts_with(program()), "{spelled}");
    }

    #[test]
    fn a_run_that_opens_hands_the_panel_to_the_desktop_once() {
        let watched = Watched::new(false);
        let outcome = Opening::Browser.show(&watched, PANEL);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(watched.asked(), vec![PANEL.to_string()]);
    }

    #[test]
    fn a_run_told_not_to_open_asks_nobody_and_is_not_a_failure() {
        let watched = Watched::new(false);
        let outcome = Opening::Never.show(&watched, PANEL);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(
            watched.asked().is_empty(),
            "`--no-open` must not reach the desktop at all"
        );
    }

    #[test]
    fn a_browser_that_cannot_be_asked_says_which_program_and_why() {
        let watched = Watched::new(true);
        let error = Opening::Browser
            .show(&watched, PANEL)
            .expect_err("an opener that cannot run is an error");
        assert_eq!(error.command(), describe());
        let message = error.to_string();
        assert!(message.contains(describe()), "{message}");
        assert!(message.contains("no such program"), "{message}");
        assert!(
            watched.asked().is_empty(),
            "a failed attempt asked nothing, so it recorded nothing"
        );
    }
}
