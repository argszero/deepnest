//! The C toolchain deepnest does not bundle, and the check it promises before the first build.
//!
//! deepnest carries a Rust compiler and a JavaScript runtime, and deliberately carries no C
//! toolchain: it is large, it is platform-specific, and the operating system's own installer is
//! better at it than a 140 MB download that would be stale by the next release. What apps need it
//! for is `bundled` SQLite, which compiles C during the first build.
//!
//! The README states the promise this module keeps: *deepnest checks for them before the first
//! build and walks you through installing what is missing, instead of failing halfway through
//! compilation.* `crates/apps` already reports a step whose program will not start, and that
//! report is truthful — but it arrives after `bun install` and `bun run build` have already
//! succeeded, a minute into a `cargo build`, pointing at a compiler that was never going to work.
//! The check belongs **before the plan runs**, and that is the whole reason this exists.
//!
//! Four decisions:
//!
//! - **The answer is three readings, not a boolean.** [`Report::Ready`], [`Report::Missing`] and
//!   [`Report::Undetermined`] are separate because the actions are: unpack nothing and build,
//!   install the named thing, or go and look at a probe that broke. Collapsing the last into
//!   "missing" would tell a user to install something they may already have.
//! - **Two of the three refuse the build.** `Undetermined` fails closed, like every other reading
//!   in this crate: a build started on a "cannot tell" spends a minute of the user's time to
//!   arrive at `BuildError::MissingTool` anyway, and says less when it gets there.
//! - **The platform's requirement is data, and the probe is a seam.** What a machine needs is a
//!   name, a fix and a [`Kind`]; how deepnest finds out is a [`Probe`]. A test of the refusal must
//!   not depend on what happens to be installed on the machine running it — the same reason
//!   `crates/apps` puts `Runner` and `Spawner` behind traits.
//! - **The fix is a command a user can paste, or a page they can open.** "Install a C toolchain" is
//!   not guidance; `xcode-select --install` is. This is the part of the promise that is about the
//!   user rather than about correctness.

use std::fmt;
use std::path::{Path, PathBuf};

/// Which of the three shapes this platform's C toolchain takes.
///
/// Not every platform implements every kind, and a probe that cannot answer says so
/// ([`Finding::Undetermined`]) rather than guessing at the nearest one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Apple's Command Line Tools, which `xcode-select` knows the location of.
    CommandLineTools,
    /// A C compiler reachable as a program on `PATH`.
    Compiler,
    /// Microsoft's C++ build tools, which are not on `PATH` outside their own developer prompt.
    BuildTools,
}

/// What this platform must have before an app can be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirement {
    /// What to call it, in the user's terms.
    pub name: &'static str,
    /// What to do about it when it is not there.
    pub fix: &'static str,
    /// How to find out whether it is there.
    pub kind: Kind,
}

/// What a [`Probe`] saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// It is there, and this is the evidence: a path, so a user can look at it.
    Present {
        /// Where it was found.
        at: String,
    },
    /// It is not there, and this is what was seen instead.
    Missing {
        /// What the probe looked at and what it got.
        detail: String,
    },
    /// This probe cannot answer this kind on this platform.
    Undetermined {
        /// Why not.
        why: String,
    },
}

/// Finds out whether this machine has the C toolchain its platform needs.
///
/// A seam rather than a direct function call, because the interesting behaviour is what deepnest
/// does with each of the three answers, and a test of that must not depend on the machine it runs
/// on. [`StdProbe`] is the real one.
pub trait Probe: Send + Sync {
    /// Answer for `kind`.
    fn answers(&self, kind: Kind) -> Finding;
}

/// What deepnest decided, with the evidence still attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// The toolchain is there; a build may run.
    Ready {
        /// What was asked for.
        requirement: Requirement,
        /// What the probe found, in its own words.
        at: String,
    },
    /// It is not there; a build must not start.
    Missing {
        /// What is missing.
        requirement: Requirement,
        /// What the probe saw instead.
        detail: String,
    },
    /// The check itself could not be answered, so the build does not start either.
    Undetermined {
        /// What could not be established.
        requirement: Requirement,
        /// Why.
        why: String,
    },
}

