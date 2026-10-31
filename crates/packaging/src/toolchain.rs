//! The unpacked toolchain on disk: where it goes, and what makes it whole.
//!
//! The layout is `~/.deepnest/toolchain/<version>/`, with one file inside it named [`MARKER`] that
//! names the payload the directory was unpacked from. Nothing else in the tree is special, which is
//! the point: the toolchain is whatever the payload said it was, and this crate never looks inside
//! it.
//!
//! Four decisions, in the order they cost something:
//!
//! - **The staging directory is a sibling of the one it becomes, inside `toolchain/`.** Unpacking
//!   into the system temp directory is the obvious thing and the wrong one: publishing the result
//!   would then be a rename **across a filesystem boundary** whenever the user's home and temp live
//!   on different volumes — and a rename across volumes is not a rename, it is a copy followed by a
//!   delete, whose interruption leaves exactly the half-unpacked directory this crate exists to
//!   prevent. Keeping the staging directory beside its destination makes the rename a rename.
//! - **A directory that exists is not evidence of a finished unpack; the marker is.** [`adopt`]
//!   writes the marker inside the staging directory *before* the rename, so the published name
//!   appears only once its contents are complete, and a directory whose marker is missing is read
//!   as [`Status::Interrupted`] rather than trusted. `crates/apps` makes the same move with
//!   `deepnest.toml`: a directory is what its marker says it is.
//! - **The marker names the payload, not the version.** A version directory can outlive the build
//!   that filled it — two development builds both call themselves `0.1.0`, and a user may restore a
//!   backup over an upgrade — so a directory that carries a *different* payload is
//!   [`Status::Foreign`] and is replaced, where a bare version comparison would have called it
//!   ready and left the user running last week's compiler.
//! - **An interrupted unpack is swept at startup, not repaired.** Half of one payload is not
//!   something to resume; the only truthful repairs are "delete it and unpack again" and "leave it
//!   and report it", and a directory nobody can use is not worth keeping. [`sweep`] is called by the
//!   process that owns the port, because the port is the single-instance admission — so no other
//!   deepnest can be filling a staging directory at that moment.
//!
//! The unpack itself fills a [`Staging`] and calls [`Staging::adopt`]; nothing else may write inside
//! `toolchain/`. A [`Staging`] that is dropped without being adopted removes what it wrote, so a
//! failed unpack leaves nothing behind even when the caller forgets to.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The directory under the deepnest home that holds the unpacked toolchain.
pub const DIR_NAME: &str = "toolchain";

/// The file whose presence makes a toolchain directory usable, and whose contents name the payload
/// it was unpacked from.
pub const MARKER: &str = "payload";

/// The suffix a directory carries while it is being filled.
///
/// A version may not end with it ([`check_version`]), so no staging name can collide with a
/// published one.
const STAGING_SUFFIX: &str = ".partial";

/// How long a version string may be. Generous for a semantic version, and short enough that the
/// path stays inside what every platform can name.
pub const MAX_VERSION_CHARS: usize = 64;

/// The version of the toolchain payload this build carries — the directory it unpacks into.
///
/// It is this crate's own version today, because the payload is not built yet. When it is, this
/// becomes the payload archive's name, and the two stop moving together.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The payload id this build expects the marker to name.
///
/// Equal to [`VERSION`] today for the same reason. The two exist separately because the marker
/// answers a different question from the directory name: not "which version is this" but "was this
/// unpacked from the payload I carry", and those diverge the moment two builds share a version.
pub const PAYLOAD: &str = env!("CARGO_PKG_VERSION");

/// `~/.deepnest/toolchain` — where every unpacked payload lives.
pub fn dir(home: &Path) -> PathBuf {
    home.join(DIR_NAME)
}

/// `~/.deepnest/toolchain/<version>` — one unpacked payload.
pub fn version_dir(home: &Path, version: &str) -> PathBuf {
    dir(home).join(version)
}

/// `~/.deepnest/toolchain/<version>.partial` — the same payload, still being written.
pub fn staging_dir(home: &Path, version: &str) -> PathBuf {
    dir(home).join(format!("{version}{STAGING_SUFFIX}"))
}

