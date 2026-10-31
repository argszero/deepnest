//! Where skills come from, and what the prompt is allowed to see of them.
//!
//! A skill is instructions the agent can choose to follow: Markdown, a header, and a body the agent
//! reads only when it decides the skill is relevant. This module finds them, and hands the caller
//! a list whose every entry is a name, a one-line description and a path — never the body. That is
//! the progressive-disclosure rule, and it is the whole reason the list can be in a prompt at all:
//! a dozen skills' worth of instructions would crowd out the conversation, while a dozen names and
//! descriptions cost a paragraph.
//!
//! # The two roots, and who wins
//!
//! - `~/.deepnest/skills/` — the user's, shared by every app.
//! - `<app>/.agent/skills/` — one app's own, and it **shadows** a user skill of the same name.
//!
//! The direction is the design's (`host 20:26`) and it is the useful one: an app that needs a
//! different `notes` skill should not have to rename the user's.
//!
//! # The two layouts, and which wins
//!
//! Both are Anthropic-compatible spellings of the same thing:
//!
//! - `skills/notes.md` — one file.
//! - `skills/notes/SKILL.md` — a directory, which can carry the scripts and assets a skill refers
//!   to.
//!
//! When both spellings declare the same name **in the same root**, the directory wins
//! (`decided in implementation`, 2026-10-31): it is the layout that can hold more, so a skill
//! being migrated to it keeps working mid-move. The losing file is reported rather than dropped —
//! a user editing `notes.md` and seeing no change would otherwise have nothing to look at.
//!
//! # What a skill may not do
//!
//! A skill is instructions, not a program and not a permission. It cannot widen the sandbox, and
//! what it says is still subject to the fence like anything else the agent does. That is why
//! nothing here is signed, versioned or trusted: the same rule that makes a skill cheap to write
//! is the rule that makes it worth no more than the agent's own judgement.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::frontmatter;

/// The directory under the deepnest home that holds the user's skills.
pub const USER_DIR: &str = "skills";

/// The directory inside an app that holds that app's own skills.
pub const APP_DIR: &str = ".agent/skills";

/// The file a skill directory is recognised by.
pub const SKILL_FILE: &str = "SKILL.md";

/// `~/.deepnest/skills` — where the user's skills live.
pub fn user_root(home: &Path) -> PathBuf {
    home.join(USER_DIR)
}

/// `<app>/.agent/skills` — where one app's skills live.
pub fn app_root(app: &Path) -> PathBuf {
    app.join(APP_DIR)
}

/// Which root a skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `~/.deepnest/skills/`.
    User,
    /// `<app>/.agent/skills/`.
    App,
}

impl Source {
    /// The word that appears in the prompt, and in a skip message.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::App => "app",
        }
    }
}

/// A skill the agent may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// What the agent calls it. From the header, or the file's own name when the header is silent.
    pub name: String,
    /// One line saying when to use it. Never empty: a skill without one is not loaded.
    pub description: String,
    /// The file, for the agent's `read` tool to open.
    pub path: PathBuf,
    /// Which root it came from.
    pub source: Source,
    /// The instructions, below the header.
    pub body: String,
}

/// Something under a skills root that is not a usable skill, and why.
///
/// The same shape as `deepnest_apps::Skipped`, for the same reason: a single list would make "no
/// skills yet" and "your skill's header has a typo" the same reading, and the user's only clue to
/// the second would be a prompt that seems emptier than it should be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// Everything found under the roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    /// The skills, sorted by name, with every shadow already resolved.
    pub skills: Vec<Skill>,
    /// The entries that were not skills, sorted by path.
    pub skipped: Vec<Skipped>,
}

impl Loaded {
    /// Whether any skill was found.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// One skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }
}

/// Which of the two layouts an entry arrived in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Flat,
    Directory,
}

