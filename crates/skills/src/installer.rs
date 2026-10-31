//! Installing what the catalog names: the package that publishes its own skill file.
//!
//! An entry with `install: self-publishing` is fetched by running the package itself. That is the
//! whole idea, and it is a good one: the package knows which files it publishes, so deepnest does
//! not have to know a file list that changes every release. This module drives that one step and
//! records what came back — run the program, take what it printed, check it is a skill the loader
//! will accept, put it where `dest` says, and record it as managed so the release check may refresh
//! it later (`impl, ref: emrg/skills/installer.py`).
//!
//! The `check: github_release` half is **not** here. It answers a different question — is there a
//! newer one? — with a different mechanism, an HTTP client that this crate does not have and does
//! not want for this. The question this module answers is asked first, by the user, once.
//!
//! # What this refuses, and why refusing is most of the work
//!
//! Installing writes a file into a directory the user also owns, from a program the user installed,
//! on the strength of a catalog the user can edit. Four of the refusals below exist because the
//! reference implementation would have done the thing it is refusing:
//!
//! - **The publisher is the entry's own name, not a constant.** The reference sets one `CLI_NAME`
//!   for every entry, so the second catalog entry would run the first one's program
//!   (`impl, ref: emrg/skills/installer.py`). A name that cannot be resolved is named in the
//!   refusal along with the repository it comes from, which is what a person needs to fix it.
//! - **A file deepnest did not write is never overwritten.** `managed` in the state file is the
//!   record of provenance, and doubt is not permission: an entry the state file does not know about
//!   whose file is already on disk is the user's, the same reading a missing `managed` field gets.
//!   The reference writes `dest/<name>.md` over whatever is there.
//! - **The published header must declare the name the catalog asked for.** The reference writes the
//!   file under the *published* name and records state under the *catalog* name, so the two
//!   disagree and the update loop then looks up a skill that is not there under that name.
//! - **Nothing is recorded that is not on disk.** The file is written first and the state second,
//!   so an interrupted install leaves an unrecorded file — which the next install refuses to
//!   overwrite, the safe direction — rather than a managed entry pointing at nothing.
//!
//! Two more follow from the crate's own rules rather than from the reference: text with no
//! `description` is not a skill the loader will read, so publishing one would write a file that
//! does nothing; and a `dest` that is neither absolute nor under `~` has no defined meaning here,
//! so it is refused instead of resolved against whatever directory the process was started in.
//!
//! **No version is recorded.** The reference writes `version: "unknown"` when its release check
//! fails; the state model already has a better answer for "the source did not say" — the field is
//! absent — and there is no release check in this slice to ask.
//!
//! **The real publisher waits as long as the program takes.** A package that hangs hangs the caller
//! that waits on it. That is the honest limit of this slice: nothing in the binary calls this yet,
//! and whoever wires it in owes it either a worker or a timeout. The [`Publisher`] seam is where
//! that decision goes without touching any of the logic here.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::atomic;
use crate::catalog::Entry;
use crate::frontmatter;
use crate::loader::SKILL_FILE;
use crate::state::State;

/// The `install:` value this build knows how to drive.
pub const SELF_PUBLISHING: &str = "self-publishing";

/// The subcommand a self-publishing package answers to.
///
/// One word, from the reference (`impl, ref: emrg/skills/installer.py`): a package that publishes
/// its own skill file does so on request, and `skill` is the request.
pub const PUBLISH_ARG: &str = "skill";

/// What a publish command produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The exit code, or `None` when a signal ended the process.
    pub status: Option<i32>,
    /// What it printed — the skill file, when it worked.
    pub stdout: String,
    pub stderr: String,
}

impl Report {
    /// Whether the command says it succeeded.
    pub fn succeeded(&self) -> bool {
        self.status == Some(0)
    }
}

/// The seam between deciding to install and running a program on the user's machine.
///
/// A trait for the same reason `Runner`, `Spawner` and `Probe` are traits in this workspace: every
/// one of the refusals below is about what happens *after* a program has been run, and a test that
/// ran a real one would be measuring whatever that program does on the machine running the suite.
pub trait Publisher: Send + Sync {
    /// Run `<program> skill` and report what happened.
    fn publish(&self, program: &str) -> io::Result<Report>;
}