/// What `~/.deepnest/toolchain/<version>/` is, as four readings rather than a yes/no.
///
/// The readings are separate because the actions are: nothing there means unpack, an interrupted
/// one means the same, and one unpacked from another payload means replace before unpacking. A
/// single `bool` would leave the caller to rediscover which of those it is holding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The payload is unpacked and this build may use it.
    Ready {
        /// The directory holding the unpacked payload.
        dir: PathBuf,
    },
    /// Nothing has been unpacked for this version.
    Absent,
    /// A directory is there and its marker is not: an unpack was interrupted, or the marker was
    /// deleted. Either way the tree cannot be vouched for, so it is never used.
    Interrupted {
        /// The directory a previous unpack left behind.
        dir: PathBuf,
    },
    /// The directory was unpacked from a different payload than the one this build carries.
    Foreign {
        /// The directory holding the wrong payload.
        dir: PathBuf,
        /// The payload id its marker named.
        found: String,
    },
}

impl Status {
    /// Whether the toolchain may be used as it stands.
    pub fn ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

impl fmt::Display for Status {
    /// The reading as a sentence, for the startup line and for anything else that reports it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready { dir } => write!(f, "unpacked at {}", dir.display()),
            Self::Absent => write!(f, "not unpacked yet — it unpacks on the first build"),
            Self::Interrupted { dir } => write!(
                f,
                "a previous unpack left {} unfinished; it will be discarded and redone",
                dir.display()
            ),
            Self::Foreign { dir, found } => write!(
                f,
                "{} holds payload {found}, which this build did not carry; it will be replaced",
                dir.display()
            ),
        }
    }
}

/// What `version` is on disk right now.
///
/// A directory that cannot be read is [`Status::Interrupted`], not an error: "I cannot vouch for
/// this tree" and "there is no marker in it" are the same answer for the caller, and refusing to
/// report a toolchain is better than reporting one that will fail at the first build.
pub fn status(home: &Path, version: &str, payload: &str) -> io::Result<Status> {
    check_version(version)?;
    let dir = version_dir(home, version);
    if !exists(&dir)? {
        return Ok(Status::Absent);
    }
    Ok(match marker(&dir)? {
        None => Status::Interrupted { dir },
        Some(found) if found == payload.trim() => Status::Ready { dir },
        Some(found) => Status::Foreign { dir, found },
    })
}

/// A directory reserved for writing one payload into.
///
/// The unpack fills [`Staging::dir`] and calls [`Staging::adopt`]. Nothing outside this crate
/// should write inside `toolchain/`, and nothing at all should write into a version directory: that
/// name means "complete" to every reader in this file.
#[derive(Debug)]
pub struct Staging {
    dir: PathBuf,
    target: PathBuf,
}

impl Staging {
    /// The directory to unpack into.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Publish what was unpacked: write the marker, then rename the staging directory into place.
    ///
    /// The marker goes in first, so a reader that arrives between the two calls sees a staging
    /// directory — which nothing reads — rather than a published name with half a payload under it.
    ///
    /// A directory already holding **this** payload is refused rather than replaced: the caller
    /// asked to unpack something that is already there, and silently replacing a working toolchain
    /// is not a repair. Any other directory in the way is used up — interrupted, or from another
    /// payload — and is removed, because leaving it would block the rename on every platform.
    pub fn adopt(self, payload: &str) -> io::Result<PathBuf> {
        let payload = payload.trim();
        if payload.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a payload id cannot be empty: the marker is how a later run knows what it is trusting",
            ));
        }
        fs::write(self.dir.join(MARKER), format!("{payload}\n"))?;

        if exists(&self.target)? {
            match marker(&self.target)? {
                Some(found) if found == payload => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} already holds payload {found}", self.target.display()),
                    ));
                }
                _ => remove_any(&self.target)?,
            }
        }

        fs::rename(&self.dir, &self.target)?;
        Ok(self.target.clone())
    }

    /// Give the reservation back without publishing anything.
    pub fn discard(self) -> io::Result<()> {
        remove_any(&self.dir)
    }
}