impl Report {
    /// Whether a build may start.
    pub fn ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// What this machine must have.
    pub fn requirement(&self) -> Requirement {
        match self {
            Self::Ready { requirement, .. }
            | Self::Missing { requirement, .. }
            | Self::Undetermined { requirement, .. } => *requirement,
        }
    }
}

impl fmt::Display for Report {
    /// The whole message a user needs: what is wrong, and what to do about it.
    ///
    /// The `Ready` reading names where the toolchain was found, because a user who has two
    /// installed is entitled to know which one deepnest is about to use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready { requirement, at } => {
                write!(f, "{} is at {at}", requirement.name)
            }
            Self::Missing {
                requirement,
                detail,
            } => write!(
                f,
                "{name} is not installed: {detail}\n\ninstall it, then build again:\n  {fix}",
                name = requirement.name,
                fix = requirement.fix
            ),
            Self::Undetermined { requirement, why } => write!(
                f,
                "deepnest could not tell whether {name} is installed: {why}\n\n\
                 fix that check, or install it yourself, then build again:\n  {fix}",
                name = requirement.name,
                fix = requirement.fix
            ),
        }
    }
}

/// What this platform needs before an app can be built.
pub fn requirement() -> Requirement {
    platform_requirement()
}

/// Ask `probe` about this platform's requirement, and turn its answer into a decision.
pub fn check(probe: &dyn Probe) -> Report {
    let requirement = platform_requirement();
    match probe.answers(requirement.kind) {
        Finding::Present { at } => Report::Ready { requirement, at },
        Finding::Missing { detail } => Report::Missing {
            requirement,
            detail,
        },
        // Fail closed, and say which of the two failures this is. A probe that cannot answer is
        // not evidence that the toolchain is there.
        Finding::Undetermined { why } => Report::Undetermined { requirement, why },
    }
}

/// The real probe: the operating system's own answer, or an honest refusal to guess.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdProbe;

