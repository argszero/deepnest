//! `~/.deepnest/apps/<slug>/deepnest.toml` — what makes a directory an app.
//!
//! The alternative is to treat every directory under `apps/` as an app, and it fails in the case
//! that costs the most: a directory whose contents are broken. With no manifest there is nothing to
//! be broken — the directory is an app and its source happens to be missing — so the failure shows
//! up much later, as a build error about a file nobody was editing. With one, a directory is an app
//! because it says so, and a directory that says so badly can be reported as such.
//!
//! It also carries the template version the app was created from. There is nothing to migrate yet;
//! the number is recorded now because the only cheap moment to start recording it is before there
//! is anything to migrate from.
//!
//! Unknown keys are rejected rather than ignored, for the reason `config.toml` rejects them: a
//! `nmae = "Notes"` that is silently ignored reads as "no name", and the diagnosis is then far from
//! the typo.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file name inside an app directory.
pub const FILE_NAME: &str = "deepnest.toml";

/// The template version new apps are created from.
pub const CURRENT_TEMPLATE: u32 = 1;

/// A parsed `deepnest.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app: AppInfo,
    /// The agents this app runs, as the app declares them.
    ///
    /// Declared here rather than created over the API because a declaration is a fact about the
    /// app — what work it needs doing, with which tools — and a fact about the app belongs in the
    /// directory that is the app. An app that creates its agents at run time would have two
    /// sources of truth for them, and the one on disk would be the stale one.
    #[serde(default, rename = "agent", skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentSpec>,
}

/// One `[[agent]]` table: an agent this app wants, and what it needs to do the work.
///
/// Everything here is a **statement about the work**, not a permission. What an agent may touch is
/// decided by the fence when the turn runs: its working directory becomes a writable root, and the
/// app's own directory is one already. A `workdir` outside the app is allowed and is exactly as
/// wide as the path says — the person who wrote the manifest is the person whose machine it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    /// What this agent is called, and what the app names it by when it asks for a turn.
    ///
    /// Checked to be a slug: it appears in a URL and in a directory name, and a name with a `/` in
    /// it would be two of each.
    pub name: String,
    /// Where it works, relative to the app's directory or absolute. Absent is the app's directory.
    pub workdir: Option<String>,
    /// What it is for, in the app's own words, appended to the agent prompt.
    ///
    /// Not a replacement for the prompt: everything deepnest tells every agent — the fence, the
    /// tools, the memory, the skill rules — stays, and this is added after it. An app that replaced
    /// the prompt would be an app that could turn the fence off by saying so.
    pub prompt: Option<String>,
    /// Extra skill directories, relative to the app's directory or absolute.
    ///
    /// The app's own `skills/` and the user's are loaded anyway; these are for a skill that lives
    /// somewhere else on this machine.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Where this app serves its own tools, as a path on the app's own port.
    ///
    /// `"/mcp"` for the usual case. Absent means the app declares no tools, and the agent has the
    /// six leaves and neither seam — which is a working agent, just one without hands into the app.
    /// The port is not written here because it is allocated per start and is different every run;
    /// this is a path, and deepnest joins it to the port it knows.
    pub tools: Option<String>,
}

/// The `[app]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppInfo {
    /// The name shown to the user. Free text, unlike the slug: this one never becomes a path.
    pub name: String,
    /// The template version this app was created from.
    #[serde(default = "current_template")]
    pub template: u32,
    /// An optional line about what the app is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn current_template() -> u32 {
    CURRENT_TEMPLATE
}

impl Manifest {
    /// A manifest for a new app.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            app: AppInfo {
                name: name.into(),
                template: CURRENT_TEMPLATE,
                description: None,
            },
            agents: Vec::new(),
        }
    }

    /// One agent by name.
    pub fn agent(&self, name: &str) -> Option<&AgentSpec> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    /// The name to show, trimmed.
    ///
    /// The value is checked non-blank on load, so this never returns an empty string for a manifest
    /// that [`load`] accepted.
    pub fn display_name(&self) -> &str {
        self.app.name.trim()
    }

    /// Write the manifest into `dir`, creating the directory if it is absent.
    ///
    /// Written to a sibling temporary file and renamed, so a crash mid-write cannot leave a
    /// half-written manifest that reads as "not an app" — the same shape the token file uses, for
    /// the same reason.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let text = toml::to_string(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let staging = dir.join(format!("{FILE_NAME}.tmp"));
        fs::write(&staging, text)?;
        fs::rename(&staging, &path)
    }
}

