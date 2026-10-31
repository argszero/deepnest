//! The catalog that ships inside the binary: which optional skills exist, and where each comes from.
//!
//! The catalog is **an ordinary skill file**. That is the whole design, and it is worth stating
//! because the alternative — a separate registry format with its own reader, its own schema and its
//! own place in the prompt — costs a second mechanism to do what one file already does. The file
//! has a `name` and a `description` like any skill, so the loader picks it up and the agent sees one
//! line saying "a catalog exists"; the machine-readable part is a `skills:` block in its header, and
//! the instructions below it are prose the agent reads when it decides it needs a capability it does
//! not have.
//!
//! # What an entry must say, and what it may omit
//!
//! An entry carries `name`, `description`, `repo`, `install`, `dest` and `check`. Only the first
//! two are required, and the reason is the same one the loader uses for a skill without a
//! `description`: `name` and `description` are what the agent decides from. The other four describe
//! **how deepnest would fetch the thing**, and not every recommended skill can be fetched — a skill
//! that is a folder someone was given has no repository to clone and no release to compare against.
//!
//! So a complete entry is *installable* and an incomplete one is *recommended only*, and the
//! difference is reported rather than smoothed over: an entry with a `repo` but no `install` says
//! how it would be found and not how it would be put in place, which is a mistake in the catalog
//! rather than a decision. See [`Catalog::installable`] and [`Catalog::recommended_only`].
//!
//! This is a deliberate departure from the reference implementation, which requires all six fields
//! and silently drops an entry that misses one (`impl, ref: emrg/skills/registry.py`). Dropping is
//! the part that does not carry over: a catalog entry that vanishes because someone forgot
//! `check:` looks exactly like a catalog that never had it, and the user's only symptom is an agent
//! that does not know a capability exists.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::atomic;
use crate::frontmatter;

/// The file the catalog lives in, inside the user's skills root.
pub const CATALOG_FILE: &str = "skill-catalog.md";

/// The catalog that ships inside the binary.
///
/// Two entries. `browser-harness` is installable — it has a repository, a CLI that publishes its own
/// skill file, and releases to compare against. `baoyu-design` is recommended only: it is a folder
/// of instructions with no published source, so there is nothing to fetch and nothing to check, and
/// saying so is more use than inventing a `repo` for it.
///
/// The instructions below the header are read by the agent, not by this module. They say what the
/// list means and how to act on it, because a file that is both data and instructions should tell
/// its reader which parts are which.
pub const BUNDLED: &str = r#"---
name: skill-catalog
description: "Catalog of optional skills deepnest can install. Read this when a task needs a capability you do not have — it names what is available, how it is fetched, and how updates are checked."
skills:
  - name: browser-harness
    description: "Direct browser control over CDP: driving the user's own browser for automation, scraping, testing and site work."
    repo: browser-use/browser-harness
    install: self-publishing
    dest: ~/.deepnest/skills/
    check: github_release
  - name: baoyu-design
    description: "Produce visual design artifacts as self-contained HTML: mockups, prototypes, wireframes, landing pages, dashboards, decks, print documents and visual research reports."
    dest: ~/.deepnest/skills/
---

# Optional skills

Everything in the header's `skills:` block is **not installed by default**. When a task needs a
capability deepnest does not have, look here before deciding it cannot be done.

An entry with `repo`, `install` and `check` can be fetched: `install: self-publishing` means the
package installs itself and publishes its own skill file, `dest` is where that file belongs, and
`check: github_release` is how a newer version is noticed.

An entry without them is **recommended only** — it names something worth having and where it belongs,
and the user supplies the files. Deepnest will not claim to install it.

Installing, updating and their state are the user's decision, not the agent's: nothing here is
fetched unless the user asks for it.
"#;

/// The fields an entry may carry.
const KNOWN_FIELDS: [&str; 6] = ["name", "description", "repo", "install", "dest", "check"];

/// One optional skill the catalog names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// The skill's name — what the loader will call it once installed, and what an install asks for.
    pub name: String,
    /// One line saying what it is for. Required: it is what the agent decides from.
    pub description: String,
    /// The repository it comes from, as `owner/repo`. Absent for a recommended-only entry.
    pub repo: Option<String>,
    /// How it publishes its own skill file. The only kind understood today is `self-publishing`.
    pub install: Option<String>,
    /// Where the published file belongs. `~` is the user's home.
    pub dest: Option<String>,
    /// How a newer version is noticed. The only kind understood today is `github_release`.
    pub check: Option<String>,
}

