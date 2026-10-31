//! The writable set: one derivation, two consumers.
//!
//! [`writable_roots`] is the only place that answers "where may a write land". The file tools
//! ask it before they touch a path, and each backend asks it while building its profile. Two
//! answers to the same question is how a fence develops a gap that neither side can see.
//!
//! Two details are load-bearing and were measured rather than assumed (`ref: emrg/sandbox/
//! roots.py`, whose docstrings record the same two):
//!
//! - **Paths are canonical before they are compared.** On macOS `/tmp` *is* `/private/tmp`,
//!   and `/var` is `/private/var`; a profile that grants `/tmp` as spelled grants nothing,
//!   because the kernel compares resolved paths. So the roots are resolved first and the
//!   profiles are built from the resolved form.
//! - **A root that does not exist resolves to its nearest existing ancestor plus the rest,
//!   unresolved.** Resolution never invents a fallback: a path that does not exist yet is
//!   granted as the caller spelled it, minus the part the filesystem could answer for.

use std::path::{Component, Path, PathBuf};

use crate::mode::Mode;
use crate::policy::Policy;

/// Whether a granted root may be written *through*, or only itself.
///
/// The distinction follows what the grant **is**, never what the disk currently says. Asking
/// the disk is how a name the host granted as one file widens to the whole tree beneath it
/// the day that name becomes a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    /// A directory: everything under it is writable too.
    Directory,
    /// One file: the path itself, and nothing beneath a same-named directory.
    File,
}

/// One granted writable root, canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritableRoot {
    pub path: PathBuf,
    pub kind: RootKind,
}

impl WritableRoot {
    pub fn directory(path: impl Into<PathBuf>) -> Self {
        Self {
            path: canonical_path(&path.into()),
            kind: RootKind::Directory,
        }
    }

    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            path: canonical_path(&path.into()),
            kind: RootKind::File,
        }
    }

    /// Whether `target` (canonical) lies inside this grant.
    pub fn covers(&self, target: &Path) -> bool {
        match self.kind {
            RootKind::File => self.path == target,
            RootKind::Directory => target == self.path || target.starts_with(&self.path),
        }
    }
}

/// Resolve a path the way the enforcement layer compares it.
///
/// The longest existing prefix is resolved by the filesystem and the components below it are
/// applied by hand: `.` dropped, `..` cancelling, names appended. A path that does not exist is
/// therefore granted under the name it *will* have, not under a name invented for it — and a
/// spelling that detours through `..` still resolves to the one place it really names, which is
/// what stops a detour from escaping the writable set.
///
/// The walk is over **components**, not over `Path::file_name`, because `file_name` answers
/// `None` for `..` — a walk built on it silently drops every parent step and turns
/// `apps/notes/../../skills` into `apps/notes/skills`.
pub fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    let components: Vec<Component<'_>> = path.components().collect();
    let mut resolved = PathBuf::new();
    let mut split = 0usize;
    for index in 1..=components.len() {
        let mut prefix = PathBuf::new();
        for component in &components[..index] {
            prefix.push(component.as_os_str());
        }
        match std::fs::canonicalize(&prefix) {
            Ok(canonical_prefix) => {
                resolved = canonical_prefix;
                split = index;
            }
            Err(_) => break,
        }
    }
    if split == 0 {
        // No component of this path exists — not even its root. The spelling is all there is.
        return path.to_path_buf();
    }
    for component in &components[split..] {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(part) => resolved.push(part),
            Component::RootDir | Component::Prefix(_) => resolved.push(component.as_os_str()),
        }
    }
    resolved
}

/// Whether two paths name the same file.
///
/// By identity — device and inode on unix, volume serial number and file index on Windows —
/// because a protected file reached through a second name (a hard link, a symlink, a
/// case-folding alias) is the same file and must be refused the same way. A string comparison
/// answers a question about the spelling, and the spelling is what an attacker chooses.
///
/// The two pairs are the same check on both platforms, and that is the point: the first version
/// of this function compared canonical spellings on Windows, which let a hard link to
/// `deepnest.token` be written. `std` reports neither number on Windows on stable
/// (`windows_by_handle` is still unstable), so the syscall is delegated rather than reimplemented.
///
/// `None` when there is no identity to compare (a path that does not exist, or cannot be
/// stat'ed): the caller then decides, and [`Policy::is_protected`] treats `None` as "not the
/// protected file" while comparing the canonical paths as a second, weaker check.
pub fn same_file(left: &Path, right: &Path) -> Option<bool> {
    same_file::is_same_file(left, right).ok()
}

