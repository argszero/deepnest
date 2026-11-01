//! The toolchain payload this binary carries.
//!
//! Everything deepnest builds an app with — the Rust toolchain, the JavaScript runtime — is packed
//! into one archive by the release pipeline and embedded in this binary, so that the first app can
//! be built on a machine with nothing installed but the OS. The archive is unpacked on first use
//! into `~/.deepnest/toolchain/<version>/` through the contract `crates/packaging` already owns;
//! nothing about *where* a toolchain lives is decided here.
//!
//! Four decisions, and the first is the one that shapes the rest:
//!
//! - **A build with no payload embeds an empty one, and says so.** `include_bytes!` needs a file at
//!   compile time, so a build script writes the archive into `OUT_DIR` and writes zero bytes when
//!   there is none. That is not a degraded mode to hide: a development build and CI have no
//!   archive, and a binary that pretended to carry one would fail on the first build with a
//!   sentence about a corrupt payload. [`embedded`] returning `None` is the honest reading, and
//!   the build then resolves its tools from `PATH` exactly as it did before.
//! - **The unpack happens at the build, not at startup.** The binary can start in half a second
//!   today, and unpacking a toolchain on the way up would make every launch pay for a step the
//!   user may never reach. It also settles the ordering the packaging crate asks for: a build is
//!   only ever started by a request, and a request can only arrive after the port was won, so the
//!   staging directory is never contested — which is the whole reason `sweep` runs after the bind.
//! - **The payload's tools are under `bin/`, and that directory is the build's `PATH`.** One
//!   layout, named here rather than discovered: a payload that put its tools anywhere else would
//!   unpack perfectly and change nothing, and the failure would arrive as "bun: command not found"
//!   from inside a build log. A payload without `bin/` leaves `PATH` alone rather than emptying it.
//! - **The prefix comes first, and is not merged.** Prepending is what makes the bundled toolchain
//!   win; a list that mixed it with the system's would let a version already installed decide how
//!   the user's app is built, which is the one thing a self-contained binary exists to prevent.

use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use deepnest_apps::{Outcome, Runner, StdRunner, Step, STEP_TIMEOUT};
use deepnest_packaging::{Status, Unpacked};

/// The payload the build script wrote beside this crate's objects; empty when the build had none.
const EMBEDDED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/toolchain.payload"));

/// The directory inside an unpacked payload that holds its programs.
pub const BIN_DIR: &str = "bin";

/// The payload this binary carries, or `None` when it was built without one.
pub fn embedded() -> Option<&'static [u8]> {
    (!EMBEDDED.is_empty()).then_some(EMBEDDED)
}

/// What ensuring the toolchain did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    /// This binary carries no payload, so the build uses whatever the machine has.
    NotBundled,
    /// A payload was already unpacked for this build.
    Ready {
        /// The directory holding the unpacked payload.
        dir: PathBuf,
    },
    /// A payload was unpacked just now, in response to this build.
    Unpacked {
        /// The directory holding the unpacked payload.
        dir: PathBuf,
        /// How many entries the archive held, for the line the caller may print.
        entries: usize,
    },
}

/// Ensure the embedded toolchain is unpacked, unpacking it if this is the first build.
pub fn ensure(home: &Path) -> io::Result<Ensured> {
    let Some(bytes) = embedded() else {
        return Ok(Ensured::NotBundled);
    };
    ensure_from(bytes, home, deepnest_packaging::PAYLOAD)
}

/// The same, from an archive the caller supplies.
///
/// Split from [`ensure`] so that everything below can be exercised without a build that carries a
/// real toolchain: a test packs a tree and unpacks it through the same code the binary runs.
pub fn ensure_from<R: Read>(archive: R, home: &Path, payload: &str) -> io::Result<Ensured> {
    let version = deepnest_packaging::VERSION;
    match deepnest_packaging::status(home, version, payload)? {
        // Unpacking again would refuse an archive already there, and replacing a working toolchain
        // is not something an ordinary first build may do.
        Status::Ready { dir } => return Ok(Ensured::Ready { dir }),
        // Absent, interrupted and foreign all mean the same thing to this function: what is on disk
        // is not this build's toolchain, and `adopt` removes whatever is in the way.
        Status::Absent | Status::Interrupted { .. } | Status::Foreign { .. } => {}
    }

    let staging = deepnest_packaging::staging(home, version)?;
    let Unpacked { dir, entries } = deepnest_packaging::unpack(archive, staging, payload)
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(Ensured::Unpacked { dir, entries })
}

