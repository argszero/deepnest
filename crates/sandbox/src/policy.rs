//! One policy for one call, and the refusal paths that read it.

use std::path::{Path, PathBuf};

use crate::mode::Mode;
use crate::roots::{canonical_path, same_file, writable_roots, WritableRoot};

/// Why a policy could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    /// An app slug that is not a single, ordinary directory name.
    ///
    /// Refused rather than sanitised: a slug is a directory name under `~/.deepnest/apps/`,
    /// and `../..` is the shortest path from one app's data to another's. Sanitising would
    /// silently rename an app the caller asked for; refusing tells them.
    InvalidSlug(String),
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::InvalidSlug(slug) => write!(
                f,
                "invalid app name {slug:?}: expected one directory name, no separators and no leading dot"
            ),
        }
    }
}

impl std::error::Error for PolicyError {}

/// The complete file-effect policy for one call.
///
/// Immutable by construction: a session's default mode cannot move because this type is
/// carried by value into the tool that resolved it. Escalation produces a *new* policy for
/// *one* call ([`crate::escalation`]) and leaves this one alone — which is what "the session
/// default never moves" means once it is a type rather than a promise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    mode: Mode,
    app_root: PathBuf,
    toolchain_root: PathBuf,
    temp_root: PathBuf,
    extra_roots: Vec<WritableRoot>,
    protected: Vec<PathBuf>,
}

impl Policy {
    /// The policy for one app under `deepnest_home` (`~/.deepnest`).
    ///
    /// The writable set is derivable from these arguments and nothing else, which is why they
    /// are the arguments: the app directory, the toolchain directory, the temp directory.
    pub fn for_app(
        mode: Mode,
        deepnest_home: impl AsRef<Path>,
        slug: &str,
        temp_root: Option<&Path>,
    ) -> Self {
        let home = deepnest_home.as_ref();
        let temp = match temp_root {
            Some(path) => path.to_path_buf(),
            None => std::env::temp_dir(),
        };
        Self {
            mode,
            app_root: home.join("apps").join(slug),
            toolchain_root: home.join("toolchain"),
            temp_root: temp,
            extra_roots: Vec::new(),
            // The three files no mode may write (`docs/design.md` §3): the model endpoint and
            // its key, the control-panel token, and the record of which skills are managed.
            // Writing any of them is a way out of the fence, a way onto the control panel, or
            // a way to make an installed skill un-updatable.
            protected: vec![
                home.join("config.toml"),
                home.join("deepnest.token"),
                home.join("skills").join(".state.json"),
            ],
        }
    }