/// The real publisher: the program, run with no shell in between.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdPublisher;

impl Publisher for StdPublisher {
    fn publish(&self, program: &str) -> io::Result<Report> {
        let output = std::process::Command::new(program)
            .arg(PUBLISH_ARG)
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(Report {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// A skill that is now on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// The name the catalog asked for and the file declares.
    pub name: String,
    /// The file itself.
    pub path: PathBuf,
    /// Whether this replaced a skill deepnest had installed before, rather than arriving fresh.
    pub refreshed: bool,
}

/// What came of an install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The file is on disk, and the state records it as managed.
    Installed(Placed),
    /// Nothing was written.
    Refused(Refused),
}

impl Outcome {
    /// The refusal, when there was one — for a caller that only wants to know why not.
    pub fn refusal(&self) -> Option<&Refused> {
        match self {
            Self::Installed(_) => None,
            Self::Refused(refused) => Some(refused),
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Installed(placed) => {
                let verb = if placed.refreshed {
                    "refreshed"
                } else {
                    "installed"
                };
                write!(f, "{} {} at {}", verb, placed.name, placed.path.display())
            }
            Self::Refused(refused) => write!(f, "{refused}"),
        }
    }
}

/// Why an install did not happen.
///
/// Every variant is a different fix, which is why they are variants rather than one message with a
/// detail string: a catalog that omits a field, a package that is not installed, a package that ran
/// and failed, and a file that is somebody else's are four different situations for the person
/// reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The catalog names the skill but not how to fetch it, so there is nothing to run.
    NotOffered {
        name: String,
        missing: Vec<&'static str>,
    },
    /// `install:` names a way of fetching this build does not know.
    UnknownKind { name: String, kind: String },
    /// `dest` is neither an absolute path nor one under `~`.
    BadDest { name: String, dest: String },
    /// Something is already there, and deepnest did not put it there.
    NotItsToReplace { name: String, path: PathBuf },
    /// The program that would publish the file is not installed.
    PublisherMissing {
        program: String,
        repo: Option<String>,
    },
    /// The program ran and did not produce a skill.
    PublisherFailed { program: String, detail: String },
    /// The program printed something, but it is not a skill the loader would read.
    UnusablePublish { program: String, why: String },
    /// The destination could not be written to.
    CannotWrite { path: PathBuf, error: String },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotOffered { name, missing } => write!(
                f,
                "`{name}` is recommended only: the catalog does not say where to fetch it \
                 (no {})",
                missing.join(", no ")
            ),
            Self::UnknownKind { name, kind } => write!(
                f,
                "`{name}` installs by `{kind}`, which this build does not know how to drive — \
                 only `{SELF_PUBLISHING}` is understood"
            ),
            Self::BadDest { name, dest } => write!(
                f,
                "`{name}` says to install into `{dest}`, which is neither an absolute path nor \
                 under `~`, so there is nowhere it unambiguously means"
            ),
            Self::NotItsToReplace { name, path } => write!(
                f,
                "`{name}` is already at {} and deepnest did not put it there, so it will not \
                 overwrite it — move it aside, then install again",
                path.display()
            ),
            Self::PublisherMissing { program, repo } => {
                write!(
                    f,
                    "`{program}` publishes its own skill file but is not installed, so `{program} \
                     {PUBLISH_ARG}` cannot be run"
                )?;
                match repo {
                    Some(repo) => write!(f, " — it comes from https://github.com/{repo}"),
                    None => Ok(()),
                }
            }
            Self::PublisherFailed { program, detail } => {
                write!(f, "`{program} {PUBLISH_ARG}` failed: {detail}")
            }
            Self::UnusablePublish { program, why } => write!(
                f,
                "`{program} {PUBLISH_ARG}` did not publish a usable skill: {why}"
            ),
            Self::CannotWrite { path, error } => {
                write!(f, "cannot write {}: {error}", path.display())
            }
        }
    }
}