/// Why a directory is not an app.
#[derive(Debug)]
pub enum ManifestError {
    /// The directory has no `deepnest.toml`.
    Missing { path: PathBuf },
    /// It has one and it could not be read.
    Unreadable { path: PathBuf, source: io::Error },
    /// It was read and is not valid TOML, or does not match the shape above.
    Unparseable { path: PathBuf, detail: String },
    /// It parsed, but a value cannot be used.
    Unusable {
        path: PathBuf,
        field: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path } => write!(f, "no {FILE_NAME} at {}", path.display()),
            Self::Unreadable { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            Self::Unparseable { path, detail } => write!(
                f,
                "{} is not a usable app manifest: {detail}",
                path.display()
            ),
            Self::Unusable {
                path,
                field,
                reason,
            } => write!(f, "{}: `{field}` {reason}", path.display()),
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Read the manifest of the app in `dir`.
pub fn load(dir: &Path) -> Result<Manifest, ManifestError> {
    let path = dir.join(FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ManifestError::Missing { path })
        }
        Err(source) => {
            return Err(ManifestError::Unreadable {
                path: path.clone(),
                source,
            })
        }
    };

    let manifest: Manifest = toml::from_str(&text).map_err(|error| ManifestError::Unparseable {
        path: path.clone(),
        detail: error.to_string(),
    })?;
    manifest.check(&path)?;
    Ok(manifest)
}