/// Load the skills an app can use.
///
/// `home` is the deepnest home (`~/.deepnest`), not the user's home. `app` is the app's own
/// directory, when there is one: a caller that has no app yet loads the user's skills alone.
///
/// A root that does not exist yields nothing and is not an error — first run has not created a
/// skills directory, and refusing to start before any exist would make the first thing a new user
/// sees a failure.
pub fn load(home: &Path, app: Option<&Path>) -> Loaded {
    let mut loaded = Loaded::default();
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();

    for (root, source) in [
        (user_root(home), Source::User),
        match app {
            Some(app) => (app_root(app), Source::App),
            // A replaced placeholder rather than an `if`: the loop then has no second shape.
            None => (PathBuf::new(), Source::App),
        },
    ] {
        if root.as_os_str().is_empty() {
            continue;
        }
        scan(&root, source, &mut by_name, &mut loaded.skipped);
    }

    loaded.skills = by_name.into_values().collect();
    loaded.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    loaded
}

/// Read one root, resolving its own collisions as it goes.
fn scan(
    root: &Path,
    source: Source,
    by_name: &mut BTreeMap<String, Skill>,
    skipped: &mut Vec<Skipped>,
) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            skipped.push(Skipped {
                path: root.to_path_buf(),
                reason: format!("this directory could not be read: {error}"),
            });
            return;
        }
    };

    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(error) => skipped.push(Skipped {
                path: root.to_path_buf(),
                reason: format!("an entry could not be read: {error}"),
            }),
        }
    }
    // Deterministic order, because which file wins a collision must not depend on the filesystem's.
    paths.sort();

    let mut flats = Vec::new();
    let mut directories = Vec::new();
    for path in paths {
        let Ok(file_type) = fs::symlink_metadata(&path).map(|meta| meta.file_type()) else {
            skipped.push(Skipped {
                path,
                reason: "its type could not be read".to_string(),
            });
            continue;
        };
        // A symlink is reported rather than followed: following one would let a skill live
        // somewhere the user never put it, and "where did this come from" is the question a skill
        // list has to be able to answer.
        if file_type.is_symlink() {
            skipped.push(Skipped {
                path,
                reason: "a symlink; skills are read from real files under the skills directory"
                    .to_string(),
            });
            continue;
        }
        if file_type.is_file() {
            let is_markdown = path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"));
            if !is_markdown {
                skipped.push(Skipped {
                    path,
                    reason: "not a Markdown file, so it cannot be a skill".to_string(),
                });
                continue;
            }
            flats.push(path);
            continue;
        }
        if file_type.is_dir() {
            let file = path.join(SKILL_FILE);
            if !file.is_file() {
                skipped.push(Skipped {
                    path,
                    reason: format!("no {SKILL_FILE} inside, so this directory is not a skill"),
                });
                continue;
            }
            directories.push(path);
            continue;
        }
        skipped.push(Skipped {
            path,
            reason: "neither a file nor a directory".to_string(),
        });
    }

    // Flat files first, then directories: within one root the directory layout wins a collision,
    // and doing it in this order makes "later wins" the only rule the loop needs.
    for (path, layout) in flats.into_iter().map(|path| (path, Layout::Flat)).chain(
        directories
            .into_iter()
            .map(|path| (path, Layout::Directory)),
    ) {
        let fallback = match layout {
            Layout::Flat => path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string(),
            Layout::Directory => path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
        };

        let skill = match read_skill(&path, layout, source, &fallback, skipped) {
            Some(skill) => skill,
            None => continue,
        };

        match by_name.get(&skill.name) {
            // The same name from a different root: the app's skills shadow the user's, which is
            // the design, and is not a problem to report.
            Some(existing) if existing.source != source => {
                by_name.insert(skill.name.clone(), skill);
            }
            // The same name from the same root: the directory layout wins, and the file it beat is
            // reported — an edit to a file that is not the one being read must not be silent.
            Some(existing) => {
                skipped.push(Skipped {
                    path: existing.path.clone(),
                    reason: format!(
                        "another entry in the same directory declares the name `{}`, and the \
                         `{SKILL_FILE}` layout wins",
                        skill.name
                    ),
                });
                by_name.insert(skill.name.clone(), skill);
            }
            None => {
                by_name.insert(skill.name.clone(), skill);
            }
        }
    }
}