/// Install one catalog entry, and record it as managed.
///
/// `home` is the user's home directory, which is what a `~` in `dest` resolves against — passed in
/// rather than looked up so a test never writes into the real one. `at` is the caller's stamp; this
/// crate does not read the clock. `state` is updated in place and **not** written: persisting it is
/// the caller's [step](State::write), and that order is deliberate — see the module doc.
pub fn install(
    entry: &Entry,
    home: &Path,
    state: &mut State,
    publisher: &dyn Publisher,
    at: &str,
) -> Outcome {
    if !entry.is_installable() {
        return Outcome::Refused(Refused::NotOffered {
            name: entry.name.clone(),
            missing: missing_for(entry),
        });
    }

    // `is_installable` guarantees these are present, so their absence here is unreachable rather
    // than an error to report; the match keeps that explicit instead of unwrapping.
    let (Some(kind), Some(dest)) = (entry.install.as_deref(), entry.dest.as_deref()) else {
        return Outcome::Refused(Refused::NotOffered {
            name: entry.name.clone(),
            missing: missing_for(entry),
        });
    };

    if kind != SELF_PUBLISHING {
        return Outcome::Refused(Refused::UnknownKind {
            name: entry.name.clone(),
            kind: kind.to_string(),
        });
    }

    let Some(dest) = resolve_dest(dest, home) else {
        return Outcome::Refused(Refused::BadDest {
            name: entry.name.clone(),
            dest: dest.to_string(),
        });
    };

    // Provenance, both halves of it. The state file says whether deepnest put this here, and the
    // disk says whether anything is here at all; either one alone is a way to overwrite somebody's
    // work, because a state file can be missing and a file can be deleted.
    let already_managed = state.get(&entry.name).is_some_and(|i| i.managed);
    if !already_managed {
        if let Some(path) = occupied(&dest, &entry.name) {
            return Outcome::Refused(Refused::NotItsToReplace {
                name: entry.name.clone(),
                path,
            });
        }
        if state.get(&entry.name).is_some() {
            // The state file knows this name and says it is not deepnest's. A refusal even when the
            // file is gone: the record is about who owns the name, and the user may be about to
            // put it back.
            return Outcome::Refused(Refused::NotItsToReplace {
                name: entry.name.clone(),
                path: dest.join(&entry.name).join(SKILL_FILE),
            });
        }
    }

    let report = match publisher.publish(&entry.name) {
        Ok(report) => report,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Outcome::Refused(Refused::PublisherMissing {
                program: entry.name.clone(),
                repo: entry.repo.clone(),
            })
        }
        Err(error) => {
            return Outcome::Refused(Refused::PublisherFailed {
                program: entry.name.clone(),
                detail: error.to_string(),
            })
        }
    };

    if !report.succeeded() {
        return Outcome::Refused(Refused::PublisherFailed {
            program: entry.name.clone(),
            detail: describe_failure(&report),
        });
    }

    let text = report.stdout.trim();
    if text.is_empty() {
        return Outcome::Refused(Refused::PublisherFailed {
            program: entry.name.clone(),
            detail: format!("it printed nothing, exit {}", describe_status(&report)),
        });
    }

    if let Err(why) = check_published(&entry.name, text) {
        return Outcome::Refused(Refused::UnusablePublish {
            program: entry.name.clone(),
            why,
        });
    }

    // The name the file declares, which `check_published` has just confirmed is the entry's own.
    let path = dest.join(&entry.name).join(SKILL_FILE);
    if let Err(error) = atomic::write(&path, &format!("{text}\n")) {
        return Outcome::Refused(Refused::CannotWrite {
            path,
            error: error.to_string(),
        });
    }

    // Keep whatever the state already knew about the version: this slice has no way to learn a new
    // one, and forgetting the old one would be worse than not improving it.
    let version = state.get(&entry.name).and_then(|i| i.version.clone());
    if already_managed {
        state.record_update(&entry.name, version, at);
    } else {
        state.record_install(&entry.name, version, at);
    }

    Outcome::Installed(Placed {
        name: entry.name.clone(),
        path,
        refreshed: already_managed,
    })
}