impl Manifest {
    /// The value-level checks TOML cannot express.
    fn check(&self, path: &Path) -> Result<(), ManifestError> {
        let unusable = |field: &'static str, reason: &'static str| ManifestError::Unusable {
            path: path.to_path_buf(),
            field,
            reason,
        };

        if self.app.name.trim().is_empty() {
            return Err(unusable(
                "app.name",
                "is empty — name the app so the panel can show something other than its slug",
            ));
        }
        for agent in &self.agents {
            if agent.name.trim().is_empty() {
                return Err(unusable(
                    "agent.name",
                    "is empty — name the agent so the app can ask for it",
                ));
            }
            if !agent
                .name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            {
                return Err(unusable(
                    "agent.name",
                    "must be lowercase letters, digits and hyphens — it is a URL segment and a                      directory name",
                ));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for agent in &self.agents {
            if !seen.insert(agent.name.as_str()) {
                return Err(unusable(
                    "agent.name",
                    "is used by two agents — an app asking for that name would get one of them,                      and which one is the order they happen to be in",
                ));
            }
        }
        if self.app.template == 0 {
            // Zero is not a version that was ever issued; a manifest claiming it was written by
            // something that did not know, and guessing which template it meant is how a migration
            // runs against the wrong shape.
            return Err(unusable(
                "app.template",
                "must be 1 or greater — 0 names no template that was ever issued",
            ));
        }
        Ok(())
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
            let base =
                std::env::temp_dir().join(format!("deepnest-app-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn app(&self) -> PathBuf {
            self.base.join("notes")
        }

        fn write(&self, text: &str) -> PathBuf {
            let dir = self.app();
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(FILE_NAME);
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_written_manifest_reads_back_unchanged() {
        let scratch = Scratch::new("round-trip");
        let manifest = Manifest::new("Notes");

        manifest.save(&scratch.app()).unwrap();
        let read = load(&scratch.app()).unwrap();

        assert_eq!(read, manifest);
        assert_eq!(read.display_name(), "Notes");
        assert_eq!(read.app.template, CURRENT_TEMPLATE);
    }

    #[test]
    fn a_directory_without_a_manifest_is_not_an_app_and_says_which_path() {
        let scratch = Scratch::new("missing");
        let error = load(&scratch.app()).unwrap_err();

        let message = error.to_string();
        assert!(matches!(error, ManifestError::Missing { .. }));
        assert!(message.contains(FILE_NAME), "{message}");
        assert!(
            message.contains(&scratch.app().display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn a_hand_written_manifest_is_accepted() {
        let scratch = Scratch::new("hand-written");
        scratch.write(
            r#"
[app]
name = "Notes"
template = 1
description = "A place to put things"
"#,
        );

        let manifest = load(&scratch.app()).unwrap();
        assert_eq!(manifest.display_name(), "Notes");
        assert_eq!(
            manifest.app.description.as_deref(),
            Some("A place to put things")
        );
    }

    #[test]
    fn a_manifest_without_a_template_names_the_current_one() {
        // The field is what a future migration reads; a manifest written before it existed must not
        // be read as template 0.
        let scratch = Scratch::new("template-default");
        scratch.write(
            r#"
[app]
name = "Notes"
"#,
        );

        assert_eq!(load(&scratch.app()).unwrap().app.template, CURRENT_TEMPLATE);
    }

    #[test]
    fn a_typo_in_a_key_name_is_reported_rather_than_read_as_absence() {
        let scratch = Scratch::new("typo");
        let path = scratch.write(
            r#"
[app]
nmae = "Notes"
"#,
        );

        let error = load(&scratch.app()).unwrap_err();
        let message = error.to_string();
        assert!(matches!(error, ManifestError::Unparseable { .. }));
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[test]
    fn agents_are_declared_in_the_manifest_and_read_back_by_name() {
        let text = r#"
[app]
name = "Notes"

[[agent]]
name = "triage"
prompt = "Decide whether a message needs handling."
tools = "/mcp"

[[agent]]
name = "writer"
workdir = "/tmp/drafts"
skills = ["../shared-skills"]
"#;
        let manifest: Manifest = toml::from_str(text).unwrap();
        assert_eq!(manifest.agents.len(), 2);
        let triage = manifest.agent("triage").unwrap();
        assert_eq!(triage.tools.as_deref(), Some("/mcp"));
        assert_eq!(
            triage.prompt.as_deref(),
            Some("Decide whether a message needs handling.")
        );
        // Absent is a real answer: an agent with no tools declared has the leaves and no hands
        // into the app, which is a working agent.
        let writer = manifest.agent("writer").unwrap();
        assert!(writer.tools.is_none());
        assert_eq!(writer.workdir.as_deref(), Some("/tmp/drafts"));
        assert_eq!(writer.skills, vec!["../shared-skills".to_string()]);
        manifest.check(Path::new("/tmp/x")).unwrap();
    }

    #[test]
    fn an_app_with_no_agents_says_so_rather_than_inventing_one() {
        let manifest: Manifest = toml::from_str("[app]\nname = \"Notes\"\n").unwrap();
        assert!(manifest.agents.is_empty());
        assert!(manifest.agent("anything").is_none());
    }

    #[test]
    fn an_agent_name_that_cannot_be_a_url_segment_is_refused() {
        let path = Path::new("/tmp/x");
        let mut manifest = Manifest::new("Notes");
        let one = |name: &str| AgentSpec {
            name: name.to_string(),
            workdir: None,
            prompt: None,
            skills: Vec::new(),
            tools: None,
        };
        manifest.agents = vec![one("Triage Bot")];
        let error = manifest.check(path).unwrap_err();
        assert!(error.to_string().contains("agent.name"), "{error}");

        // Two agents with one name answer the same request with two different answers.
        manifest.agents = vec![one("triage"), one("triage")];
        let error = manifest.check(path).unwrap_err();
        assert!(error.to_string().contains("two agents"), "{error}");
    }

    #[test]
    fn an_empty_name_is_refused_and_names_the_field() {
        let scratch = Scratch::new("empty-name");
        scratch.write(
            r#"
[app]
name = "   "
"#,
        );

        let error = load(&scratch.app()).unwrap_err();
        assert!(matches!(error, ManifestError::Unusable { .. }));
        assert!(error.to_string().contains("app.name"), "{error}");
    }

    #[test]
    fn a_missing_name_is_refused() {
        let scratch = Scratch::new("no-name");
        scratch.write(
            r#"
[app]
template = 1
"#,
        );

        assert!(load(&scratch.app()).is_err());
    }

    #[test]
    fn template_zero_names_no_template_and_is_refused() {
        let scratch = Scratch::new("template-zero");
        scratch.write(
            r#"
[app]
name = "Notes"
template = 0
"#,
        );

        let error = load(&scratch.app()).unwrap_err();
        assert!(error.to_string().contains("app.template"), "{error}");
    }

    #[test]
    fn an_unreadable_manifest_names_the_path() {
        // A directory where the file should be: it exists, so this is not `Missing`, and there is
        // nothing to parse, so it is not `Unparseable` either.
        let scratch = Scratch::new("unreadable");
        let dir = scratch.app();
        fs::create_dir_all(dir.join(FILE_NAME)).unwrap();

        let error = load(&dir).unwrap_err();
        assert!(matches!(error, ManifestError::Unreadable { .. }), "{error}");
        assert!(
            error.to_string().contains(&dir.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn saving_creates_the_directory_and_leaves_no_staging_file() {
        let scratch = Scratch::new("save-creates");
        let dir = scratch.app();

        Manifest::new("Notes").save(&dir).unwrap();

        assert!(dir.join(FILE_NAME).is_file());
        assert!(
            !dir.join(format!("{FILE_NAME}.tmp")).exists(),
            "the staging file must be renamed, not left behind"
        );
    }
}
