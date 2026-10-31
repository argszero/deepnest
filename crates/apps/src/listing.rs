//! What is under `~/.deepnest/apps/`, and what is there but is not an app.
//!
//! The listing has two halves on purpose. `apps` is what the panel shows; `skipped` is every
//! directory that looks like it might be one and is not, with the reason. A single `Vec<App>` would
//! be shorter and would also make "no apps yet" and "your app is there but its manifest has a typo"
//! the same reading — and the user's only evidence for the second would be that the panel seems
//! emptier than it should be.
//!
//! Two rules about what counts:
//!
//! - **A missing `apps/` directory is an empty listing, not an error.** First run has not created
//!   it yet, and refusing to list apps before any exist would make the first thing a new user sees
//!   a failure.
//! - **A symlink is not an app.** It is reported as skipped rather than followed: following one
//!   would let a directory outside `apps/` — including one the sandbox never agreed to — appear as
//!   an app, and the writable set is built from app directories.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::manifest::{self, Manifest};
use crate::slug::Slug;

/// An app that is present and usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// The validated directory name, which is also the URL segment.
    pub slug: Slug,
    /// The directory itself.
    pub dir: PathBuf,
    /// Its parsed manifest.
    pub manifest: Manifest,
}

impl App {
    /// The name to show for this app.
    pub fn display_name(&self) -> &str {
        self.manifest.display_name()
    }
}

/// Something under `apps/` that is not an app, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// Everything under `apps/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// The apps that were found, sorted by slug.
    pub apps: Vec<App>,
    /// The entries that were not apps, sorted by path.
    pub skipped: Vec<Skipped>,
}

impl Listing {
    /// Whether any app was found.
    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }
}