/// Which of the three fetch fields the entry is missing.
fn missing_for(entry: &Entry) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if entry.repo.is_none() {
        missing.push("repo");
    }
    if entry.install.is_none() {
        missing.push("install");
    }
    if entry.dest.is_none() {
        missing.push("dest");
    }
    missing
}

/// Where a catalog `dest` points, or `None` when it does not unambiguously say.
///
/// `~` and `~/…` resolve against `home`. `~user/…` does not: expanding it needs the system's user
/// table, and treating it as a literal relative directory would create one named `~user` wherever
/// the process happened to be started. A relative path is refused for the same reason — the
/// meaning of `skills/` depends on something deepnest does not control.
pub fn resolve_dest(dest: &str, home: &Path) -> Option<PathBuf> {
    if let Some(rest) = dest.strip_prefix('~') {
        if rest.is_empty() {
            return Some(home.to_path_buf());
        }
        let rest = rest.strip_prefix('/')?;
        return Some(home.join(rest));
    }
    let path = Path::new(dest);
    path.is_absolute().then(|| path.to_path_buf())
}

/// A file already standing where this skill would go, in either of the two layouts the loader reads.
fn occupied(dest: &Path, name: &str) -> Option<PathBuf> {
    let directory = dest.join(name).join(SKILL_FILE);
    if directory.exists() {
        return Some(directory);
    }
    let flat = dest.join(format!("{name}.md"));
    flat.exists().then_some(flat)
}

/// Whether the published text is a skill, and the skill that was asked for.
///
/// The name check is the one the reference implementation does not make, and the reason is worth
/// keeping: the state file is keyed by the name the catalog used, so a file that declares a
/// different one is a file the update loop will never find again.
fn check_published(expected: &str, text: &str) -> Result<(), String> {
    let (fields, _) = frontmatter::split(text)
        .map_err(|error| format!("it printed text with no header ({error})"))?;
    let name = fields.get("name").unwrap_or_default().trim();
    let description = fields.get("description").unwrap_or_default().trim();
    if name.is_empty() {
        return Err("its header has no name".to_string());
    }
    if description.is_empty() {
        return Err(format!(
            "`{name}` has no description, and a skill without one is not loaded"
        ));
    }
    if name != expected {
        return Err(format!(
            "it declares itself `{name}` while the catalog asked for `{expected}`, so a record \
             under one name would never match the file under the other"
        ));
    }
    Ok(())
}

/// One line saying how the command ended, for a refusal that has to explain it.
fn describe_failure(report: &Report) -> String {
    let mut detail = format!("exit {}", describe_status(report));
    let tail = report.stderr.trim();
    if !tail.is_empty() {
        // The last line rather than the whole thing: a failed install can print pages, and the
        // line that says why is at the end.
        let last = tail.lines().last().unwrap_or_default().trim();
        if !last.is_empty() {
            detail.push_str(&format!(": {last}"));
        }
    }
    detail
}