impl Entry {
    /// Whether deepnest could actually fetch this entry.
    ///
    /// All of `repo`, `install` and `dest` — a place to fetch from, a way to turn that into a skill
    /// file, and somewhere to put it. `check` is not required here: an entry that can be installed
    /// once but cannot be compared against a release is a catalog that will not offer updates, which
    /// is a smaller problem than not installing at all.
    pub fn is_installable(&self) -> bool {
        self.repo.is_some() && self.install.is_some() && self.dest.is_some()
    }
}

/// Why a line in the `skills:` block was not an entry.
///
/// Two variants rather than a string, because they want two different fixes: one is a catalog that
/// names half a thing, the other a catalog written in a shape this reader does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    /// It had a `name` and was missing something an entry needs.
    Incomplete {
        /// The name it did give, or a placeholder when it gave none.
        name: String,
        /// The fields it was missing, in the order the file lists them.
        missing: Vec<&'static str>,
    },
    /// It had no `name` at all, so there is nothing to call it.
    Nameless {
        /// The first line of the entry, so the report can point at it.
        first_line: String,
    },
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete { name, missing } => {
                write!(f, "catalog entry `{name}` has no {}", missing.join(", no "))
            }
            Self::Nameless { first_line } => {
                write!(f, "catalog entry has no name: `{first_line}`")
            }
        }
    }
}

/// The catalog, as parsed: what it names, and what it could not read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Entries, in the order the file lists them.
    pub entries: Vec<Entry>,
    /// Lines that were meant to be entries and are not, each with its reason.
    pub rejected: Vec<Rejected>,
}

impl Catalog {
    /// Entries deepnest could fetch.
    pub fn installable(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|entry| entry.is_installable())
    }

    /// Entries that name something worth having and leave the files to the user.
    pub fn recommended_only(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|entry| !entry.is_installable())
    }

    /// One entry, by name.
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// Whether the catalog names nothing at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read the `skills:` block out of a catalog file's text.
    ///
    /// The block is a list of maps, written the way a person writes one:
    ///
    /// ```text
    /// skills:
    ///   - name: browser-harness
    ///     repo: browser-use/browser-harness
    ///   - name: baoyu-design
    /// ```
    ///
    /// A line whose text begins with `- ` opens an entry; the rest are `key: value` lines belonging
    /// to the entry above them. Indentation is not counted, because the shape says it and counting
    /// it would reject a file that is easier to read with two spaces than with four.
    pub fn parse(text: &str) -> Self {
        let Ok((header, _)) = frontmatter::split(text) else {
            // No header at all: the file is not a catalog, and saying "zero entries" without
            // saying why would read as an empty catalog rather than a wrong file.
            return Self {
                entries: Vec::new(),
                rejected: vec![Rejected::Nameless {
                    first_line: "the file has no `---` header".to_string(),
                }],
            };
        };

        let mut entries: Vec<Entry> = Vec::new();
        let mut rejected: Vec<Rejected> = Vec::new();
        let mut current: Option<Entry> = None;
        let mut current_first_line = String::new();

        for raw in header.block("skills") {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (is_entry_start, rest) = match line.strip_prefix("- ") {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            if is_entry_start {
                if let Some(entry) = current.take() {
                    Self::settle(entry, &current_first_line, &mut entries, &mut rejected);
                }
                current_first_line = rest.to_string();
                current = Some(Entry::default());
            }
            let Some(entry) = current.as_mut() else {
                // A `key: value` line before any `- ` opened an entry.
                rejected.push(Rejected::Nameless {
                    first_line: line.to_string(),
                });
                continue;
            };
            let Some((key, value)) = rest.split_once(':') else {
                continue;
            };
            let key = key.trim();
            if !KNOWN_FIELDS.contains(&key) {
                continue;
            }
            let value = unquote(value.trim());
            match key {
                "name" => entry.name = value,
                "description" => entry.description = value,
                "repo" => entry.repo = Some(value).filter(|v| !v.is_empty()),
                "install" => entry.install = Some(value).filter(|v| !v.is_empty()),
                "dest" => entry.dest = Some(value).filter(|v| !v.is_empty()),
                "check" => entry.check = Some(value).filter(|v| !v.is_empty()),
                _ => {}
            }
        }
        if let Some(entry) = current.take() {
            Self::settle(entry, &current_first_line, &mut entries, &mut rejected);
        }

        Self { entries, rejected }
    }

    /// File a finished entry, or say what it was missing.
    fn settle(
        entry: Entry,
        first_line: &str,
        entries: &mut Vec<Entry>,
        rejected: &mut Vec<Rejected>,
    ) {
        if entry.name.is_empty() {
            rejected.push(Rejected::Nameless {
                first_line: first_line.to_string(),
            });
            return;
        }
        if entry.description.is_empty() {
            rejected.push(Rejected::Incomplete {
                name: entry.name,
                missing: vec!["description"],
            });
            return;
        }
        // A `repo` with no `install` is a catalog that says where a thing lives and not how it gets
        // here. That is a mistake in the file, not a decision to recommend without fetching, so it
        // is reported instead of being read as "recommended only".
        if entry.repo.is_some() && entry.install.is_none() {
            rejected.push(Rejected::Incomplete {
                name: entry.name,
                missing: vec!["install"],
            });
            return;
        }
        entries.push(entry);
    }
}