/// Read one candidate file into a skill, or explain why it is not one.
fn read_skill(
    path: &Path,
    layout: Layout,
    source: Source,
    fallback_name: &str,
    skipped: &mut Vec<Skipped>,
) -> Option<Skill> {
    let file = match layout {
        Layout::Flat => path.to_path_buf(),
        Layout::Directory => path.join(SKILL_FILE),
    };

    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) => {
            skipped.push(Skipped {
                path: file,
                reason: format!("it could not be read: {error}"),
            });
            return None;
        }
    };

    let (frontmatter, body) = match frontmatter::split(&text) {
        Ok(split) => split,
        Err(reason) => {
            skipped.push(Skipped {
                path: file,
                reason: reason.to_string(),
            });
            return None;
        }
    };

    // A skill with no description is not loaded. The description is what the agent decides from,
    // so a skill without one cannot be chosen — and a name alone in the prompt would be a line the
    // agent has no way to act on.
    let Some(description) = frontmatter.get("description") else {
        skipped.push(Skipped {
            path: file,
            reason: "its header has no `description`, so the agent has nothing to choose it by"
                .to_string(),
        });
        return None;
    };
    let description = description.trim();
    if description.is_empty() {
        skipped.push(Skipped {
            path: file,
            reason: "its `description` is empty, so the agent has nothing to choose it by"
                .to_string(),
        });
        return None;
    }

    let name = frontmatter
        .get("name")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback_name)
        .to_string();
    if name.is_empty() {
        skipped.push(Skipped {
            path: file,
            reason: "it has no `name` and its path does not supply one".to_string(),
        });
        return None;
    }

    Some(Skill {
        name,
        description: description.to_string(),
        path: file,
        source,
        body: body.to_string(),
    })
}