fn describe_status(report: &Report) -> String {
    match report.status {
        Some(code) => code.to_string(),
        None => "no exit code".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::state;

    /// A path rooted at this host's filesystem root: `/var/tmp/skills` on unix, `C:\var\tmp\skills`
    /// on Windows.
    ///
    /// A test that spells out `/var/tmp/skills` is absolute on unix and merely drive-relative on
    /// Windows, so it measures the platform rather than the rule being tested. `resolve_dest`
    /// refuses a relative path by design, which is why the literal made this suite red on Windows
    /// alone.
    #[cfg(windows)]
    fn an_absolute_path(rest: &str) -> PathBuf {
        PathBuf::from(format!("C:\\{}", rest.replace('/', "\\")))
    }

    #[cfg(not(windows))]
    fn an_absolute_path(rest: &str) -> PathBuf {
        PathBuf::from(format!("/{rest}"))
    }

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-installer-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn home(&self) -> PathBuf {
            self.base.join("home")
        }

        /// The skills directory a catalog entry points at, spelled the way the bundled one is.
        fn dest(&self) -> PathBuf {
            self.home().join(".deepnest").join("skills")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// A publisher that answers whatever the test says, and remembers being asked.
    struct Fake {
        answer: Box<dyn Fn() -> io::Result<Report> + Send + Sync>,
        called: Mutex<Vec<String>>,
    }

    impl Fake {
        fn printing(text: &str) -> Self {
            let text = text.to_string();
            Self {
                answer: Box::new(move || {
                    Ok(Report {
                        status: Some(0),
                        stdout: text.clone(),
                        stderr: String::new(),
                    })
                }),
                called: Mutex::new(Vec::new()),
            }
        }

        fn failing(status: i32, stderr: &str) -> Self {
            let stderr = stderr.to_string();
            Self {
                answer: Box::new(move || {
                    Ok(Report {
                        status: Some(status),
                        stdout: String::new(),
                        stderr: stderr.clone(),
                    })
                }),
                called: Mutex::new(Vec::new()),
            }
        }

        fn missing() -> Self {
            Self {
                answer: Box::new(|| Err(io::Error::new(io::ErrorKind::NotFound, "no such file"))),
                called: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.called.lock().unwrap().clone()
        }
    }

    impl Publisher for Fake {
        fn publish(&self, program: &str) -> io::Result<Report> {
            self.called.lock().unwrap().push(program.to_string());
            (self.answer)()
        }
    }

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.to_string(),
            description: "Direct browser control.".to_string(),
            repo: Some("browser-use/browser-harness".to_string()),
            install: Some(SELF_PUBLISHING.to_string()),
            dest: Some("~/.deepnest/skills/".to_string()),
            check: Some("github_release".to_string()),
        }
    }

    fn published(name: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: Always use this for any web interaction.\n---\n\
             # browser-harness\n\nDrive the user's browser.\n"
        )
    }

    #[test]
    fn a_self_publishing_entry_lands_in_a_directory_of_its_own() {
        let scratch = Scratch::new("fresh");
        let mut state = State::new();
        let publisher = Fake::printing(&published("browser-harness"));

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &publisher,
            "2026-10-31T12:00:00Z",
        );

        let Outcome::Installed(placed) = &outcome else {
            panic!("expected an install, got {outcome}");
        };
        assert_eq!(placed.name, "browser-harness");
        assert_eq!(
            placed.path,
            scratch.dest().join("browser-harness").join(SKILL_FILE)
        );
        assert!(!placed.refreshed);
        // The directory layout, because that is the layout `~/.deepnest/skills/<name>/SKILL.md`
        // names and the one the loader prefers.
        let text = std::fs::read_to_string(&placed.path).unwrap();
        assert!(text.starts_with("---\nname: browser-harness"), "{text}");
        assert!(text.ends_with('\n'), "{text}");
        assert_eq!(publisher.calls(), ["browser-harness"]);
    }

    #[test]
    fn the_state_records_it_as_managed_and_without_a_version_it_could_not_know() {
        let scratch = Scratch::new("managed");
        let mut state = State::new();
        install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing(&published("browser-harness")),
            "2026-10-31T12:00:00Z",
        );

        assert!(state.may_refresh("browser-harness"));
        let recorded = state.get("browser-harness").unwrap();
        assert_eq!(
            recorded.installed_at.as_deref(),
            Some("2026-10-31T12:00:00Z")
        );
        // Absent, not "unknown": nothing here asked a release for a version.
        assert_eq!(recorded.version, None);
    }

    #[test]
    fn a_recommended_only_entry_is_refused_before_anything_runs() {
        let scratch = Scratch::new("recommended");
        let mut state = State::new();
        let mut recommended = entry("baoyu-design");
        recommended.repo = None;
        recommended.install = None;
        recommended.dest = None;
        let publisher = Fake::printing(&published("baoyu-design"));

        let outcome = install(&recommended, &scratch.home(), &mut state, &publisher, "t");

        let refused = outcome.refusal().expect("refused");
        assert!(
            matches!(refused, Refused::NotOffered { missing, .. }
                if missing == &["repo", "install", "dest"]),
            "{refused}"
        );
        // Nothing ran, nothing was written, nothing was recorded.
        assert!(publisher.calls().is_empty());
        assert!(!scratch.dest().exists());
        assert!(state.is_empty());
    }

    #[test]
    fn an_entry_with_no_repository_is_not_installed_even_though_it_says_how() {
        // The catalog calls an entry installable only when all three of `repo`, `install` and
        // `dest` are there, and this list and `Catalog::installable` have to agree about the same
        // entry: installing one the catalog itself calls recommended-only would make the two
        // halves disagree about what deepnest will fetch.
        let scratch = Scratch::new("no-repo");
        let mut state = State::new();
        let mut nowhere = entry("browser-harness");
        nowhere.repo = None;
        let publisher = Fake::printing(&published("browser-harness"));

        let outcome = install(&nowhere, &scratch.home(), &mut state, &publisher, "t");

        let refused = outcome.refusal().expect("refused");
        assert!(
            matches!(refused, Refused::NotOffered { missing, .. } if missing == &["repo"]),
            "{refused}"
        );
        assert!(publisher.calls().is_empty());
        assert!(!scratch.dest().exists());
    }

    #[test]
    fn an_install_kind_this_build_does_not_know_is_refused_by_name() {
        let scratch = Scratch::new("unknown-kind");
        let mut state = State::new();
        let mut cloned = entry("notes");
        cloned.install = Some("git-clone".to_string());
        let publisher = Fake::printing(&published("notes"));

        let outcome = install(&cloned, &scratch.home(), &mut state, &publisher, "t");

        let refused = outcome.refusal().expect("refused");
        assert!(
            matches!(refused, Refused::UnknownKind { kind, .. } if kind == "git-clone"),
            "{refused}"
        );
        assert!(refused.to_string().contains("self-publishing"), "{refused}");
        assert!(publisher.calls().is_empty());
    }

    #[test]
    fn a_skill_the_user_put_there_is_never_overwritten() {
        let scratch = Scratch::new("user-skill");
        let dir = scratch.dest().join("browser-harness");
        std::fs::create_dir_all(&dir).unwrap();
        let theirs = dir.join(SKILL_FILE);
        std::fs::write(
            &theirs,
            "---\nname: browser-harness\ndescription: mine\n---\nmy edits\n",
        )
        .unwrap();

        let mut state = State::new();
        state.record_user_skill("browser-harness");
        let publisher = Fake::printing(&published("browser-harness"));

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &publisher,
            "t",
        );

        let refused = outcome.refusal().expect("refused");
        assert!(
            matches!(refused, Refused::NotItsToReplace { path, .. } if path == &theirs),
            "{refused}"
        );
        assert!(publisher.calls().is_empty(), "nothing may run");
        // The user's file is exactly as it was, and the state still says it is theirs.
        assert_eq!(
            std::fs::read_to_string(&theirs).unwrap(),
            "---\nname: browser-harness\ndescription: mine\n---\nmy edits\n"
        );
        assert!(!state.may_refresh("browser-harness"));
    }

    #[test]
    fn a_file_the_state_file_does_not_know_is_not_overwritten_either() {
        // No state at all — a fresh install, or a state file that was lost. The disk says something
        // is there, and that is enough: doubt is not permission.
        let scratch = Scratch::new("unknown-provenance");
        std::fs::create_dir_all(scratch.dest()).unwrap();
        let flat = scratch.dest().join("browser-harness.md");
        std::fs::write(&flat, "something the user wrote").unwrap();

        let mut state = State::new();
        let publisher = Fake::printing(&published("browser-harness"));
        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &publisher,
            "t",
        );

        let refused = outcome.refusal().expect("refused");
        // The flat layout is a layout the loader reads, so it counts as occupied.
        assert!(
            matches!(refused, Refused::NotItsToReplace { path, .. } if path == &flat),
            "{refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&flat).unwrap(),
            "something the user wrote"
        );
    }

    #[test]
    fn a_name_the_state_file_records_as_the_users_is_refused_even_with_no_file() {
        // The record is about who owns the name, not about what is at the path today: a file the
        // user deleted while editing it elsewhere must not come back as deepnest's.
        let scratch = Scratch::new("named-by-user");
        let mut state = State::new();
        state.record_user_skill("browser-harness");
        let publisher = Fake::printing(&published("browser-harness"));

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &publisher,
            "t",
        );

        assert!(
            matches!(outcome.refusal(), Some(Refused::NotItsToReplace { .. })),
            "{outcome}"
        );
        assert!(publisher.calls().is_empty());
        assert!(!state.may_refresh("browser-harness"));
        assert!(!scratch.dest().exists());
    }

    #[test]
    fn a_publisher_that_is_not_installed_is_named_with_its_repository() {
        let scratch = Scratch::new("missing-cli");
        let mut state = State::new();

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::missing(),
            "t",
        );

        let refused = outcome.refusal().expect("refused");
        assert!(
            matches!(refused, Refused::PublisherMissing { program, repo }
                if program == "browser-harness"
                    && repo.as_deref() == Some("browser-use/browser-harness")),
            "{refused}"
        );
        // The fix is a place to go and get it, not just a diagnosis.
        let message = refused.to_string();
        assert!(
            message.contains("https://github.com/browser-use/browser-harness"),
            "{message}"
        );
        assert!(!scratch.dest().exists());
        assert!(state.is_empty());
    }

    #[test]
    fn a_publisher_that_fails_writes_nothing_and_says_what_it_said() {
        let scratch = Scratch::new("publish-failed");
        let mut state = State::new();

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::failing(2, "warning: something\nError: no browser found\n"),
            "t",
        );

        let refused = outcome.refusal().expect("refused");
        assert!(
            matches!(refused, Refused::PublisherFailed { detail, .. } if detail.contains("exit 2")),
            "{refused}"
        );
        // The line that says why, not the whole log.
        let message = refused.to_string();
        assert!(message.contains("Error: no browser found"), "{message}");
        assert!(!message.contains("warning: something"), "{message}");
        assert!(!scratch.dest().exists());
        assert!(state.is_empty());
    }

    #[test]
    fn a_publisher_that_prints_nothing_is_a_refusal_not_an_empty_skill() {
        let scratch = Scratch::new("empty");
        let mut state = State::new();

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing("   \n"),
            "t",
        );

        assert!(
            matches!(outcome.refusal(), Some(Refused::PublisherFailed { .. })),
            "{outcome}"
        );
        assert!(state.is_empty());
    }

    #[test]
    fn published_text_without_a_description_is_not_a_skill_the_loader_would_read() {
        let scratch = Scratch::new("no-description");
        let mut state = State::new();

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing("---\nname: browser-harness\n---\n# nothing to decide from\n"),
            "t",
        );

        let refused = outcome.refusal().expect("refused");
        assert!(
            matches!(refused, Refused::UnusablePublish { why, .. }
                if why.contains("no description")),
            "{refused}"
        );
        assert!(!scratch.dest().exists());
    }

    #[test]
    fn published_text_with_no_header_at_all_is_refused() {
        let scratch = Scratch::new("no-header");
        let mut state = State::new();

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing("# just some markdown\n"),
            "t",
        );

        assert!(
            matches!(outcome.refusal(), Some(Refused::UnusablePublish { .. })),
            "{outcome}"
        );
    }

    #[test]
    fn a_published_name_that_disagrees_with_the_catalog_is_refused() {
        // Written under the published name and recorded under the catalog name — the reference
        // implementation's behaviour — is a file the update loop can never find again.
        let scratch = Scratch::new("name-disagrees");
        let mut state = State::new();

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing(&published("browser-harness-cli")),
            "t",
        );

        let refused = outcome.refusal().expect("refused");
        assert!(
            matches!(refused, Refused::UnusablePublish { why, .. }
                if why.contains("browser-harness-cli") && why.contains("browser-harness")),
            "{refused}"
        );
        assert!(!scratch.dest().exists());
        assert!(state.is_empty());
    }

    #[test]
    fn reinstalling_a_managed_skill_refreshes_it_and_keeps_when_it_first_arrived() {
        let scratch = Scratch::new("refresh");
        let mut state = State::new();
        install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing(&published("browser-harness")),
            "2026-10-31T12:00:00Z",
        );

        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing(&published("browser-harness")),
            "2026-11-01T12:00:00Z",
        );

        let Outcome::Installed(placed) = &outcome else {
            panic!("a managed skill may be replaced, got {outcome}");
        };
        assert!(placed.refreshed);
        let recorded = state.get("browser-harness").unwrap();
        assert_eq!(
            recorded.installed_at.as_deref(),
            Some("2026-10-31T12:00:00Z"),
            "a refresh is not a new arrival"
        );
        assert_eq!(recorded.updated_at.as_deref(), Some("2026-11-01T12:00:00Z"));
    }

    #[test]
    fn a_refresh_does_not_forget_the_version_it_already_had() {
        let scratch = Scratch::new("keep-version");
        let mut state = State::new();
        state.record_install("browser-harness", Some("0.1.8".to_string()), "t0");

        install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing(&published("browser-harness")),
            "t1",
        );

        assert_eq!(
            state.get("browser-harness").unwrap().version.as_deref(),
            Some("0.1.8"),
            "this slice cannot learn a newer one, and must not lose the old one"
        );
    }

    #[test]
    fn a_tilde_dest_resolves_against_the_home_it_is_given() {
        let home = Path::new("/home/somebody");
        assert_eq!(
            resolve_dest("~/.deepnest/skills/", home),
            Some(home.join(".deepnest").join("skills"))
        );
        assert_eq!(resolve_dest("~", home), Some(home.to_path_buf()));
        // Not under `~`, so it stands as written and is not joined onto `home`.
        let outside = an_absolute_path("var/tmp/skills");
        assert_eq!(
            resolve_dest(outside.to_str().unwrap(), home),
            Some(outside.clone())
        );
    }

    #[test]
    fn a_dest_that_is_neither_absolute_nor_under_tilde_is_refused() {
        let scratch = Scratch::new("bad-dest");
        let mut state = State::new();
        let mut vague = entry("browser-harness");
        vague.dest = Some("skills/".to_string());
        let publisher = Fake::printing(&published("browser-harness"));

        let outcome = install(&vague, &scratch.home(), &mut state, &publisher, "t");

        assert!(
            matches!(outcome.refusal(), Some(Refused::BadDest { .. })),
            "{outcome}"
        );
        assert!(publisher.calls().is_empty());
        // And `~user` names somebody's home this process cannot know.
        assert_eq!(resolve_dest("~somebody/skills", Path::new("/home/x")), None);
    }

    #[test]
    fn a_dest_that_cannot_be_written_is_a_refusal_rather_than_a_state_record() {
        // A file where the skills directory should be: creating the directory fails, so the
        // install must not claim to have happened.
        let scratch = Scratch::new("unwritable");
        std::fs::create_dir_all(scratch.home().join(".deepnest")).unwrap();
        std::fs::write(
            scratch.home().join(".deepnest").join("skills"),
            "not a directory",
        )
        .unwrap();

        let mut state = State::new();
        let outcome = install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing(&published("browser-harness")),
            "t",
        );

        assert!(
            matches!(outcome.refusal(), Some(Refused::CannotWrite { .. })),
            "{outcome}"
        );
        assert!(state.is_empty(), "a record that is not on disk is a lie");
    }

    #[test]
    fn the_state_file_written_afterwards_reflects_the_install() {
        // The crate's contract is that `install` updates the state and the caller persists it; this
        // is the round trip a caller would do, so the two halves are known to agree.
        let scratch = Scratch::new("persist");
        let mut state = State::new();
        install(
            &entry("browser-harness"),
            &scratch.home(),
            &mut state,
            &Fake::printing(&published("browser-harness")),
            "2026-10-31T12:00:00Z",
        );
        state.write(&state::path(&scratch.dest())).unwrap();

        let read = State::read(&state::path(&scratch.dest()));
        assert!(read.may_refresh("browser-harness"));
    }
}