    /// Build from explicit roots — the form tests and the app-lifecycle code use.
    pub fn from_roots(
        mode: Mode,
        app_root: impl Into<PathBuf>,
        toolchain_root: impl Into<PathBuf>,
        temp_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            mode,
            app_root: app_root.into(),
            toolchain_root: toolchain_root.into(),
            temp_root: temp_root.into(),
            extra_roots: Vec::new(),
            protected: Vec::new(),
        }
    }

    /// Refuse a slug that is not one directory name.
    pub fn validate_slug(slug: &str) -> Result<(), PolicyError> {
        let invalid = slug.is_empty()
            || slug.starts_with('.')
            || slug.contains('/')
            || slug.contains('\\')
            || slug.contains('\0')
            || slug.contains("..")
            || slug.chars().any(|c| c.is_control());
        if invalid {
            Err(PolicyError::InvalidSlug(slug.to_string()))
        } else {
            Ok(())
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn app_root(&self) -> &Path {
        &self.app_root
    }

    pub fn toolchain_root(&self) -> &Path {
        &self.toolchain_root
    }

    pub fn temp_root(&self) -> &Path {
        &self.temp_root
    }

    pub fn extra_roots(&self) -> &[WritableRoot] {
        &self.extra_roots
    }

    pub fn protected(&self) -> &[PathBuf] {
        &self.protected
    }

    /// Add a root the host named for this session.
    pub fn with_extra_root(mut self, root: WritableRoot) -> Self {
        self.extra_roots.push(root);
        self
    }

    /// Add a protected path — for the tests that hold the rule, and for the packaging cycle
    /// when the toolchain gains files with the same standing.
    pub fn with_protected(mut self, path: impl Into<PathBuf>) -> Self {
        self.protected.push(path.into());
        self
    }

    /// The same policy at a different mode. The one legitimate way a mode changes: the
    /// caller builds a new value for one call.
    pub fn with_mode(&self, mode: Mode) -> Self {
        Self {
            mode,
            ..self.clone()
        }
    }

    /// The roots this policy grants, canonical.
    pub fn roots(&self) -> Vec<WritableRoot> {
        writable_roots(self)
    }

    /// Whether `path` names one of the protected files.
    ///
    /// Identity first (`same_file`: device and inode on unix, volume serial and file index on
    /// Windows, so a hard link or a symlink is caught),
    /// spelling second — because a protected file that does not exist yet has no identity, and
    /// a mode that could write it once it appears is not a protected file at all.
    pub fn is_protected(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        let canonical = canonical_path(path);
        self.protected.iter().any(|guard| {
            matches!(same_file(&canonical, guard), Some(true)) || canonical == canonical_path(guard)
        })
    }

    /// Whether a write to `path` is allowed. **The one answer the file tools and the profile
    /// both consume.**
    ///
    /// - `danger-full-access` → yes, except the protected files, which no mode writes.
    /// - `read-only` → no, always.
    /// - `workspace-write` → only inside a granted root, and never a protected file.
    pub fn is_writable(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        if self.is_protected(path) {
            return false;
        }
        if self.mode == Mode::DangerFullAccess {
            return true;
        }
        if self.mode == Mode::ReadOnly {
            return false;
        }
        let target = canonical_path(path);
        self.roots().iter().any(|root| root.covers(&target))
    }

    /// Why a write was refused, in words the agent can act on — or `None` when it was not.
    ///
    /// One refusal must not read like another: "you are in read-only" and "that path is
    /// outside your app" need different responses from a model, and "that file is protected"
    /// needs a third. A single "denied" teaches nothing and gets retried unchanged.
    pub fn write_refusal(&self, path: impl AsRef<Path>) -> Option<String> {
        let path = path.as_ref();
        if self.is_protected(path) {
            return Some(format!(
                "{} is protected: it holds deepnest's own configuration or credentials, and no mode may write it",
                path.display()
            ));
        }
        if self.mode == Mode::ReadOnly {
            return Some(format!(
                "this call is read-only, so {} cannot be written",
                path.display()
            ));
        }
        if self.is_writable(path) {
            return None;
        }
        let roots = self.roots();
        let listed = roots
            .iter()
            .map(|root| root.path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "{} is outside the writable set ({}); move the file into one of those, or ask the host to widen the sandbox for one call",
            path.display(),
            if listed.is_empty() {
                "which is empty".to_string()
            } else {
                listed
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A real home with a temp root *outside* it: the temp directory is a writable root, so a
    /// home inside the temp dir would make its every path writable and prove nothing.
    struct Scratch {
        base: PathBuf,
        home: PathBuf,
        temp: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-policy-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let home = base.join("home");
            let temp = base.join("temp");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&temp).unwrap();
            Self { base, home, temp }
        }

        fn policy(&self, mode: Mode) -> Policy {
            Policy::for_app(mode, &self.home, "notes", Some(&self.temp))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn workspace_write_allows_the_three_roots_and_nothing_else() {
        let scratch = Scratch::new("ww");
        let policy = scratch.policy(Mode::WorkspaceWrite);
        assert!(policy.is_writable(scratch.home.join("apps/notes/src/app.rs")));
        assert!(policy.is_writable(scratch.home.join("toolchain/cargo/bin/cargo")));
        assert!(policy.is_writable(scratch.temp.join("scratch.txt")));
        // The home itself is not a root: its other directories are not writable.
        assert!(!policy.is_writable(scratch.home.join("skills/other/SKILL.md")));
        assert!(!policy.is_writable(scratch.home.join("apps/other/src/app.rs")));
    }

    #[test]
    fn a_detour_through_dotdot_does_not_escape_the_set() {
        let scratch = Scratch::new("detour");
        let policy = scratch.policy(Mode::WorkspaceWrite);
        // Canonicalised before it is compared: the path resolves outside the app directory, so
        // spelling it through `..` buys nothing.
        assert!(!policy.is_writable(scratch.home.join("apps/notes/../../skills/x")));
    }

    #[test]
    fn read_only_refuses_even_inside_the_app() {
        let scratch = Scratch::new("ro");
        let policy = scratch.policy(Mode::ReadOnly);
        assert!(!policy.is_writable(scratch.home.join("apps/notes/x")));
        assert!(policy
            .write_refusal(scratch.home.join("apps/notes/x"))
            .unwrap()
            .contains("read-only"));
    }

    #[test]
    fn danger_full_access_still_refuses_the_protected_files() {
        let scratch = Scratch::new("danger");
        fs::create_dir_all(scratch.home.join("skills")).unwrap();
        fs::write(scratch.home.join("deepnest.token"), b"tok").unwrap();
        let policy = scratch.policy(Mode::DangerFullAccess);
        assert!(policy.is_writable(scratch.home.join("anywhere/at/all")));
        assert!(!policy.is_writable(scratch.home.join("deepnest.token")));
        assert!(!policy.is_writable(scratch.home.join("config.toml")));
        assert!(!policy.is_writable(scratch.home.join("skills/.state.json")));
        assert!(policy
            .write_refusal(scratch.home.join("config.toml"))
            .unwrap()
            .contains("protected"));
    }

    #[test]
    fn a_protected_file_is_refused_through_another_name() {
        let scratch = Scratch::new("alias");
        let token = scratch.home.join("deepnest.token");
        fs::write(&token, b"tok").unwrap();
        let alias = scratch.home.join("innocent-looking.conf");
        // A hard link: the alias that needs no privilege on any platform and is the same file by
        // identity, which is the property under test. This test used `fs::copy` on non-unix,
        // which produces a *different* file — so it could not have detected a spelling-based
        // identity check, and did not: Windows compared canonical spellings until the CI matrix
        // ran this.
        fs::hard_link(&token, &alias).unwrap();

        let policy = scratch.policy(Mode::DangerFullAccess);
        assert!(!policy.is_writable(&alias), "spelling is not identity");
    }

    #[test]
    fn a_slug_cannot_walk_out_of_its_own_directory() {
        assert!(Policy::validate_slug("notes").is_ok());
        assert!(Policy::validate_slug("my-app_2").is_ok());
        for bad in ["../etc", "..", "", ".hidden", "a/b", "a\\b", "a\0b"] {
            assert!(
                Policy::validate_slug(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn a_refusal_says_which_rule_refused_it() {
        let scratch = Scratch::new("words");
        let policy = scratch.policy(Mode::WorkspaceWrite);
        let protected = policy
            .write_refusal(scratch.home.join("config.toml"))
            .unwrap();
        assert!(protected.contains("protected"), "{protected}");
        let elsewhere = policy.write_refusal("/somewhere/else.txt").unwrap();
        assert!(
            elsewhere.contains("outside the writable set"),
            "{elsewhere}"
        );
        assert!(
            elsewhere.contains("apps"),
            "the roots are named: {elsewhere}"
        );
        assert_eq!(
            policy.write_refusal(scratch.home.join("apps/notes/ok.txt")),
            None
        );
    }

    #[test]
    fn changing_the_mode_builds_a_new_policy_and_leaves_the_old_one_alone() {
        let scratch = Scratch::new("immutable");
        let session = scratch.policy(Mode::ReadOnly);
        let escalated = session.with_mode(Mode::WorkspaceWrite);
        assert_eq!(session.mode(), Mode::ReadOnly);
        assert_eq!(escalated.mode(), Mode::WorkspaceWrite);
        assert!(!session.is_writable(scratch.home.join("apps/notes/x")));
        assert!(escalated.is_writable(scratch.home.join("apps/notes/x")));
    }
}