impl Drop for Staging {
    /// A staging directory outliving its owner is exactly the leftover [`sweep`] exists to remove,
    /// so it is removed here instead, on the way out of a failed unpack. After a successful
    /// [`Staging::adopt`] the path no longer exists, and this is a no-op.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Reserve a staging directory for `version`, removing anything an earlier attempt left.
///
/// Reusing a leftover is refused, not because resuming is impossible, but because half of one
/// payload mixed with half of another is not a toolchain and cannot be recognised as one afterwards.
pub fn staging(home: &Path, version: &str) -> io::Result<Staging> {
    check_version(version)?;
    fs::create_dir_all(dir(home))?;
    let staged = staging_dir(home, version);
    remove_any(&staged)?;
    fs::create_dir_all(&staged)?;
    Ok(Staging {
        dir: staged,
        target: version_dir(home, version),
    })
}

/// Remove every staging directory under `toolchain/`, and say which ones were removed.
///
/// Called once per run by the process that owns the port. An empty list is the ordinary case; a
/// list with something in it is worth printing, because it is the only trace of a first run that
/// was killed while unpacking, and a user who sees "deepnest is unpacking its toolchain" twice
/// deserves to know why.
pub fn sweep(home: &Path) -> io::Result<Vec<PathBuf>> {
    let root = dir(home);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        // No `toolchain/` at all is not a fault: an install that has never built anything has no
        // reason to hold one, exactly as `apps/` is absent until an app is created.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut removed = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry
            .file_name()
            .to_string_lossy()
            .ends_with(STAGING_SUFFIX)
        {
            continue;
        }
        let path = entry.path();
        remove_any(&path)?;
        removed.push(path);
    }
    // Deterministic order, so a report reads the same way twice.
    removed.sort();
    Ok(removed)
}

/// Whether `version` can be a directory name on every platform deepnest ships on.
///
/// The version is a compile-time constant today, and it is still checked: it becomes a path
/// segment, and the moment it is read from a payload header instead of a constant nothing else in
/// this file would stop `../..` from being called a version. The alphabet is the union of what NTFS,
/// APFS and ext4 will name without escaping anything.
fn check_version(version: &str) -> io::Result<()> {
    let usable = !version.is_empty()
        && version.chars().count() <= MAX_VERSION_CHARS
        && version != "."
        && version != ".."
        && !version.ends_with(STAGING_SUFFIX)
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
    if usable {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("`{version}` cannot be a toolchain directory name"),
    ))
}