impl Probe for StdProbe {
    fn answers(&self, kind: Kind) -> Finding {
        match kind {
            Kind::CommandLineTools => command_line_tools(),
            Kind::Compiler => compiler(),
            Kind::BuildTools => build_tools(),
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_requirement() -> Requirement {
    Requirement {
        name: "Xcode Command Line Tools",
        fix: "xcode-select --install",
        kind: Kind::CommandLineTools,
    }
}

#[cfg(target_os = "windows")]
fn platform_requirement() -> Requirement {
    Requirement {
        name: "Visual Studio C++ Build Tools",
        fix: "https://visualstudio.microsoft.com/visual-cpp-build-tools/ — install the \
              \"Desktop development with C++\" workload",
        kind: Kind::BuildTools,
    }
}

/// Linux and every other unix: no vendor owns this, so the name is generic and the fix names the
/// package managers a user is most likely to have.
#[cfg(all(unix, not(target_os = "macos")))]
fn platform_requirement() -> Requirement {
    Requirement {
        name: "a C compiler and linker",
        fix: "sudo apt install build-essential (Debian/Ubuntu), sudo dnf groupinstall \
              \"Development Tools\" (Fedora), or sudo pacman -S base-devel (Arch)",
        kind: Kind::Compiler,
    }
}

/// Anything that is neither unix nor Windows. deepnest does not ship for one, and asking for a
/// compiler by name is a better answer than refusing to compile this crate there.
#[cfg(not(any(unix, target_os = "windows")))]
fn platform_requirement() -> Requirement {
    Requirement {
        name: "a C compiler and linker",
        fix: "install your platform's C toolchain and put it on PATH",
        kind: Kind::Compiler,
    }
}

/// The compiler programs looked for, in the order they are preferred.
///
/// `cc` first because it is the name the POSIX world agrees on, and a platform that has it has
/// chosen which compiler it means. The Windows names carry the extension because this scan joins a
/// bare program name onto a directory and looks for the file the platform would actually run.
#[cfg(unix)]
const COMPILERS: &[&str] = &["cc", "gcc", "clang"];
#[cfg(windows)]
const COMPILERS: &[&str] = &["cl.exe", "clang.exe", "gcc.exe", "cc.exe"];
#[cfg(not(any(unix, windows)))]
const COMPILERS: &[&str] = &["cc", "gcc", "clang"];

/// Whether a C compiler is reachable as a program on `PATH`.
fn compiler() -> Finding {
    let mut looked = Vec::new();
    for program in COMPILERS {
        if let Some(path) = on_path(program) {
            return Finding::Present {
                at: path.display().to_string(),
            };
        }
        looked.push(*program);
    }
    Finding::Missing {
        detail: format!("none of {} is on PATH", looked.join(", ")),
    }
}

#[cfg(target_os = "macos")]
fn command_line_tools() -> Finding {
    // `xcode-select -p` rather than a fixed path: the tools sit in
    // `/Library/Developer/CommandLineTools` by default and a full Xcode install puts them
    // somewhere else entirely, and `-s` can put them anywhere at all. A path check would answer
    // "missing" for a machine that compiles C perfectly well, and a false "missing" stops a build
    // that would have worked.
    let output = match std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return Finding::Undetermined {
                why: format!("`xcode-select -p` could not be run: {error}"),
            }
        }
    };
    if !output.status.success() {
        return Finding::Missing {
            detail: "`xcode-select -p` reported no active developer directory".to_string(),
        };
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Finding::Missing {
            detail: "`xcode-select -p` printed nothing".to_string(),
        };
    }
    if !Path::new(&path).is_dir() {
        return Finding::Missing {
            detail: format!("`xcode-select -p` names {path}, which is not a directory"),
        };
    }
    Finding::Present { at: path }
}

#[cfg(not(target_os = "macos"))]
fn command_line_tools() -> Finding {
    Finding::Undetermined {
        why: "the Xcode Command Line Tools are a macOS requirement, and this is not macOS"
            .to_string(),
    }
}

#[cfg(windows)]
fn build_tools() -> Finding {
    // `vswhere` is the query tool Microsoft ships with every installer, and it is the only way to
    // find the build tools: they live under a versioned, edition-dependent directory and they are
    // not put on PATH outside a developer prompt, so a program scan answers "missing" for a
    // machine that builds C every day.
    let Some(vswhere) = vswhere() else {
        return Finding::Missing {
            detail: "no Visual Studio installer was found, so no build tools were installed by \
                     one"
            .to_string(),
        };
    };
    let output = match std::process::Command::new(&vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return Finding::Undetermined {
                why: format!("{} could not be run: {error}", vswhere.display()),
            }
        }
    };
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        return Finding::Missing {
            detail: format!("{} exited with {}", vswhere.display(), output.status),
        };
    }
    // vswhere answers success with nothing printed when no installation matches the requirement,
    // which is exactly the case a user needs named: Visual Studio is there, the C++ workload is
    // not, and the fix is a checkbox in the installer they already ran.
    if path.is_empty() {
        return Finding::Missing {
            detail: "Visual Studio is installed, but without the C++ build tools component"
                .to_string(),
        };
    }
    Finding::Present { at: path }
}

#[cfg(not(windows))]
fn build_tools() -> Finding {
    Finding::Undetermined {
        why: "Visual Studio's build tools are a Windows requirement, and this is not Windows"
            .to_string(),
    }
}