/// The skill list as the prompt sees it: a name, a description and a path each, and no bodies.
///
/// Empty when there are no skills, so a caller can append the result without testing it — a
/// heading with nothing under it is a section the agent reads and learns nothing from.
pub fn summary(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(skills.len() * 96 + 256);
    out.push_str("## Available Skills\n\n");
    out.push_str(
        "The following skills are available. When a skill seems relevant to the request, read its \
         file at the listed path and follow its instructions. Do not describe a skill you have not \
         read.\n\n",
    );
    for skill in skills {
        out.push_str("- **");
        out.push_str(&skill.name);
        out.push_str("** (");
        out.push_str(skill.source.as_str());
        out.push_str(", `");
        out.push_str(&skill.path.display().to_string());
        out.push_str("`): ");
        out.push_str(&skill.description);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-skills-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn home(&self) -> PathBuf {
            self.base.join("home")
        }

        fn app(&self) -> PathBuf {
            self.base.join("app")
        }

        /// A flat skill in the user's root.
        fn user_skill(&self, file: &str, name: &str, description: &str) -> PathBuf {
            self.write(
                &self.home().join(USER_DIR).join(file),
                name,
                description,
                "body",
            )
        }

        /// A directory-layout skill in an app's root.
        fn app_skill(&self, dir: &str, name: &str, description: &str) -> PathBuf {
            let path = self.app().join(APP_DIR).join(dir).join(SKILL_FILE);
            self.write(&path, name, description, "body")
        }

        fn write(&self, path: &Path, name: &str, description: &str, body: &str) -> PathBuf {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
            )
            .unwrap();
            path.to_path_buf()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn no_skills_directories_at_all_is_an_empty_load_not_an_error() {
        let scratch = Scratch::new("missing");
        let loaded = load(&scratch.home(), Some(&scratch.app()));
        assert!(loaded.is_empty());
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
    }

    #[test]
    fn a_flat_file_and_a_directory_layout_are_both_skills() {
        let scratch = Scratch::new("layouts");
        let flat = scratch.user_skill("notes.md", "notes", "Take notes");
        let nested = scratch.app_skill("browser", "browser", "Drive a browser");

        let loaded = load(&scratch.home(), Some(&scratch.app()));
        assert_eq!(loaded.skills.len(), 2);
        assert_eq!(loaded.get("notes").unwrap().path, flat);
        assert_eq!(loaded.get("browser").unwrap().path, nested);
        assert_eq!(loaded.get("notes").unwrap().source, Source::User);
        assert_eq!(loaded.get("browser").unwrap().source, Source::App);
    }

    #[test]
    fn an_app_skill_shadows_a_user_skill_of_the_same_name() {
        let scratch = Scratch::new("shadow");
        scratch.user_skill("notes.md", "notes", "The user's");
        scratch.app_skill("notes", "notes", "This app's");

        let loaded = load(&scratch.home(), Some(&scratch.app()));
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.get("notes").unwrap().description, "This app's");
        assert_eq!(loaded.get("notes").unwrap().source, Source::App);
        // Shadowing is the design, not a fault: nothing is reported.
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
    }

    #[test]
    fn the_directory_layout_wins_a_collision_inside_one_root_and_the_file_is_reported() {
        let scratch = Scratch::new("collision");
        let flat = scratch.user_skill("notes.md", "notes", "From the file");
        let dir = scratch.home().join(USER_DIR).join("notes").join(SKILL_FILE);
        scratch.write(&dir, "notes", "From the directory", "body");

        let loaded = load(&scratch.home(), None);
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(
            loaded.get("notes").unwrap().description,
            "From the directory"
        );
        assert_eq!(loaded.skipped.len(), 1, "{:?}", loaded.skipped);
        assert_eq!(loaded.skipped[0].path, flat);
        assert!(
            loaded.skipped[0].reason.contains("wins"),
            "{:?}",
            loaded.skipped
        );
        assert!(dir.is_file());
    }

    #[test]
    fn a_skill_without_a_description_is_not_loaded_and_says_why() {
        let scratch = Scratch::new("no-description");
        let path = scratch.home().join(USER_DIR).join("nameless.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "---\nname: nameless\n---\nbody\n").unwrap();

        let loaded = load(&scratch.home(), None);
        assert!(loaded.is_empty());
        assert_eq!(loaded.skipped.len(), 1);
        assert!(
            loaded.skipped[0].reason.contains("description"),
            "{:?}",
            loaded.skipped
        );
    }

    #[test]
    fn an_empty_description_is_not_a_description() {
        let scratch = Scratch::new("empty-description");
        let path = scratch.home().join(USER_DIR).join("blank.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "---\nname: blank\ndescription:   \n---\nbody\n").unwrap();

        let loaded = load(&scratch.home(), None);
        assert!(loaded.is_empty());
        assert!(
            loaded.skipped[0].reason.contains("empty"),
            "{:?}",
            loaded.skipped
        );
    }

    #[test]
    fn the_name_falls_back_to_the_path_when_the_header_is_silent() {
        let scratch = Scratch::new("fallback-name");
        let path = scratch.home().join(USER_DIR).join("from-file.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "---\ndescription: d\n---\nbody\n").unwrap();
        let dir = scratch
            .home()
            .join(USER_DIR)
            .join("from-dir")
            .join(SKILL_FILE);
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::write(&dir, "---\ndescription: d\n---\nbody\n").unwrap();

        let loaded = load(&scratch.home(), None);
        assert_eq!(loaded.skills.len(), 2);
        assert!(loaded.get("from-file").is_some(), "{:?}", loaded.skills);
        assert!(loaded.get("from-dir").is_some(), "{:?}", loaded.skills);
    }

    #[test]
    fn a_directory_without_a_skill_file_is_reported() {
        // `SKILL.md` is not `README.md`, and a directory holding only the latter is a directory the
        // user has to be told about rather than one that quietly contributes nothing.
        //
        // The spelling that would be most useful to catch — `skill.md` for `SKILL.md` — cannot be
        // caught on macOS or Windows, where the two names are one file and this loader finds it.
        // The rule still holds where it matters: on a case-sensitive filesystem, which is where the
        // app is built on Linux.
        let scratch = Scratch::new("no-skill-file");
        let dir = scratch.home().join(USER_DIR).join("notes");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("README.md"), "# not a skill\n").unwrap();

        let loaded = load(&scratch.home(), None);
        assert!(loaded.is_empty());
        assert_eq!(loaded.skipped.len(), 1, "{:?}", loaded.skipped);
        assert!(
            loaded.skipped[0].reason.contains(SKILL_FILE),
            "{:?}",
            loaded.skipped
        );
    }

    #[test]
    fn a_file_that_is_not_markdown_is_reported_rather_than_read() {
        let scratch = Scratch::new("not-markdown");
        let path = scratch.home().join(USER_DIR).join("notes.txt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "---\ndescription: d\n---\n").unwrap();

        let loaded = load(&scratch.home(), None);
        assert!(loaded.is_empty());
        assert!(
            loaded.skipped[0].reason.contains("Markdown"),
            "{:?}",
            loaded.skipped
        );
    }

    #[test]
    fn a_file_with_a_broken_header_is_reported_with_the_reason_the_parser_gave() {
        let scratch = Scratch::new("bad-header");
        let path = scratch.home().join(USER_DIR).join("broken.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# just markdown\n").unwrap();

        let loaded = load(&scratch.home(), None);
        assert!(loaded.is_empty());
        assert!(
            loaded.skipped[0].reason.contains("---"),
            "{:?}",
            loaded.skipped
        );
    }

    #[test]
    fn a_symlinked_skill_is_reported_rather_than_followed() {
        let scratch = Scratch::new("symlink");
        let real = scratch.write(
            &scratch.base.join("outside").join("notes.md"),
            "notes",
            "Outside the skills directory",
            "body",
        );
        let link = scratch.home().join(USER_DIR).join("notes.md");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        {
            // Windows needs a privilege for symlinks; the rule is asserted on unix and the test
            // still has to say something true elsewhere.
            return;
        }

        let loaded = load(&scratch.home(), None);
        assert!(loaded.is_empty());
        assert_eq!(loaded.skipped.len(), 1);
        assert!(
            loaded.skipped[0].reason.contains("symlink"),
            "{:?}",
            loaded.skipped
        );
    }

    #[test]
    fn the_body_travels_with_the_skill_so_the_agent_can_be_told_to_read_it() {
        let scratch = Scratch::new("body");
        scratch.user_skill("notes.md", "notes", "Take notes");
        let loaded = load(&scratch.home(), None);
        assert_eq!(loaded.get("notes").unwrap().body, "body");
    }

    #[test]
    fn skills_are_sorted_by_name_whatever_order_the_filesystem_gives() {
        let scratch = Scratch::new("sorted");
        for name in ["zebra", "alpha", "middle"] {
            scratch.user_skill(&format!("{name}.md"), name, "d");
        }
        let loaded = load(&scratch.home(), None);
        let names: Vec<&str> = loaded.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "middle", "zebra"]);
    }

    #[test]
    fn the_summary_carries_a_name_a_description_and_a_path_and_no_body() {
        let scratch = Scratch::new("summary");
        let path = scratch.user_skill("notes.md", "notes", "Take notes");
        // A body distinctive enough that finding it in the prompt would be unambiguous.
        fs::write(
            &path,
            "---\nname: notes\ndescription: Take notes\n---\nSECRET-INSTRUCTIONS\n",
        )
        .unwrap();
        let loaded = load(&scratch.home(), None);

        let summary = summary(&loaded.skills);
        assert!(summary.starts_with("## Available Skills"), "{summary}");
        assert!(summary.contains("**notes** (user, `"), "{summary}");
        assert!(summary.contains("): Take notes"), "{summary}");
        // Progressive disclosure: the instructions are not in the prompt, and neither is the
        // header's own delimiters.
        assert!(!summary.contains("SECRET-INSTRUCTIONS"), "{summary}");
        assert!(!summary.contains("---"), "{summary}");
    }

    #[test]
    fn a_summary_of_nothing_is_empty_rather_than_a_heading_over_nothing() {
        assert_eq!(summary(&[]), "");
    }

    #[test]
    fn the_roots_are_the_two_the_design_names() {
        let home = Path::new("/home/u/.deepnest");
        assert_eq!(user_root(home), Path::new("/home/u/.deepnest/skills"));
        let app = Path::new("/home/u/.deepnest/apps/notes");
        assert_eq!(
            app_root(app),
            Path::new("/home/u/.deepnest/apps/notes/.agent/skills")
        );
    }
}
