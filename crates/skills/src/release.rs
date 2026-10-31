//! `check: github_release`: is there a newer release than the one on disk?
//!
//! The installer answers "put this skill here". This module answers a different question, asked
//! later and repeatedly, with a different mechanism: the package publishes releases, and the forge
//! knows what the newest one is. So the check is one request and one comparison.
//!
//! # Where the line is drawn
//!
//! **The request is a seam, and this crate has no transport.** [`Releases`] is the seam, and what it
//! returns is the answer's *body* — not a parsed release, not a version. That is the same shape
//! [`crate::installer::Publisher`] has, and for the same reason: every way a body can be wrong is
//! decided here, in code a test can drive with a string, and whatever eventually speaks HTTP stays
//! dumb enough to be replaceable. What is missing is only the socket: GitHub answers over TLS, and
//! while `crates/gateway::transport` speaks it, this crate has no dependency on it and nothing here
//! is wired to that adapter.
//! [crate::release::release_url] therefore states the URL as part of the contract, tested, while the
//! call that would fetch it waits.
//!
//! # Four answers, four next steps
//!
//! [`Verdict`] is not a boolean because "no" and "I could not tell" are not the same answer, and
//! treating the second as the first is how an update check silently stops offering updates:
//!
//! - [`Verdict::UpToDate`] — nothing to do.
//! - [`Verdict::Newer`] — refresh, and record the version that refresh adopted.
//! - [`Verdict::Unknown`] — the newest release is known but nothing is recorded about what is
//!   installed. A refresh adopts the newest and records it, which is the only way an unversioned
//!   skill ever becomes comparable. Today that is every skill the installer put here.
//! - [`Verdict::CannotTell`] — the question was not answered, and [`Why`] says which part was
//!   missing. Never read this as "up to date": it is the branch that keeps a broken check from
//!   looking like a healthy one.
//!
//! # What counts as a version, and what is refused rather than guessed
//!
//! A tag is compared only when it is **dotted numbers**, optionally `v`-prefixed: `1`, `v0.1.8`,
//! `1.0.0`. Three shapes are refused, each because guessing would produce a wrong answer that looks
//! like a right one:
//!
//! - **Date tags** (`2026-10-31`) and any other non-numeric form. Compared as numbers they would
//!   read as `2026 > 1`, so a repository that tags by date would offer an "update" on every check
//!   forever.
//! - **Pre-releases** (`1.0.0-beta.2`). Ordering them properly is a real rule set, and the failure
//!   of getting it wrong is an update to a beta nobody asked for. Refusing offers no update, which
//!   is the safe direction, and installing by hand still works.
//! - **A recorded version the same rules reject**, which is a file somebody edited. The state file
//!   is a file users open, so this is a reading, not a corruption.
//!
//! Comparison itself is numeric per component, padding the shorter side with zeros, so `0.9` is
//! older than `0.10` (a string comparison would say the opposite) and `1.0` equals `1.0.0`.

use std::cmp::Ordering;
use std::fmt;
use std::io;

use crate::catalog::Entry;
use crate::state::Installed;

/// The `check:` value this module understands.
pub const GITHUB_RELEASE: &str = "github_release";

/// The endpoint a `github_release` check asks, spelled once.
///
/// Part of the contract rather than a detail of some transport: `/releases/latest` on the GitHub API
/// is the question, and a test asserts the shape so a later transport does not invent a second one.
pub fn release_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases/latest")
}

/// The seam between deciding to check and reaching the network.
///
/// A trait for the same reason [`crate::installer::Publisher`] is one: every decision worth testing
/// here is about what a *body* means, and a test that fetched a real one would be measuring GitHub —
/// and would then need the network, a token, and whatever the repository happened to release today.
///
/// The return value is the answer's body as the forge sent it. Interpreting it is this module's
/// work, not the transport's.
pub trait Releases: Send + Sync {
    /// The body of the latest release's answer for `repo`, spelled `owner/name`.
    fn latest(&self, repo: &str) -> io::Result<String>;
}

/// A version this module is willing to compare.
///
/// Constructed by [`Version::parse`] and by nothing else, so holding one means it already passed the
/// rules in the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    parts: Vec<u64>,
}