/// Whether anything at all is at `path`, without following a symlink to find out.
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The payload id a directory's marker names, or `None` when it has none worth trusting.
fn marker(dir: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(dir.join(MARKER)) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Remove whatever is at `path`: a directory, a file, or nothing at all.
///
/// The type test is `symlink_metadata`, so a symlink is removed rather than followed — following one
/// would delete what it points at, and a `toolchain/` entry someone symlinked is their business
/// until deepnest has to replace it.
fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-packaging-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// A home with a toolchain directory, as a first run would leave it.
    fn scratch(name: &str) -> (Scratch, PathBuf) {
        let scratch = Scratch::new(name);
        let home = scratch.base.join("home");
        fs::create_dir_all(&home).unwrap();
        (scratch, home)
    }

    /// Fill a staging directory the way the unpack will, and publish it.
    fn unpack(home: &Path, version: &str, payload: &str) -> PathBuf {
        let staging = staging(home, version).unwrap();
        fs::create_dir_all(staging.dir().join("bin")).unwrap();
        fs::write(
            staging.dir().join("bin").join("rustc"),
            "not a real compiler",
        )
        .unwrap();
        staging.adopt(payload).unwrap()
    }

    #[test]
    fn a_toolchain_that_was_never_unpacked_is_absent_rather_than_a_fault() {
        let (_scratch, home) = scratch("absent");
        assert_eq!(status(&home, "0.1.0", "0.1.0").unwrap(), Status::Absent);
        // And nothing was created by asking.
        assert!(!dir(&home).exists());
    }

    #[test]
    fn adopt_publishes_the_directory_and_the_marker_names_the_payload() {
        let (_scratch, home) = scratch("ready");
        // The staging directory has to exist before it can be filled.
        fs::create_dir_all(dir(&home)).unwrap();
        let staging = staging(&home, "0.1.0").unwrap();
        fs::create_dir_all(staging.dir().join("bin")).unwrap();
        fs::write(staging.dir().join("bin").join("rustc"), "compiler").unwrap();
        let published = staging.adopt("payload-7").unwrap();

        assert_eq!(published, version_dir(&home, "0.1.0"));
        assert_eq!(
            fs::read_to_string(published.join(MARKER)).unwrap().trim(),
            "payload-7"
        );
        assert_eq!(
            status(&home, "0.1.0", "payload-7").unwrap(),
            Status::Ready { dir: published }
        );
        // The staging name is gone: nothing is left under the tree that looks like a second copy.
        assert!(!staging_dir(&home, "0.1.0").exists());
    }

    #[test]
    fn a_directory_without_a_marker_is_interrupted_rather_than_ready() {
        let (_scratch, home) = scratch("interrupted");
        let dir = version_dir(&home, "0.1.0");
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin").join("rustc"), "half a compiler").unwrap();

        let status = status(&home, "0.1.0", "payload-7").unwrap();
        assert_eq!(status, Status::Interrupted { dir });
        assert!(!status.ready());
    }

    #[test]
    fn an_empty_marker_names_no_payload() {
        let (_scratch, home) = scratch("empty-marker");
        let dir = version_dir(&home, "0.1.0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MARKER), "\n  \n").unwrap();

        assert_eq!(
            status(&home, "0.1.0", "payload-7").unwrap(),
            Status::Interrupted { dir }
        );
    }

    #[test]
    fn a_directory_from_another_payload_is_reported_with_what_it_holds() {
        let (_scratch, home) = scratch("foreign");
        let dir = version_dir(&home, "0.1.0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MARKER), "payload-6\n").unwrap();

        let status = status(&home, "0.1.0", "payload-7").unwrap();
        assert_eq!(
            status,
            Status::Foreign {
                dir,
                found: "payload-6".to_string()
            }
        );
        assert!(!status.ready());
    }

    #[test]
    fn adopting_over_an_interrupted_directory_replaces_it() {
        let (_scratch, home) = scratch("replace-interrupted");
        let leftover = version_dir(&home, "0.1.0");
        fs::create_dir_all(leftover.join("bin")).unwrap();
        fs::write(leftover.join("bin").join("stale"), "half a compiler").unwrap();

        let published = unpack(&home, "0.1.0", "payload-7");

        assert_eq!(published, leftover);
        assert!(!published.join("bin").join("stale").exists());
        assert!(published.join("bin").join("rustc").exists());
        assert_eq!(
            status(&home, "0.1.0", "payload-7").unwrap(),
            Status::Ready { dir: published }
        );
    }

    #[test]
    fn adopting_over_the_same_payload_is_refused_rather_than_replaced() {
        let (_scratch, home) = scratch("refuse-same");
        let published = unpack(&home, "0.1.0", "payload-7");

        let staging = staging(&home, "0.1.0").unwrap();
        let error = staging.adopt("payload-7").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        // And the toolchain that was there is untouched.
        assert_eq!(
            status(&home, "0.1.0", "payload-7").unwrap(),
            Status::Ready { dir: published }
        );
    }

    #[test]
    fn a_staging_directory_dropped_without_adopting_leaves_nothing() {
        let (_scratch, home) = scratch("dropped");
        let staging = staging(&home, "0.1.0").unwrap();
        let path = staging.dir().to_path_buf();
        fs::write(path.join("rustc"), "half a compiler").unwrap();
        drop(staging);

        assert!(!path.exists(), "{} outlived its owner", path.display());
        assert_eq!(status(&home, "0.1.0", "payload-7").unwrap(), Status::Absent);
    }

    #[test]
    fn a_failed_adopt_leaves_nothing_behind_either() {
        let (_scratch, home) = scratch("failed-adopt");
        let staging = staging(&home, "0.1.0").unwrap();
        let path = staging.dir().to_path_buf();
        // An empty payload id is refused after the marker would have been written.
        let error = staging.adopt("   ").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        assert!(!path.exists(), "{} outlived a failed adopt", path.display());
        assert_eq!(status(&home, "0.1.0", "payload-7").unwrap(), Status::Absent);
    }

    #[test]
    fn the_staging_directory_is_a_sibling_of_the_one_it_becomes() {
        let (_scratch, home) = scratch("sibling");
        // The rename that publishes an unpack has to stay inside one filesystem, and it does only
        // if both directories are under the same parent.
        let root = dir(&home);
        assert_eq!(staging_dir(&home, "0.1.0").parent(), Some(root.as_path()));
        assert_eq!(version_dir(&home, "0.1.0").parent(), Some(root.as_path()));
    }

    #[test]
    fn staging_over_a_leftover_of_the_same_version_starts_from_empty() {
        let (_scratch, home) = scratch("reuse-leftover");
        let leftover = staging_dir(&home, "0.1.0");
        fs::create_dir_all(&leftover).unwrap();
        fs::write(leftover.join("rustc"), "half a compiler").unwrap();

        let staging = staging(&home, "0.1.0").unwrap();
        assert_eq!(staging.dir(), leftover);
        assert!(
            !leftover.join("rustc").exists(),
            "an interrupted attempt must not be resumed"
        );
    }

    #[test]
    fn sweep_removes_staging_directories_and_keeps_published_ones() {
        let (_scratch, home) = scratch("sweep");
        let published = unpack(&home, "0.1.0", "payload-7");
        let leftover = staging_dir(&home, "0.2.0");
        fs::create_dir_all(&leftover).unwrap();
        // A file with the suffix is as much a leftover as a directory is.
        let file = dir(&home).join("0.3.0.partial");
        fs::write(&file, "leftover").unwrap();

        let mut removed = sweep(&home).unwrap();
        removed.sort();
        assert_eq!(removed, vec![leftover.clone(), file.clone()]);

        assert!(!leftover.exists());
        assert!(!file.exists());
        assert!(
            published.exists(),
            "a published toolchain is not a leftover"
        );
        assert_eq!(
            status(&home, "0.1.0", "payload-7").unwrap(),
            Status::Ready { dir: published }
        );
    }

    #[test]
    fn sweeping_a_home_with_no_toolchain_directory_is_not_a_fault() {
        let (_scratch, home) = scratch("sweep-absent");
        assert!(sweep(&home).unwrap().is_empty());
        assert!(!dir(&home).exists(), "sweeping must not create the tree");
    }

    #[test]
    fn a_version_that_cannot_be_a_directory_name_is_refused() {
        let (_scratch, home) = scratch("versions");
        let too_long = "9".repeat(MAX_VERSION_CHARS + 1);
        for version in [
            "",
            ".",
            "..",
            "../0.1.0",
            "0.1.0\\win",
            "0.1.0/posix",
            "0.1.0 partial",
            "0.1.0.partial",
            too_long.as_str(),
        ] {
            let error = status(&home, version, "payload-7").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "`{version}`");
            let error = staging(&home, version).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "`{version}`");
        }
    }

    #[test]
    fn every_reading_says_what_it_means() {
        let dir = PathBuf::from("/home/.deepnest/toolchain/0.1.0");
        let readings = [
            (
                Status::Ready { dir: dir.clone() },
                "unpacked at /home/.deepnest/toolchain/0.1.0",
            ),
            (
                Status::Absent,
                "not unpacked yet — it unpacks on the first build",
            ),
            (
                Status::Interrupted { dir: dir.clone() },
                "a previous unpack left /home/.deepnest/toolchain/0.1.0 unfinished; it will be discarded and redone",
            ),
            (
                Status::Foreign {
                    dir: dir.clone(),
                    found: "payload-6".to_string(),
                },
                "/home/.deepnest/toolchain/0.1.0 holds payload payload-6, which this build did not carry; it will be replaced",
            ),
        ];
        for (status, expected) in readings {
            assert_eq!(status.to_string(), expected);
        }
    }
}