/// A value with one layer of matching quotes removed.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'"' || first == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

/// Where the catalog lives in a given skills root.
pub fn path(root: &Path) -> PathBuf {
    root.join(CATALOG_FILE)
}

/// Write the bundled catalog into `root` if it is not there, and return its path either way.
///
/// The bundled file is a **floor, not a source of truth**: an existing file is never overwritten,
/// because the user's edits and any entry they added live in it. That also covers the two cases this
/// exists for — a fresh install that has no `~/.deepnest/skills/` yet, and an upgrade where the file
/// was deleted — without making an upgrade able to throw away a catalog someone curated.
pub fn ensure(root: &Path) -> io::Result<PathBuf> {
    let target = path(root);
    if target.exists() {
        return Ok(target);
    }
    atomic::write(&target, BUNDLED)?;
    Ok(target)
}

/// Read the catalog from `root`, or report why it could not be read.
///
/// A missing file is an empty catalog and not an error: nothing has installed a catalog yet, which
/// is a state the binary starts in. A file that cannot be read is the same empty catalog — this
/// returns the parsed shape rather than an error because the caller wants to show what is there,
/// and "nothing, and here is why" is what there is.
pub fn load(root: &Path) -> Catalog {
    match fs::read_to_string(path(root)) {
        Ok(text) => Catalog::parse(&text),
        Err(_) => Catalog::default(),
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
                .join(format!("deepnest-catalog-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn root(&self) -> PathBuf {
            self.base.join("skills")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn the_bundled_catalog_names_the_two_skills_the_design_does() {
        let catalog = Catalog::parse(BUNDLED);
        let names: Vec<&str> = catalog.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["browser-harness", "baoyu-design"]);
        assert!(catalog.rejected.is_empty(), "{:?}", catalog.rejected);
    }

    #[test]
    fn an_entry_with_a_repo_and_an_install_is_installable() {
        let catalog = Catalog::parse(BUNDLED);
        let harness = catalog.get("browser-harness").unwrap();
        assert_eq!(harness.repo.as_deref(), Some("browser-use/browser-harness"));
        assert_eq!(harness.install.as_deref(), Some("self-publishing"));
        assert_eq!(harness.dest.as_deref(), Some("~/.deepnest/skills/"));
        assert_eq!(harness.check.as_deref(), Some("github_release"));
        assert!(harness.is_installable());
    }

    #[test]
    fn an_entry_without_a_repo_is_recommended_rather_than_installable() {
        // The design names `baoyu-design` as a catalog entry, and it has no published source. The
        // catalog says so instead of inventing one, so it reads as "worth having" and never as
        // "deepnest will fetch this".
        let catalog = Catalog::parse(BUNDLED);
        let design = catalog.get("baoyu-design").unwrap();
        assert!(design.repo.is_none());
        assert!(!design.is_installable());
        assert_eq!(catalog.installable().count(), 1);
        assert_eq!(catalog.recommended_only().count(), 1);
    }

    #[test]
    fn an_entry_without_a_description_is_reported_not_dropped() {
        let text = "---\nname: c\nskills:\n  - name: notes\n    repo: a/b\n    install: self-publishing\n    dest: ~/.deepnest/skills/\n---\n";
        let catalog = Catalog::parse(text);
        assert!(catalog.is_empty());
        assert_eq!(
            catalog.rejected,
            vec![Rejected::Incomplete {
                name: "notes".to_string(),
                missing: vec!["description"],
            }]
        );
    }

    #[test]
    fn an_entry_that_names_a_repo_but_not_how_to_install_it_is_a_mistake() {
        // Not "recommended only": recommending a thing and knowing where it lives but not how it
        // arrives is a gap in the catalog, and reading it as a decision would hide the gap.
        let text =
            "---\nname: c\nskills:\n  - name: notes\n    description: d\n    repo: a/b\n---\n";
        let catalog = Catalog::parse(text);
        assert!(catalog.is_empty());
        assert_eq!(
            catalog.rejected,
            vec![Rejected::Incomplete {
                name: "notes".to_string(),
                missing: vec!["install"],
            }]
        );
    }

    #[test]
    fn an_entry_with_no_name_is_reported_with_the_line_it_started_on() {
        let text = "---\nname: c\nskills:\n  - description: nameless\n    repo: a/b\n---\n";
        let catalog = Catalog::parse(text);
        assert!(catalog.is_empty());
        assert_eq!(
            catalog.rejected,
            vec![Rejected::Nameless {
                first_line: "description: nameless".to_string(),
            }]
        );
    }

    #[test]
    fn a_file_with_no_header_is_not_an_empty_catalog() {
        let catalog = Catalog::parse("# just prose\n");
        assert!(catalog.is_empty());
        assert_eq!(catalog.rejected.len(), 1, "{:?}", catalog.rejected);
    }

    #[test]
    fn a_header_with_no_skills_block_is_an_empty_catalog() {
        // A skill file that is not a catalog: name and description, no list. Nothing to report.
        let catalog = Catalog::parse("---\nname: notes\ndescription: d\n---\nbody\n");
        assert!(catalog.is_empty());
        assert!(catalog.rejected.is_empty());
    }

    #[test]
    fn unknown_fields_are_ignored_and_do_not_end_an_entry() {
        let text = "---\nname: c\nskills:\n  - name: notes\n    homepage: https://example.invalid\n    description: d\n---\n";
        let catalog = Catalog::parse(text);
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].description, "d");
    }

    #[test]
    fn a_quoted_description_loses_its_quotes_and_keeps_its_colons() {
        let text = "---\nname: c\nskills:\n  - name: notes\n    description: \"a: b\"\n---\n";
        let catalog = Catalog::parse(text);
        assert_eq!(catalog.entries[0].description, "a: b");
    }

    #[test]
    fn a_comment_inside_the_block_is_not_an_entry() {
        let text = "---\nname: c\nskills:\n  # later\n  - name: notes\n    description: d\n---\n";
        let catalog = Catalog::parse(text);
        assert_eq!(catalog.entries.len(), 1);
        assert!(catalog.rejected.is_empty());
    }

    #[test]
    fn ensure_writes_the_bundled_catalog_where_it_is_missing() {
        let scratch = Scratch::new("ensure-missing");
        let written = ensure(&scratch.root()).unwrap();
        assert_eq!(written, path(&scratch.root()));
        let catalog = load(&scratch.root());
        assert_eq!(catalog.entries.len(), 2);
    }

    #[test]
    fn ensure_leaves_a_catalog_that_is_already_there_alone() {
        // The reason this is not "write the bundled file every start": an upgrade would then
        // discard a catalog the user curated. The floor is written once, never refreshed.
        let scratch = Scratch::new("ensure-existing");
        let root = scratch.root();
        fs::create_dir_all(&root).unwrap();
        let edited = "---\nname: skill-catalog\ndescription: mine\nskills:\n  - name: mine\n    description: d\n---\n";
        fs::write(path(&root), edited).unwrap();

        ensure(&root).unwrap();
        assert_eq!(fs::read_to_string(path(&root)).unwrap(), edited);
        assert_eq!(load(&root).get("mine").unwrap().name, "mine");
    }

    #[test]
    fn a_catalog_that_cannot_be_read_is_an_empty_one_not_a_failure() {
        // Unreadable is a state to report, not a reason to take the binary down: the agent simply
        // has no optional skills this run.
        let scratch = Scratch::new("unreadable");
        assert!(load(&scratch.root()).is_empty());
    }

    #[test]
    fn a_half_written_catalog_is_never_what_a_reader_sees() {
        // Written through a temporary neighbour and renamed, so the file at `path` is always whole.
        // Asserted by the leftover check: a scratch file surviving the call would mean the write
        // went somewhere a reader could see it mid-flight.
        let scratch = Scratch::new("atomic");
        ensure(&scratch.root()).unwrap();
        let leftovers: Vec<_> = fs::read_dir(scratch.root())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