/// The directory the unpacked toolchain's programs are in, when there is one to use.
///
/// `None` covers both "no payload" and "a payload that carries no `bin/`", because the action is
/// the same for both: leave `PATH` as it is. It reads the directory rather than trusting the
/// marker, because the marker says the unpack finished and not what it put there.
pub fn bin(home: &Path) -> io::Result<Option<PathBuf>> {
    match deepnest_packaging::status(
        home,
        deepnest_packaging::VERSION,
        deepnest_packaging::PAYLOAD,
    )? {
        Status::Ready { dir } => {
            let bin = dir.join(BIN_DIR);
            Ok(bin.is_dir().then_some(bin))
        }
        _ => Ok(None),
    }
}

/// The build runner the binary uses: the real one, with the bundled toolchain in front.
///
/// It is a `Runner` rather than a function so that the unpack sits exactly where the packaging
/// crate says it belongs — on the way into a build — and so that a build that cannot get its
/// toolchain reports that instead of reporting a missing program.
#[derive(Debug, Clone)]
pub struct Toolchain {
    home: PathBuf,
    timeout: Duration,
    payload: Option<&'static [u8]>,
}

impl Toolchain {
    /// The runner for this home, carrying whatever payload the build embedded.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            timeout: STEP_TIMEOUT,
            payload: embedded(),
        }
    }

    /// The same runner over an archive the caller supplies, for a test that has no real toolchain.
    pub fn with_payload(mut self, payload: &'static [u8]) -> Self {
        self.payload = Some(payload);
        self
    }

    /// Ensure the toolchain this runner carries, which is the embedded one in the binary.
    fn carrying(&self) -> io::Result<Ensured> {
        match self.payload {
            Some(bytes) => ensure_from(bytes, &self.home, deepnest_packaging::PAYLOAD),
            None => Ok(Ensured::NotBundled),
        }
    }
}