impl Version {
    /// Read a tag or a recorded version, or `None` when it is not dotted numbers.
    ///
    /// A single leading `v` or `V` is dropped, because a release is tagged `v1.2.3` while the version
    /// inside it is `1.2.3` and the two are the same version. Nothing else is stripped: `release-1.2`
    /// and `1.2.3-beta` are refusals, not versions with a decoration.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text
            .strip_prefix('v')
            .or_else(|| text.strip_prefix('V'))
            .unwrap_or(text);
        if text.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        for part in text.split('.') {
            // `is_ascii_digit` rather than a parse attempt: `+1` and ` 1` would both parse as numbers
            // and neither is a version component.
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            parts.push(part.parse::<u64>().ok()?);
        }
        Some(Self { parts })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self
            .parts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<String>>()
            .join(".");
        f.write_str(&text)
    }
}

/// Order two versions numerically, component by component, the shorter padded with zeros.
///
/// Numeric is the whole point: `0.9` before `0.10`, which a string comparison gets backwards.
fn order(left: &Version, right: &Version) -> Ordering {
    let width = left.parts.len().max(right.parts.len());
    for index in 0..width {
        let this = left.parts.get(index).copied().unwrap_or(0);
        let that = right.parts.get(index).copied().unwrap_or(0);
        match this.cmp(&that) {
            Ordering::Equal => continue,
            decided => return decided,
        }
    }
    Ordering::Equal
}

/// What the check found, and therefore what the caller does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The newest release is the version that is installed. Nothing to do.
    UpToDate {
        /// The version both sides agree on.
        version: Version,
    },
    /// The package has released something newer than what is on disk.
    Newer {
        /// The newest release, which a refresh adopts and records.
        version: Version,
    },
    /// The newest release is known; what is installed is not recorded, so the two cannot be compared.
    Unknown {
        /// The newest release, which a refresh adopts and records — the only way this skill becomes
        /// comparable at all.
        latest: Version,
    },
    /// The question was not answered. Never a synonym for [`Verdict::UpToDate`].
    CannotTell(Why),
}

impl Verdict {
    /// The version a refresh would adopt, for the two verdicts where refreshing is the next step.
    ///
    /// `None` for [`Verdict::UpToDate`] and for [`Verdict::CannotTell`], which is what makes it safe
    /// for a caller to treat "there is something to adopt" as one decision.
    pub fn adoptable(&self) -> Option<&Version> {
        match self {
            Self::Newer { version } | Self::Unknown { latest: version } => Some(version),
            Self::UpToDate { .. } | Self::CannotTell(_) => None,
        }
    }
}

/// Which part of the question went unanswered.
///
/// One variant per missing piece, because each has a different next step: a catalog to fix, a state
/// file to look at, a network to restore, or a repository that changed how it tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// The entry names no `check:`, so nothing ever asked about a newer version.
    NotChecked,
    /// The entry names a check this build does not know, so it was not performed.
    UnknownCheck {
        /// The value the catalog carried.
        kind: String,
    },
    /// The entry names no repository, so there is no release to look at.
    NoRepo,
    /// The skill is not one deepnest installed, so no update may touch it.
    NotManaged {
        /// The skill's name, so the report can name what it refused.
        name: String,
    },
    /// The request was made and did not arrive.
    Unreachable {
        /// What the transport said.
        detail: String,
    },
    /// The answer arrived and is not a release — a proxy's page, an API error, a truncated body.
    NotARelease {
        /// Why the body could not be read as a release, including the forge's own message when it
        /// sent one.
        detail: String,
    },
    /// The tag the repository publishes is not a version this build orders: a date, a name, a
    /// pre-release.
    TagNotAVersion {
        /// The tag as published.
        tag: String,
    },
    /// The version recorded for the installed skill is not one this build orders, which means the
    /// state file was edited by hand or by another tool.
    InstalledNotAVersion {
        /// The version as recorded.
        version: String,
    },
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotChecked => write!(
                f,
                "the catalog entry names no update check, so no newer release was looked for"
            ),
            Self::UnknownCheck { kind } => write!(
                f,
                "the catalog entry asks for `check: {kind}`, which this build does not know"
            ),
            Self::NoRepo => write!(
                f,
                "the catalog entry names no repository, so there are no releases to compare against"
            ),
            Self::NotManaged { name } => write!(
                f,
                "`{name}` was not installed by deepnest, so it is the user's and is never updated"
            ),
            Self::Unreachable { detail } => {
                write!(f, "the release could not be fetched: {detail}")
            }
            Self::NotARelease { detail } => {
                write!(f, "the answer is not a release: {detail}")
            }
            Self::TagNotAVersion { tag } => write!(
                f,
                "the newest tag `{tag}` is not a version this build can order, so it was not \
                 compared — install by hand if it is newer"
            ),
            Self::InstalledNotAVersion { version } => write!(
                f,
                "the version recorded for this skill, `{version}`, is not one this build can order"
            ),
        }
    }
}