/// List the apps in `apps_dir`.
///
/// A directory that does not exist yet yields an empty listing: that is first run, not a fault.
pub fn list(apps_dir: &Path) -> io::Result<Listing> {
    let entries = match fs::read_dir(apps_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Listing::default()),
        Err(error) => return Err(error),
    };

    let mut listing = Listing::default();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // One unreadable entry must not hide the apps that are readable.
                listing.skipped.push(Skipped {
                    path: apps_dir.to_path_buf(),
                    reason: format!("an entry could not be read: {error}"),
                });
                continue;
            }
        };
        let path = entry.path();

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                listing.skipped.push(Skipped {
                    path,
                    reason: format!("its type could not be read: {error}"),
                });
                continue;
            }
        };
        if file_type.is_symlink() {
            listing.skipped.push(Skipped {
                path,
                reason: "a symlink; deepnest lists only real directories under apps/".to_string(),
            });
            continue;
        }
        if !file_type.is_dir() {
            listing.skipped.push(Skipped {
                path,
                reason: "not a directory".to_string(),
            });
            continue;
        }

        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            listing.skipped.push(Skipped {
                path,
                reason: "its name is not valid UTF-8, so it cannot be a slug".to_string(),
            });
            continue;
        };

        let slug = match Slug::parse(name) {
            Ok(slug) => slug,
            Err(error) => {
                listing.skipped.push(Skipped {
                    path,
                    reason: format!("its name is not a usable slug: {error}"),
                });
                continue;
            }
        };

        match manifest::load(&path) {
            Ok(manifest) => listing.apps.push(App {
                slug,
                dir: path,
                manifest,
            }),
            Err(error) => listing.skipped.push(Skipped {
                path,
                reason: error.to_string(),
            }),
        }
    }

    listing
        .apps
        .sort_by(|left, right| left.slug.cmp(&right.slug));
    listing
        .skipped
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(listing)
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
                .join(format!("deepnest-listing-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn apps(&self) -> PathBuf {
            self.base.join("apps")
        }

        fn app(&self, slug: &str, name: &str) -> PathBuf {
            let dir = self.apps().join(slug);
            Manifest::new(name).save(&dir).unwrap();
            dir
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_missing_apps_directory_is_an_empty_listing_not_an_error() {
        let scratch = Scratch::new("missing-dir");
        let listing = list(&scratch.apps()).unwrap();

        assert!(listing.is_empty());
        assert!(listing.skipped.is_empty());
    }

    #[test]
    fn an_empty_apps_directory_lists_nothing() {
        let scratch = Scratch::new("empty-dir");
        fs::create_dir_all(scratch.apps()).unwrap();

        assert!(list(&scratch.apps()).unwrap().is_empty());
    }

    #[test]
    fn an_app_with_a_manifest_is_found() {
        let scratch = Scratch::new("one-app");
        let dir = scratch.app("notes", "Notes");

        let listing = list(&scratch.apps()).unwrap();

        assert_eq!(listing.apps.len(), 1, "{listing:?}");
        assert!(listing.skipped.is_empty(), "{listing:?}");
        assert_eq!(listing.apps[0].slug.as_str(), "notes");
        assert_eq!(listing.apps[0].dir, dir);
        assert_eq!(listing.apps[0].display_name(), "Notes");
    }

    #[test]
    fn apps_are_listed_in_slug_order_whatever_the_filesystem_returns() {
        let scratch = Scratch::new("order");
        for (slug, name) in [("zeta", "Zeta"), ("alpha", "Alpha"), ("mid-1", "Mid")] {
            scratch.app(slug, name);
        }

        let names: Vec<String> = list(&scratch.apps())
            .unwrap()
            .apps
            .into_iter()
            .map(|app| app.slug.as_str().to_string())
            .collect();
        assert_eq!(names, ["alpha", "mid-1", "zeta"]);
    }

    #[test]
    fn a_stray_file_is_skipped_and_named() {
        let scratch = Scratch::new("stray-file");
        fs::create_dir_all(scratch.apps()).unwrap();
        let stray = scratch.apps().join("notes.txt");
        fs::write(&stray, "not an app").unwrap();

        let listing = list(&scratch.apps()).unwrap();

        assert!(listing.is_empty());
        assert_eq!(listing.skipped.len(), 1, "{listing:?}");
        assert_eq!(listing.skipped[0].path, stray);
        assert!(
            listing.skipped[0].reason.contains("not a directory"),
            "{:?}",
            listing.skipped[0]
        );
    }

    #[test]
    fn a_directory_whose_name_is_not_a_slug_is_skipped_with_the_reason() {
        // The common case: someone made the folder by hand with a capital letter.
        let scratch = Scratch::new("bad-name");
        fs::create_dir_all(scratch.apps().join("Notes")).unwrap();

        let listing = list(&scratch.apps()).unwrap();

        assert!(listing.is_empty());
        assert_eq!(listing.skipped.len(), 1, "{listing:?}");
        assert!(
            listing.skipped[0].reason.contains("slug")
                && listing.skipped[0].reason.contains("lowercase"),
            "{:?}",
            listing.skipped[0]
        );
    }

    #[test]
    fn a_directory_without_a_manifest_is_skipped_and_the_reason_names_the_file() {
        let scratch = Scratch::new("no-manifest");
        fs::create_dir_all(scratch.apps().join("notes")).unwrap();

        let listing = list(&scratch.apps()).unwrap();

        assert!(listing.is_empty());
        assert!(
            listing.skipped[0].reason.contains(manifest::FILE_NAME),
            "{:?}",
            listing.skipped[0]
        );
    }

    #[test]
    fn a_broken_app_does_not_hide_the_healthy_ones() {
        let scratch = Scratch::new("mixed");
        scratch.app("good", "Good");
        let broken = scratch.apps().join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(
            broken.join(manifest::FILE_NAME),
            "[app]\nnmae = \"Broken\"\n",
        )
        .unwrap();

        let listing = list(&scratch.apps()).unwrap();

        assert_eq!(listing.apps.len(), 1, "{listing:?}");
        assert_eq!(listing.apps[0].slug.as_str(), "good");
        assert_eq!(listing.skipped.len(), 1, "{listing:?}");
        assert_eq!(listing.skipped[0].path, broken);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_reported_rather_than_followed() {
        // Following it would let a directory the sandbox never agreed to appear as an app, and the
        // writable set is built from app directories.
        let scratch = Scratch::new("symlink");
        let outside = scratch.app("real", "Real");
        fs::create_dir_all(scratch.apps()).unwrap();
        let link = scratch.apps().join("linked");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let listing = list(&scratch.apps()).unwrap();

        assert_eq!(listing.apps.len(), 1, "{listing:?}");
        assert_eq!(listing.apps[0].slug.as_str(), "real");
        assert_eq!(listing.skipped.len(), 1, "{listing:?}");
        assert_eq!(listing.skipped[0].path, link);
        assert!(
            listing.skipped[0].reason.contains("symlink"),
            "{:?}",
            listing.skipped[0]
        );
    }
}