impl Runner for Toolchain {
    fn run(&self, step: &Step, log: &Path) -> io::Result<Outcome> {
        let ensured = self.carrying();
        if let Err(error) = ensured {
            // A step, not a `MissingTool`: the program may be perfectly present, and what failed is
            // the thing that was supposed to supply it. The build reports this as the step's own
            // outcome, so the log names the cause where the user is already looking.
            return Ok(Outcome {
                code: None,
                detail: format!("the bundled toolchain could not be unpacked: {error}"),
            });
        }

        let prefix = bin(&self.home)?;
        StdRunner::with_timeout(self.timeout)
            .with_path_prefix(prefix)
            .run(step, log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-payload-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn home(&self) -> PathBuf {
            self.base.join("home")
        }

        fn tree(&self) -> PathBuf {
            self.base.join("tree")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// A payload of one file, packed the way the release pipeline packs a toolchain.
    fn payload_with(scratch: &Scratch, relative: &str, body: &str) -> Vec<u8> {
        let tree = scratch.tree();
        let path = tree.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        let mut archive = Vec::new();
        deepnest_packaging::pack(&tree, &mut archive).unwrap();
        archive
    }

    #[test]
    fn a_first_build_unpacks_the_payload_it_carries() {
        let scratch = Scratch::new("first");
        let archive = payload_with(&scratch, "bin/tool", "#!/bin/sh\nexit 0\n");

        let ensured = ensure_from(
            archive.as_slice(),
            &scratch.home(),
            deepnest_packaging::PAYLOAD,
        )
        .unwrap();
        let dir = scratch
            .home()
            .join(deepnest_packaging::DIR_NAME)
            .join(deepnest_packaging::VERSION);
        // Two entries for one file: the archive carries `bin/` as well as the file under it.
        assert_eq!(
            ensured,
            Ensured::Unpacked {
                dir: dir.clone(),
                entries: 2
            }
        );
        assert!(dir.join("bin").join("tool").is_file());
    }

    #[test]
    fn a_second_build_uses_the_toolchain_instead_of_unpacking_again() {
        let scratch = Scratch::new("second");
        let archive = payload_with(&scratch, "bin/tool", "#!/bin/sh\nexit 0\n");

        ensure_from(
            archive.as_slice(),
            &scratch.home(),
            deepnest_packaging::PAYLOAD,
        )
        .unwrap();
        // The archive is passed again on purpose: an unpack that ran twice would refuse this one
        // with an error rather than quietly replace a working toolchain, so `Ready` is what proves
        // the second call short-circuited on the marker.
        let again = ensure_from(
            archive.as_slice(),
            &scratch.home(),
            deepnest_packaging::PAYLOAD,
        )
        .unwrap();
        assert!(matches!(again, Ensured::Ready { .. }), "{again:?}");
    }

    #[test]
    fn the_bin_directory_is_what_the_payload_put_there() {
        let scratch = Scratch::new("bin");
        assert_eq!(bin(&scratch.home()).unwrap(), None, "nothing unpacked yet");

        let with_bin = payload_with(&scratch, "bin/tool", "#!/bin/sh\nexit 0\n");
        ensure_from(
            with_bin.as_slice(),
            &scratch.home(),
            deepnest_packaging::PAYLOAD,
        )
        .unwrap();
        let found = bin(&scratch.home()).unwrap().expect("no bin directory");
        assert!(found.ends_with(BIN_DIR), "{}", found.display());
    }

    #[test]
    fn a_payload_without_a_bin_directory_leaves_path_alone() {
        // A tree with no `bin/` unpacks perfectly and supplies no programs. Answering with a
        // prefix that does not exist would put a broken entry in front of every build's `PATH`.
        let scratch = Scratch::new("nobin");
        let archive = payload_with(&scratch, "share/readme.txt", "no tools here\n");

        ensure_from(
            archive.as_slice(),
            &scratch.home(),
            deepnest_packaging::PAYLOAD,
        )
        .unwrap();
        assert_eq!(bin(&scratch.home()).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_build_finds_its_tool_in_the_payload_and_not_on_the_host() {
        // The whole slice in one assertion: a program that exists only inside the carried payload
        // is what a build step runs. Before this, the same step could only find a program the
        // machine happened to have installed, which is the opposite of a self-contained binary.
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new("runs");
        // Packed after the bit is set: the packer carries the mode, and a payload whose tools lost
        // it would unpack into a directory `PATH` reaches and the kernel refuses to run.
        let tree = scratch.tree();
        let tool = tree.join("bin").join("deepnest-payload-probe");
        fs::create_dir_all(tool.parent().unwrap()).unwrap();
        fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let mut archive = Vec::new();
        deepnest_packaging::pack(&tree, &mut archive).unwrap();

        let runner =
            Toolchain::new(scratch.home()).with_payload(Box::leak(archive.into_boxed_slice()));
        let log = scratch.base.join("build.log");
        let step = Step {
            label: "probe",
            program: "deepnest-payload-probe",
            args: &[],
            cwd: scratch.base.clone(),
        };
        let outcome = runner.run(&step, &log).unwrap();
        assert!(outcome.succeeded(), "{outcome:?}");
    }

    #[test]
    fn a_binary_built_without_a_payload_says_so() {
        // CI and a development build have no archive. The reading is `None`, and the consequence is
        // that everything keeps working the way it did before the payload existed.
        assert!(
            embedded().is_none(),
            "this build unexpectedly carries a payload"
        );
        let scratch = Scratch::new("none");
        assert_eq!(ensure(&scratch.home()).unwrap(), Ensured::NotBundled);
        assert_eq!(bin(&scratch.home()).unwrap(), None);
    }
}