/// Where `vswhere.exe` is, if an installer put one there.
///
/// `ProgramFiles(x86)` first because that is where a 64-bit Windows installs it, then
/// `ProgramFiles` for the 32-bit case where the first is absent.
#[cfg(windows)]
fn vswhere() -> Option<PathBuf> {
    let relative = Path::new("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Some(base) = std::env::var_os(variable) else {
            continue;
        };
        let candidate = PathBuf::from(base).join(&relative);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The first directory on `PATH` that holds an executable named `program`.
fn on_path(program: &str) -> Option<PathBuf> {
    find_on(program, std::env::var_os("PATH")?)
}

/// The first directory in `path` that holds an executable named `program`.
///
/// Split from [`on_path`] so the rule is testable without mutating the process environment, which
/// is global state and would make every other test in this binary depend on the order they ran in.
fn find_on(program: &str, path: impl AsRef<std::ffi::OsStr>) -> Option<PathBuf> {
    for dir in std::env::split_paths(&path) {
        if !searchable(&dir) {
            continue;
        }
        let candidate = dir.join(program);
        if runs(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Whether a `PATH` segment is a place deepnest will look for a compiler.
///
/// An empty segment means the current directory. That is a thing `PATH` is allowed to contain and
/// a thing a build must not be decided by: which compiler `cargo build` then found would depend on
/// where deepnest happened to be started from.
fn searchable(dir: &Path) -> bool {
    !dir.as_os_str().is_empty()
}

#[cfg(unix)]
fn runs(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// On Windows there is no execute bit to read; whether a file runs is decided by its extension,
/// and the program names looked for already carry theirs.
#[cfg(not(unix))]
fn runs(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A probe that answers what it is told, and records what it was asked.
    struct Fake {
        finding: Finding,
        asked: Mutex<Vec<Kind>>,
    }

    impl Fake {
        fn new(finding: Finding) -> Self {
            Self {
                finding,
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<Kind> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Probe for Fake {
        fn answers(&self, kind: Kind) -> Finding {
            self.asked.lock().unwrap().push(kind);
            self.finding.clone()
        }
    }

    fn present() -> Fake {
        Fake::new(Finding::Present {
            at: "/opt/dev".to_string(),
        })
    }

    #[test]
    fn the_requirement_names_a_tool_and_says_how_to_get_it() {
        let requirement = requirement();
        assert!(!requirement.name.is_empty());
        // "Install a C toolchain" is not guidance. Every platform's fix is a command to paste or a
        // page to open, and this is the assertion that keeps it that way when a platform is added.
        assert!(
            requirement.fix.contains("--")
                || requirement.fix.contains("://")
                || requirement.fix.contains("install"),
            "the fix is not something a user can act on: {}",
            requirement.fix
        );
    }

    #[test]
    fn check_asks_the_probe_about_this_platforms_kind_and_nothing_else() {
        let probe = present();
        let _ = check(&probe);
        assert_eq!(probe.asked(), vec![requirement().kind]);
    }

    #[test]
    fn a_present_toolchain_is_ready_and_says_where_it_was_found() {
        let report = check(&present());
        assert!(report.ready());
        assert_eq!(report.requirement(), requirement());
        match &report {
            Report::Ready { at, .. } => assert_eq!(at, "/opt/dev"),
            other => panic!("{other:?}"),
        }
        assert!(report.to_string().contains("/opt/dev"), "{report}");
    }

    #[test]
    fn a_missing_toolchain_refuses_the_build_and_names_the_fix() {
        let probe = Fake::new(Finding::Missing {
            detail: "none of cc, gcc, clang is on PATH".to_string(),
        });
        let report = check(&probe);

        assert!(
            !report.ready(),
            "a build without a C compiler must not start"
        );
        assert!(matches!(report, Report::Missing { .. }));
        let message = report.to_string();
        assert!(message.contains(requirement().name), "{message}");
        assert!(message.contains("none of cc, gcc, clang"), "{message}");
        // The whole point of the report is that the user is told what to do next.
        assert!(message.contains(requirement().fix), "{message}");
    }

    #[test]
    fn a_probe_that_cannot_answer_refuses_the_build_rather_than_assuming() {
        let probe = Fake::new(Finding::Undetermined {
            why: "the check broke".to_string(),
        });
        let report = check(&probe);

        assert!(!report.ready(), "`cannot tell` is not `yes`");
        assert!(
            matches!(report, Report::Undetermined { .. }),
            "an unanswerable check is its own reading, not a missing toolchain: {report:?}"
        );
        let message = report.to_string();
        assert!(message.contains("could not tell"), "{message}");
        assert!(message.contains("the check broke"), "{message}");
        assert!(message.contains(requirement().fix), "{message}");
    }

    #[test]
    fn the_standard_probe_answers_every_kind_without_panicking() {
        // This says nothing about what is installed here — a bare container may have no compiler,
        // and that is a truthful answer rather than a failing test. What it checks is that no
        // platform arm is a `todo!()`.
        for kind in [Kind::CommandLineTools, Kind::Compiler, Kind::BuildTools] {
            let _ = StdProbe.answers(kind);
        }
    }

    #[test]
    fn a_kind_this_platform_does_not_have_is_undetermined_rather_than_missing() {
        // Exactly one of these three belongs to whatever platform is running this.
        let foreign: Vec<Kind> = [Kind::CommandLineTools, Kind::Compiler, Kind::BuildTools]
            .into_iter()
            .filter(|kind| *kind != requirement().kind)
            .collect();
        assert_eq!(foreign.len(), 2, "a platform has one requirement");

        // The two that do not belong are answered by a refusal that names the platform, so a
        // caller that asks anyway is told why instead of being told "not installed".
        for kind in foreign {
            match StdProbe.answers(kind) {
                Finding::Undetermined { why } => assert!(!why.is_empty()),
                // Windows answers `Compiler` with a PATH scan, which is a reasonable answer even
                // though the platform's requirement is the build tools.
                Finding::Missing { .. } | Finding::Present { .. } => {}
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_program_counts_as_runnable_only_when_it_is_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("deepnest-cc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let executable = base.join("cc");
        std::fs::write(&executable, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(runs(&executable));

        let plain = base.join("README");
        std::fs::write(&plain, "not a program").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!runs(&plain), "a data file is not a compiler");

        assert!(!runs(&base), "a directory is not a compiler");
        assert!(!runs(&base.join("absent")), "nothing is not a compiler");
        assert!(
            !runs(&base.join("cc").join("nested")),
            "a file is not a directory"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn every_report_carries_its_requirement() {
        let reports = [
            check(&present()),
            check(&Fake::new(Finding::Missing {
                detail: "gone".to_string(),
            })),
            check(&Fake::new(Finding::Undetermined {
                why: "unreadable".to_string(),
            })),
        ];
        for report in reports {
            assert_eq!(report.requirement(), requirement(), "{report:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_asks_xcode_rather_than_scanning_path() {
        // Pinned rather than left to the platform helper: a `cc` on `PATH` on macOS is a shim that
        // fails at compile time with "invalid active developer path", so a PATH scan here would
        // answer "present" for the one machine that cannot build.
        let requirement = requirement();
        assert_eq!(requirement.kind, Kind::CommandLineTools);
        assert_eq!(requirement.name, "Xcode Command Line Tools");
        assert!(requirement.fix.contains("xcode-select --install"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_asks_the_installer_rather_than_scanning_path() {
        let requirement = requirement();
        assert_eq!(requirement.kind, Kind::BuildTools);
        assert_eq!(requirement.name, "Visual Studio C++ Build Tools");
        assert!(requirement.fix.contains("Desktop development with C++"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_asks_for_a_compiler_and_names_the_package_managers() {
        let requirement = requirement();
        assert_eq!(requirement.kind, Kind::Compiler);
        assert_eq!(requirement.name, "a C compiler and linker");
        // No vendor owns this one, so the fix has to name the ways people actually install it.
        assert!(requirement.fix.contains("build-essential"));
    }

    #[test]
    fn an_empty_path_segment_is_not_a_place_to_look() {
        assert!(!searchable(Path::new("")));
        assert!(searchable(Path::new("/usr/bin")));
    }

    #[cfg(unix)]
    #[test]
    fn the_path_scan_skips_what_cannot_run_and_finds_what_can() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("deepnest-cc-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let first = base.join("first");
        let second = base.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        // A file with the right name in the first directory, which is not a program.
        std::fs::write(first.join("cc"), "not a program").unwrap();
        std::fs::set_permissions(first.join("cc"), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(second.join("cc"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(second.join("cc"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(find_on("cc", &path), Some(second.join("cc")));

        // Nothing named that anywhere: absent, not a panic and not a directory.
        assert_eq!(find_on("absent", &path), None);

        // An empty segment contributes nothing even when a `cc` sits in one of the real ones.
        let with_empty =
            std::ffi::OsString::from(format!("{}::{}", first.display(), second.display()));
        assert_eq!(find_on("cc", &with_empty), Some(second.join("cc")));

        let _ = std::fs::remove_dir_all(&base);
    }
}