/// Ask whether `entry`'s package has released something newer than what is installed.
///
/// `installed` is what the state file knows, or `None` when it knows nothing about this name. The
/// check refuses a skill that is not `managed` **before** it fetches anything: a user's own file is
/// theirs, and there is no reason to ask the network about a file no update may touch.
pub fn check(entry: &Entry, installed: Option<&Installed>, releases: &dyn Releases) -> Verdict {
    match entry.check.as_deref() {
        None | Some("") => return Verdict::CannotTell(Why::NotChecked),
        Some(GITHUB_RELEASE) => {}
        Some(kind) => {
            return Verdict::CannotTell(Why::UnknownCheck {
                kind: kind.to_string(),
            })
        }
    }

    if let Some(installed) = installed {
        if !installed.managed {
            return Verdict::CannotTell(Why::NotManaged {
                name: entry.name.clone(),
            });
        }
    }

    let Some(repo) = entry.repo.as_deref() else {
        return Verdict::CannotTell(Why::NoRepo);
    };

    let body = match releases.latest(repo) {
        Ok(body) => body,
        Err(error) => {
            return Verdict::CannotTell(Why::Unreachable {
                detail: error.to_string(),
            })
        }
    };

    let tag = match tag_of(&body) {
        Ok(tag) => tag,
        Err(detail) => return Verdict::CannotTell(Why::NotARelease { detail }),
    };

    let Some(latest) = Version::parse(&tag) else {
        return Verdict::CannotTell(Why::TagNotAVersion { tag });
    };

    let Some(recorded) = installed.and_then(|i| i.version.as_deref()) else {
        // Nothing is recorded about what is on disk, so the two cannot be compared. The newest
        // release is still a fact worth reporting: it is what a refresh would adopt, and recording
        // it is what makes the next check answerable.
        return Verdict::Unknown { latest };
    };

    let Some(current) = Version::parse(recorded) else {
        return Verdict::CannotTell(Why::InstalledNotAVersion {
            version: recorded.to_string(),
        });
    };

    if order(&latest, &current) == Ordering::Greater {
        Verdict::Newer { version: latest }
    } else {
        // Equal or older. A release older than what is installed is not an update — the state file
        // can be ahead of the latest release when somebody installed from a branch, and stepping
        // back would be a downgrade nobody asked for.
        Verdict::UpToDate { version: current }
    }
}

