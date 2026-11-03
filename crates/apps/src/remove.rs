//! Deleting an app: the one operation here that destroys user data.
//!
//! Creating writes text. Building runs a toolchain. Starting runs a binary. All three are
//! recoverable by doing the right thing next. This one is not: an app directory holds the user's
//! database, their uploaded files and their agent's whole conversation, and there is no trash and
//! no undo. So the shape of this module is mostly about what it refuses.
//!
//! Four rules, each the reason for a line below:
//!
//! - **Only a direct child of `apps/`, named by a validated [`Slug`].** The slug is the only thing
//!   that reaches a path, and [`Slug`] has already refused `.`, `..` and every separator on three
//!   platforms. Nothing here joins a caller's string to a path.
//! - **A symlink is refused, not followed.** [`crate::list`] already reports one as skipped for the
//!   same reason — following it would let a directory outside `apps/` appear as an app — and
//!   `remove_dir_all` on a symlink is either the link or its target depending on the platform.
//!   Removing the wrong one of those is not a bug to fix later.
//! - **A missing directory is an error, not a success.** "Deleted" for something that was not there
//!   is the one answer that makes a user believe data is gone when it never was, or believe it is
//!   still there when it is not. The caller gets [`RemoveError::NotFound`] and says so.
//! - **What was removed is reported, not assumed.** The count and the bytes are what a confirmation
//!   can be honest about, and they are also the only evidence that the directory was the size the
//!   user thought it was.
//!
//! What this does **not** decide: whether the caller really meant it. A confirmation is a question
//! to a person, and the route (`POST /api/apps/<slug>/delete`) is where that question is asked and
//! answered — by requiring the slug itself in the query, so a stray `POST` deletes nothing.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::slug::Slug;

/// What a delete removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// The app's name, as the caller spelled it.
    pub slug: String,
    /// The directory that is now gone.
    pub dir: PathBuf,
    /// Every entry under it, files and directories both.
    pub entries: usize,
    /// The bytes of the files under it. Directories themselves count for nothing, which is what
    /// `du`-style reporting means everywhere.
    pub bytes: u64,
}