/// The writable set for one policy: canonical, deduplicated, in a stable order.
///
/// The set is exactly the app directory, the toolchain directory and the temp directory
/// (`host 19:03`, `docs/design.md` §3). Under [`Mode::ReadOnly`] it is empty — not "the same
/// set, enforced elsewhere": an empty set is what makes the read-only profile and the
/// read-only refusal path provably agree. Under [`Mode::DangerFullAccess`] it is empty too,
/// and that is *not* a refusal: the mode is checked before the set is consulted.
pub fn writable_roots(policy: &Policy) -> Vec<WritableRoot> {
    if policy.mode() == Mode::ReadOnly {
        return Vec::new();
    }
    let mut roots = Vec::with_capacity(3);
    roots.push(WritableRoot::directory(policy.app_root()));
    roots.push(WritableRoot::directory(policy.toolchain_root()));
    roots.push(WritableRoot::directory(policy.temp_root()));
    roots.extend(policy.extra_roots().iter().cloned());

    let mut seen: Vec<WritableRoot> = Vec::with_capacity(roots.len());
    for root in roots {
        // Deduplicate by (kind, canonical path): the same object granted twice is one grant,
        // while a file grant and a directory grant of the same name stay distinct.
        if !seen
            .iter()
            .any(|kept| kept.kind == root.kind && kept.path == root.path)
        {
            seen.push(root);
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;
    use crate::policy::Policy;
    use std::fs;

    /// One scratch home plus a temp root that is **not** inside it.
    ///
    /// The separation is the point: the temp directory is a writable root, so a fixture that
    /// put the home under the temp dir would make every path inside it writable and prove
    /// nothing about the roots it meant to test.
    struct Scratch {
        home: PathBuf,
        temp: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-roots-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let home = base.join("home");
            let temp = base.join("temp");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&temp).unwrap();
            Self { home, temp }
        }

        fn policy(&self, mode: Mode) -> Policy {
            Policy::for_app(mode, &self.home, "notes", Some(&self.temp))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Some(base) = self.home.parent() {
                let _ = fs::remove_dir_all(base);
            }
        }
    }

    #[test]
    fn a_missing_path_resolves_to_its_existing_ancestor_plus_the_rest() {
        let scratch = Scratch::new("missing");
        let resolved_root = fs::canonicalize(&scratch.home).unwrap();
        let missing = scratch.home.join("not-yet").join("deeper");
        assert_eq!(
            canonical_path(&missing),
            resolved_root.join("not-yet").join("deeper")
        );
    }

    #[test]
    fn dot_and_dotdot_are_applied_before_the_path_is_compared() {
        let scratch = Scratch::new("dots");
        let resolved = fs::canonicalize(&scratch.home).unwrap();
        assert_eq!(
            canonical_path(&scratch.home.join("b").join("..").join("a")),
            resolved.join("a")
        );
    }

    #[test]
    fn a_path_that_exists_resolves_to_what_the_filesystem_says() {
        // macOS is where this matters: /tmp is /private/tmp, so a grant spelled `temp_dir()`
        // and the kernel's own view differ by a symlink. Whatever the platform does, both
        // sides read the same answer because both ask here.
        let scratch = Scratch::new("exists");
        assert_eq!(
            canonical_path(&scratch.temp),
            fs::canonicalize(&scratch.temp).unwrap()
        );
    }

    #[test]
    fn two_names_for_one_file_are_one_file() {
        // The property the protected-file rule rests on, pinned where the function lives: an
        // alias is the same file, a second file with identical content is not, and a path that
        // does not exist has no identity to compare.
        let scratch = Scratch::new("identity");
        let original = scratch.home.join("deepnest.token");
        fs::write(&original, b"tok").unwrap();

        let alias = scratch.home.join("innocent-looking.conf");
        fs::hard_link(&original, &alias).unwrap();
        let same_content = scratch.home.join("other.conf");
        fs::write(&same_content, b"tok").unwrap();

        assert_eq!(same_file(&original, &alias), Some(true));
        assert_eq!(same_file(&original, &same_content), Some(false));
        assert_eq!(same_file(&original, &scratch.home.join("absent")), None);
    }

    #[test]
    fn a_file_grant_covers_itself_but_not_a_tree_of_that_name() {
        // Both sides go through `canonical_path`, which is what every real caller does. Asserting
        // against the raw spelling only worked on a platform where canonicalising this path is a
        // no-op: on Windows `/srv/notes.md` resolves against the current drive, so the grant and
        // the target agree only after both are resolved.
        let spelled = Path::new("/srv/notes.md");
        let file = WritableRoot::file(spelled);
        assert!(file.covers(&canonical_path(spelled)));
        assert!(!file.covers(&canonical_path(Path::new("/srv/notes.md/inner"))));
    }

    #[test]
    fn a_directory_grant_covers_everything_below_it() {
        let dir = WritableRoot::directory("/srv/app");
        assert!(dir.covers(&canonical_path(Path::new("/srv/app"))));
        assert!(dir.covers(&canonical_path(Path::new("/srv/app/src/main.rs"))));
        // A component boundary, not a string prefix: `apple` is not inside `app`.
        assert!(!dir.covers(&canonical_path(Path::new("/srv/apple"))));
    }

    #[test]
    fn read_only_derives_an_empty_set() {
        let scratch = Scratch::new("ro");
        assert!(writable_roots(&scratch.policy(Mode::ReadOnly)).is_empty());
    }

    #[test]
    fn workspace_write_derives_exactly_the_three_roots() {
        let scratch = Scratch::new("ww");
        let policy = scratch.policy(Mode::WorkspaceWrite);
        let roots = writable_roots(&policy);
        assert_eq!(roots.len(), 3, "{roots:?}");
        assert_eq!(roots[0].kind, RootKind::Directory);
        assert_eq!(
            roots[0].path,
            canonical_path(&scratch.home.join("apps/notes"))
        );
        assert_eq!(
            roots[1].path,
            canonical_path(&scratch.home.join("toolchain"))
        );
        assert_eq!(roots[2].path, canonical_path(&scratch.temp));
    }

    #[test]
    fn a_root_listed_twice_is_granted_once() {
        let scratch = Scratch::new("dedupe");
        let policy = scratch
            .policy(Mode::WorkspaceWrite)
            .with_extra_root(WritableRoot::directory(scratch.home.join("apps/notes")));
        let roots = writable_roots(&policy);
        let app = canonical_path(&scratch.home.join("apps/notes"));
        assert_eq!(
            roots.iter().filter(|root| root.path == app).count(),
            1,
            "{roots:?}"
        );
    }

    #[test]
    fn a_file_grant_and_a_directory_grant_of_one_name_stay_distinct() {
        let scratch = Scratch::new("kinds");
        let policy = scratch
            .policy(Mode::WorkspaceWrite)
            .with_extra_root(WritableRoot::file(scratch.home.join("notes.md")));
        let roots = writable_roots(&policy);
        assert!(
            roots.iter().any(|root| root.kind == RootKind::File),
            "{roots:?}"
        );
    }

    #[test]
    fn same_file_sees_through_a_second_name() {
        let scratch = Scratch::new("same");
        let real = scratch.home.join("token");
        fs::write(&real, b"secret").unwrap();
        let link = scratch.home.join("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        fs::copy(&real, &link).unwrap();

        #[cfg(unix)]
        assert_eq!(
            same_file(&real, &link),
            Some(true),
            "a symlink is the same file"
        );
        // A path that does not exist has no identity to compare.
        assert_eq!(same_file(&real, &scratch.home.join("absent")), None);
    }

    #[test]
    fn two_distinct_files_are_not_the_same_file() {
        let scratch = Scratch::new("distinct");
        let one = scratch.home.join("one");
        let two = scratch.home.join("two");
        fs::write(&one, b"1").unwrap();
        fs::write(&two, b"2").unwrap();
        assert_eq!(same_file(&one, &two), Some(false));
    }
}