/// The `tag_name` in a release body, or why the body is not one.
///
/// Read by hand off a `serde_json::Value` rather than into a struct, because the only field this
/// build needs is the tag, and a struct would silently start refusing bodies that carry less than a
/// full release — a test fixture, a mirror, an older API.
fn tag_of(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("the body is not JSON ({error})"))?;

    if let Some(tag) = value.get("tag_name").and_then(serde_json::Value::as_str) {
        let tag = tag.trim();
        if !tag.is_empty() {
            return Ok(tag.to_string());
        }
    }

    // GitHub puts its own reason in `message` — `Not Found`, `API rate limit exceeded`. Passing it
    // through is the difference between "the answer is not a release" and knowing which one it is.
    Err(
        match value.get("message").and_then(serde_json::Value::as_str) {
            Some(message) => format!("it carries no `tag_name`, and says: {message}"),
            None => "it carries no `tag_name`".to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering as Atomic};
    use std::sync::Mutex;

    /// A seam that answers with what the test told it, and counts how often it was asked.
    struct Scripted {
        answer: Result<String, String>,
        calls: AtomicUsize,
        asked: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn answering(body: &str) -> Self {
            Self {
                answer: Ok(body.to_string()),
                calls: AtomicUsize::new(0),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn failing(detail: &str) -> Self {
            Self {
                answer: Err(detail.to_string()),
                calls: AtomicUsize::new(0),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Atomic::SeqCst)
        }
    }

    impl Releases for Scripted {
        fn latest(&self, repo: &str) -> io::Result<String> {
            self.calls.fetch_add(1, Atomic::SeqCst);
            self.asked.lock().unwrap().push(repo.to_string());
            self.answer.clone().map_err(io::Error::other)
        }
    }

    fn entry() -> Entry {
        Entry {
            name: "browser-harness".into(),
            description: "drives a browser".into(),
            repo: Some("browser-use/browser-harness".into()),
            install: Some(crate::installer::SELF_PUBLISHING.into()),
            dest: Some("~/.deepnest/skills/".into()),
            check: Some(GITHUB_RELEASE.into()),
        }
    }

    fn managed(version: Option<&str>) -> Installed {
        Installed::managed_at(version.map(str::to_string), "2026-10-31T09:00:00Z")
    }

    fn release(tag: &str) -> String {
        format!(r#"{{"tag_name": "{tag}", "name": "{tag}", "draft": false}}"#)
    }

    #[test]
    fn a_released_tag_is_compared_as_a_whole_version() {
        // `0.10` is newer than `0.9`. Compared as text it is not, which is the bug this test exists
        // to keep out: an update check that goes quiet at the tenth release.
        let seam = Scripted::answering(&release("v0.10.0"));
        let verdict = check(&entry(), Some(&managed(Some("0.9.0"))), &seam);
        assert_eq!(
            verdict,
            Verdict::Newer {
                version: Version::parse("0.10.0").unwrap()
            }
        );
        assert_eq!(seam.calls(), 1);
    }

    #[test]
    fn the_same_version_is_up_to_date_however_the_leading_v_is_written() {
        let seam = Scripted::answering(&release("v1.2.3"));
        let verdict = check(&entry(), Some(&managed(Some("1.2.3"))), &seam);
        assert_eq!(
            verdict,
            Verdict::UpToDate {
                version: Version::parse("1.2.3").unwrap()
            }
        );
    }

    #[test]
    fn a_patch_release_on_a_two_part_version_is_still_an_update() {
        // The padding has to work in the direction that matters: `1.0` is older than `1.0.1`, so a
        // missing third component means zero and not "equal to whatever the tag says".
        let seam = Scripted::answering(&release("v1.0.1"));
        let verdict = check(&entry(), Some(&managed(Some("1.0"))), &seam);
        assert_eq!(
            verdict,
            Verdict::Newer {
                version: Version::parse("1.0.1").unwrap()
            }
        );
    }

    #[test]
    fn a_tag_of_nothing_but_space_is_not_a_release() {
        // Trimming happens where the tag is read, so a tag that is only whitespace is a missing
        // tag — not a "version" that fails to parse two steps later.
        let seam = Scripted::answering(&release("   "));
        let verdict = check(&entry(), Some(&managed(Some("1.0.0"))), &seam);
        assert!(
            matches!(verdict, Verdict::CannotTell(Why::NotARelease { .. })),
            "{verdict:?}"
        );
    }

    #[test]
    fn a_shorter_version_is_padded_rather_than_treated_as_different() {
        // `1.0` and `1.0.0` are the same version, so there is nothing to offer.
        let seam = Scripted::answering(&release("v1.0.0"));
        let verdict = check(&entry(), Some(&managed(Some("1.0"))), &seam);
        assert!(matches!(verdict, Verdict::UpToDate { .. }), "{verdict:?}");
    }

    #[test]
    fn an_older_release_is_not_an_update() {
        // The state file can be ahead of the latest release — somebody installed from a branch.
        // Offering the release back would be a downgrade nobody asked for.
        let seam = Scripted::answering(&release("v1.0.0"));
        let verdict = check(&entry(), Some(&managed(Some("1.4.0"))), &seam);
        // And it reports what is installed, not the older release: calling the installed version
        // 1.0.0 because that is what the tag says would be a lie the next check acts on.
        assert_eq!(
            verdict,
            Verdict::UpToDate {
                version: Version::parse("1.4.0").unwrap()
            }
        );
    }

    #[test]
    fn a_date_shaped_tag_is_refused_rather_than_read_as_a_huge_version() {
        // `2026-10-31` compared as numbers is `2026 > 1`, so a repository tagging by date would
        // offer an update on every check for ever. Refusing offers none, which is the safe answer.
        let seam = Scripted::answering(&release("2026-10-31"));
        let verdict = check(&entry(), Some(&managed(Some("1.0.0"))), &seam);
        assert_eq!(
            verdict,
            Verdict::CannotTell(Why::TagNotAVersion {
                tag: "2026-10-31".into()
            })
        );
        assert_eq!(verdict.adoptable(), None);
    }

    #[test]
    fn a_pre_release_tag_is_refused() {
        // Ordering pre-releases is a rule set of its own, and getting it wrong updates somebody to
        // a beta they never asked for. No update is the safe direction.
        let seam = Scripted::answering(&release("v2.0.0-beta.1"));
        let verdict = check(&entry(), Some(&managed(Some("1.0.0"))), &seam);
        assert!(matches!(
            verdict,
            Verdict::CannotTell(Why::TagNotAVersion { .. })
        ));
    }

    #[test]
    fn a_skill_with_no_recorded_version_gets_the_newest_release_to_adopt() {
        // Every skill the installer puts here today: it records provenance, not a version. The
        // check cannot compare, and saying so while naming the newest release is what lets a
        // refresh adopt it and make the next check answerable.
        let seam = Scripted::answering(&release("v0.1.8"));
        let verdict = check(&entry(), Some(&managed(None)), &seam);
        assert_eq!(
            verdict,
            Verdict::Unknown {
                latest: Version::parse("0.1.8").unwrap()
            }
        );
        assert_eq!(
            verdict.adoptable().map(ToString::to_string),
            Some("0.1.8".to_string())
        );
    }

    #[test]
    fn a_skill_the_state_file_does_not_know_is_treated_as_unversioned() {
        let seam = Scripted::answering(&release("v0.1.8"));
        let verdict = check(&entry(), None, &seam);
        assert!(matches!(verdict, Verdict::Unknown { .. }), "{verdict:?}");
    }

    #[test]
    fn a_user_brought_skill_is_refused_without_asking_the_network() {
        // The rule the whole state file exists for: a file deepnest did not write is the user's.
        // Refusing before the fetch is part of the answer — there is nothing to ask about a file no
        // update may touch.
        let seam = Scripted::answering(&release("v9.9.9"));
        let verdict = check(&entry(), Some(&Installed::unmanaged()), &seam);
        assert_eq!(
            verdict,
            Verdict::CannotTell(Why::NotManaged {
                name: "browser-harness".into()
            })
        );
        assert_eq!(seam.calls(), 0, "the network was asked about a user's file");
    }

    #[test]
    fn an_entry_without_a_check_is_not_a_failure_to_fetch() {
        let mut no_check = entry();
        no_check.check = None;
        let seam = Scripted::answering(&release("v9.9.9"));
        assert_eq!(
            check(&no_check, Some(&managed(Some("1.0.0"))), &seam),
            Verdict::CannotTell(Why::NotChecked)
        );
        assert_eq!(seam.calls(), 0);
    }

    #[test]
    fn an_empty_check_is_the_same_as_none() {
        // A catalog written by hand can leave the key with no value. That is "no check", not a
        // check named "".
        let mut empty = entry();
        empty.check = Some(String::new());
        let seam = Scripted::answering(&release("v9.9.9"));
        assert_eq!(
            check(&empty, Some(&managed(Some("1.0.0"))), &seam),
            Verdict::CannotTell(Why::NotChecked)
        );
        assert_eq!(seam.calls(), 0);
    }

    #[test]
    fn a_tag_padded_by_whatever_wrote_the_body_still_reads() {
        // A mirror, a fixture, or a hand-edited answer. The tag is a value, not a byte sequence.
        let seam = Scripted::answering(&release("  v0.1.8  "));
        let verdict = check(&entry(), Some(&managed(Some("0.1.7"))), &seam);
        assert_eq!(
            verdict,
            Verdict::Newer {
                version: Version::parse("0.1.8").unwrap()
            }
        );
    }

    #[test]
    fn a_check_this_build_does_not_know_is_named_rather_than_attempted() {
        let mut other = entry();
        other.check = Some("git_tag".into());
        let seam = Scripted::answering(&release("v9.9.9"));
        assert_eq!(
            check(&other, Some(&managed(Some("1.0.0"))), &seam),
            Verdict::CannotTell(Why::UnknownCheck {
                kind: "git_tag".into()
            })
        );
        assert_eq!(seam.calls(), 0);
    }

    #[test]
    fn an_entry_without_a_repository_has_nothing_to_check() {
        let mut no_repo = entry();
        no_repo.repo = None;
        let seam = Scripted::answering(&release("v9.9.9"));
        assert_eq!(
            check(&no_repo, Some(&managed(Some("1.0.0"))), &seam),
            Verdict::CannotTell(Why::NoRepo)
        );
    }

    #[test]
    fn a_request_that_does_not_arrive_is_reported_with_what_the_transport_said() {
        // Nothing is wired to a socket yet, so this is the branch a real run lands in, and it has
        // to read as "not checked" and never as "up to date".
        let seam = Scripted::failing("no socket is wired to a skill release check in this build");
        let verdict = check(&entry(), Some(&managed(Some("1.0.0"))), &seam);
        match &verdict {
            Verdict::CannotTell(Why::Unreachable { detail }) => {
                assert!(detail.contains("no socket is wired"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(verdict.adoptable(), None);
    }

    #[test]
    fn an_api_error_body_is_reported_with_the_forge_own_message() {
        // GitHub answers a rate limit or a missing repository with a `message`, and passing it
        // through is what turns "not a release" into something a person can act on.
        let seam = Scripted::answering(r#"{"message": "API rate limit exceeded"}"#);
        let verdict = check(&entry(), Some(&managed(Some("1.0.0"))), &seam);
        match &verdict {
            Verdict::CannotTell(Why::NotARelease { detail }) => {
                assert!(detail.contains("rate limit"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_body_that_is_not_json_is_not_a_release() {
        let seam = Scripted::answering("<html>proxy says no</html>");
        let verdict = check(&entry(), Some(&managed(Some("1.0.0"))), &seam);
        assert!(matches!(
            verdict,
            Verdict::CannotTell(Why::NotARelease { .. })
        ));
    }

    #[test]
    fn a_recorded_version_this_build_cannot_order_is_reported_against_the_state_file() {
        // A hand-edited `.state.json`. Distinct from a bad tag: this one is fixed by looking at the
        // file, that one is the repository's business.
        let seam = Scripted::answering(&release("v1.0.0"));
        let verdict = check(&entry(), Some(&managed(Some("unknown"))), &seam);
        assert_eq!(
            verdict,
            Verdict::CannotTell(Why::InstalledNotAVersion {
                version: "unknown".into()
            })
        );
    }

    #[test]
    fn the_repository_is_asked_by_the_name_the_catalog_gave_it() {
        let seam = Scripted::answering(&release("v1.0.0"));
        let _ = check(&entry(), Some(&managed(Some("1.0.0"))), &seam);
        assert_eq!(
            seam.asked.lock().unwrap().as_slice(),
            ["browser-use/browser-harness"]
        );
    }

    #[test]
    fn the_endpoint_is_the_one_part_of_the_contract_a_transport_must_keep() {
        assert_eq!(
            release_url("browser-use/browser-harness"),
            "https://api.github.com/repos/browser-use/browser-harness/releases/latest"
        );
    }

    #[test]
    fn a_version_is_only_dotted_numbers_behind_an_optional_v() {
        // Surrounding whitespace is trimmed — a header written by hand may carry it — but nothing
        // inside the version is.
        for good in ["1", "v1", "1.2", "V1.2.3", " 1.2.3 ", "0.0.1"] {
            assert!(Version::parse(good).is_some(), "{good} should parse");
        }
        for bad in [
            "",
            "v",
            "1.",
            ".1",
            "1..2",
            "1.2.3-rc1",
            "1.2.3+build",
            "release-1.2",
            "vv1.2",
            "1.2.x",
            "-1.2",
            "+1.2",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad} should not parse");
        }
    }

    #[test]
    fn a_version_prints_back_the_way_it_was_read() {
        let version = Version::parse("v0.1.8").unwrap();
        assert_eq!(version.to_string(), "0.1.8");
    }
}