/// Why a delete did not happen.
#[derive(Debug)]
pub enum RemoveError {
    /// There is no directory with that name under `apps/`.
    NotFound { dir: PathBuf },
    /// It is there but is a symlink, and this will not decide which of two things that means.
    Symlink { dir: PathBuf },
    /// It is there but is not a directory: a file someone put in `apps/`.
    NotADirectory { dir: PathBuf },
    /// The filesystem refused, part way through or before it started.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for RemoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The sentence a caller shows, so each one names the next step rather than the fault.
            RemoveError::NotFound { dir } => write!(
                f,
                "there is no app directory at {} — nothing was deleted",
                dir.display()
            ),
            RemoveError::Symlink { dir } => write!(
                f,
                "{} is a symlink, and deleting one is deleting either the link or what it points \
                 at. Remove it yourself if that is what you meant",
                dir.display()
            ),
            RemoveError::NotADirectory { dir } => {
                write!(f, "{} is not a directory", dir.display())
            }
            RemoveError::Io { path, source } => {
                write!(f, "{} could not be removed: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for RemoveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RemoveError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Delete the app `slug` from `apps_dir`, and say what went.
///
/// The directory is removed recursively and the deletion cannot be undone; see this module's own
/// note on why that is the caller's question and not this function's.
pub fn remove(apps_dir: &Path, slug: &Slug) -> Result<Removed, RemoveError> {
    let dir = apps_dir.join(slug.as_str());

    // `symlink_metadata` rather than `metadata`: the question is what is *at* this path, not what
    // it points at. A `NotFound` here is the honest answer for a name nobody created.
    let metadata = match fs::symlink_metadata(&dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(RemoveError::NotFound { dir })
        }
        Err(source) => return Err(RemoveError::Io { path: dir, source }),
    };
    if metadata.file_type().is_symlink() {
        return Err(RemoveError::Symlink { dir });
    }
    if !metadata.is_dir() {
        return Err(RemoveError::NotADirectory { dir });
    }

    // Counted before the removal, because afterwards there is nothing to count. A failure to walk
    // stops the delete: reporting "removed 0 bytes" for a directory that could not be read would
    // be a confirmation of the wrong thing.
    let (entries, bytes) = measure(&dir)?;

    fs::remove_dir_all(&dir).map_err(|source| RemoveError::Io {
        path: dir.clone(),
        source,
    })?;

    Ok(Removed {
        slug: slug.as_str().to_string(),
        dir,
        entries,
        bytes,
    })
}

/// How many entries are under `dir`, and how many bytes the files among them hold.
///
/// Symlinks inside the tree are counted as entries and not followed, for the reason the top-level
/// one is refused: following them would report the size of somewhere else entirely.
fn measure(dir: &Path) -> Result<(usize, u64), RemoveError> {
    let mut entries = 0usize;
    let mut bytes = 0u64;
    let mut pending = vec![dir.to_path_buf()];

    while let Some(current) = pending.pop() {
        let listing = fs::read_dir(&current).map_err(|source| RemoveError::Io {
            path: current.clone(),
            source,
        })?;
        for entry in listing {
            let entry = entry.map_err(|source| RemoveError::Io {
                path: current.clone(),
                source,
            })?;
            entries += 1;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| RemoveError::Io {
                path: path.clone(),
                source,
            })?;
            if metadata.is_dir() {
                pending.push(path);
            } else if !metadata.file_type().is_symlink() {
                bytes += metadata.len();
            }
        }
    }
    Ok((entries, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-remove-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(base.join("apps")).unwrap();
            Self { base }
        }

        fn apps(&self) -> PathBuf {
            self.base.join("apps")
        }

        /// An app the way `create` leaves one: a manifest and data underneath it.
        fn app(&self, name: &str) {
            let dir = self.apps().join(name);
            fs::create_dir_all(dir.join(".agent").join("memory")).unwrap();
            fs::write(dir.join("deepnest.toml"), "[app]\nname = \"Notes\"\n").unwrap();
            fs::write(dir.join("app.db"), "the user's data").unwrap();
            fs::write(dir.join(".agent").join("history.jsonl"), "one turn").unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn slug(name: &str) -> Slug {
        Slug::parse(name).unwrap()
    }

    #[test]
    fn deleting_an_app_removes_its_directory_and_reports_what_went() {
        let scratch = Scratch::new("whole");
        scratch.app("notes");

        let removed = remove(&scratch.apps(), &slug("notes")).unwrap();
        assert_eq!(removed.slug, "notes");
        assert!(!scratch.apps().join("notes").exists());
        // The evidence a confirmation is honest about: the data, the conversation, the manifest.
        assert_eq!(removed.entries, 5, "{removed:?}");
        assert!(removed.bytes >= 12 + 14 + 8, "{removed:?}");
    }

    #[test]
    fn another_app_is_untouched() {
        // The property that makes this safe to offer at all: it deletes one directory, named by one
        // slug, and a second app beside it is not part of anything it reads.
        let scratch = Scratch::new("neighbour");
        scratch.app("notes");
        scratch.app("todo");

        remove(&scratch.apps(), &slug("notes")).unwrap();
        assert!(scratch.apps().join("todo").join("app.db").is_file());
    }

    #[test]
    fn a_name_that_is_not_there_is_an_error_rather_than_a_success() {
        // "Deleted" for something that was never there is a claim the user acts on.
        let scratch = Scratch::new("missing");
        let error = remove(&scratch.apps(), &slug("notes")).unwrap_err();
        assert!(matches!(error, RemoveError::NotFound { .. }), "{error:?}");
        assert!(error.to_string().contains("nothing was deleted"), "{error}");
    }

    // Creating a symlink needs a privilege Windows does not grant a normal account, so the setup
    // only exists on unix; `remove` itself checks the entry the same way on both.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused_and_neither_side_of_it_is_removed() {
        let scratch = Scratch::new("symlink");
        let outside = scratch.base.join("somewhere-else");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious.db"), "not this app's problem").unwrap();
        std::os::unix::fs::symlink(&outside, scratch.apps().join("notes")).unwrap();

        let error = remove(&scratch.apps(), &slug("notes")).unwrap_err();
        assert!(matches!(error, RemoveError::Symlink { .. }), "{error:?}");
        // Both survive: the link, and what it points at.
        assert!(scratch.apps().join("notes").symlink_metadata().is_ok());
        assert!(outside.join("precious.db").is_file());
    }

    #[test]
    fn a_plain_file_in_the_apps_directory_is_not_a_directory_to_delete() {
        let scratch = Scratch::new("file");
        fs::write(scratch.apps().join("notes"), "a file").unwrap();
        let error = remove(&scratch.apps(), &slug("notes")).unwrap_err();
        assert!(
            matches!(error, RemoveError::NotADirectory { .. }),
            "{error:?}"
        );
        assert!(scratch.apps().join("notes").is_file(), "it was not deleted");
    }
}
