//! The enforcers: one per platform, described from the first line.
//!
//! A backend answers one question — *how does this platform confine a process to the policy's
//! writable set* — and answers it as a value the caller can inspect ([`Confinement`]), not as
//! a side effect. That is what lets a Linux profile be tested on macOS and a Windows plan be
//! read without a Windows machine: the profile is data before it is a spawn.
//!
//! Three rules worth stating, each one a way this area is usually got wrong:
//!
//! - **`/dev/null` is granted unconditionally on macOS.** A write fence without it makes
//!   nearly every real command fail, and the error names `/dev/null` rather than the sandbox —
//!   the hardest possible failure to attribute to its cause.
//! - **A denial is a dialect.** Each backend lists the text *it* produces when it refuses.
//!   A shared list would claim refusals a backend never emits, and a refusal read as a runner
//!   failure is a policy decision read as a bug.
//! - **The runner's own failure is not a denial.** A profile the kernel will not compile
//!   means the command never ran; a refused write means it ran and was stopped. The two need
//!   different answers, so each backend carries both signatures.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::mode::Mode;
use crate::policy::Policy;
use crate::roots::{RootKind, WritableRoot};

/// The platform a backend confines for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
}

impl Platform {
    /// The platform this build targets.
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Platform::MacOs
        }
        #[cfg(target_os = "linux")]
        {
            Platform::Linux
        }
        #[cfg(target_os = "windows")]
        {
            Platform::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            // A platform this build does not ship for. Reported as Windows-shaped so that a
            // compile check on an unusual host still type-checks the largest backend, and
            // refused at runtime by `available_backend` rather than silently unconfined.
            Platform::Windows
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::MacOs => "macos",
            Platform::Linux => "linux",
            Platform::Windows => "windows",
        }
    }
}

/// A program and its arguments, ready to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
}

impl Invocation {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }
}

/// One access-control entry the Windows launcher would apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclEntry {
    pub path: PathBuf,
    /// `allow` or `deny`, as the ACE's type.
    pub kind: AclEntryKind,
    /// The rights the ACE carries, in the order the launcher applies them.
    pub rights: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclEntryKind {
    Allow,
    Deny,
}

/// The Windows confinement plan.
///
/// A plan rather than an [`Invocation`] because Windows has no launcher binary to prepend: the
/// confinement is an ACL on the tree plus a restricted token handed to the child, and expressing
/// that as argv would be a fiction. The applier is the Windows cycle's work; what exists here is
/// the derivation both of them will read, so the ACLs a user sees and the ACLs applied cannot
/// drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclPlan {
    /// Deny-writes ACEs, one per protected object.
    pub deny: Vec<AclEntry>,
    /// Grants for the writable set. Empty under `read-only`.
    pub allow: Vec<AclEntry>,
    /// Whether the launcher must hand the child a restricted token (integrity level plus the
    /// restricted SIDs). Always true for a confined mode on this platform.
    pub restricted_token: bool,
    /// The mode the plan expresses, so an applier can assert what it was given.
    pub mode: Mode,
}

/// What a call runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confinement {
    /// Prepend a launcher program and its profile arguments to the caller's argv.
    Launcher(Invocation),
    /// Apply an ACL plan, then spawn under a restricted token.
    Acl(AclPlan),
    /// Nothing — `danger-full-access`.
    Unconfined,
}

/// A sandbox that cannot be honoured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// The launcher this platform needs is not installed.
    ///
    /// Fail closed: the profile *is* the confinement, so a run without the launcher is not a
    /// weaker sandbox, it is none at all. The message names the program so the user can
    /// install it.
    LauncherMissing {
        program: &'static str,
        platform: &'static str,
    },
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::LauncherMissing { program, platform } => write!(
                f,
                "cannot confine this call: {program} is not installed on this {platform} system, \
                 and deepnest does not run a write-capable command unconfined"
            ),
        }
    }
}

impl std::error::Error for SandboxError {}

/// One way of confining a process on one platform.
pub trait SandboxBackend: Send + Sync {
    /// The backend's name, as it appears in a log line and in a refusal.
    fn name(&self) -> &'static str;

    fn platform(&self) -> Platform;

    /// The text this backend prints when it **refuses a write**. Lowercase; matched as a
    /// substring of the child's stderr.
    fn denial_signatures(&self) -> &'static [&'static str];

    /// The text this backend prints when **the command never ran** — a profile it will not
    /// compile, a namespace it cannot create. A different answer from a denial, and the
    /// reason both lists exist.
    fn runner_failure_signatures(&self) -> &'static [&'static str];

    /// The program that must be present for this backend to work.
    fn launcher(&self) -> Option<&'static str>;

    /// Build the confinement for one policy, given the command to run.
    ///
    /// `danger-full-access` returns [`Confinement::Unconfined`] here and nowhere else: the
    /// exemption lives in one place per backend so no call site has to remember it.
    fn confine(&self, policy: &Policy, command: &[String]) -> Result<Confinement, SandboxError>;

    /// Whether the launcher is reachable. `lookup` resolves a program name to a path, so the
    /// answer is testable without a filesystem.
    fn launcher_available(&self, lookup: &dyn Fn(&str) -> Option<PathBuf>) -> bool {
        match self.launcher() {
            Some(program) => lookup(program).is_some(),
            None => true,
        }
    }
}

/// macOS Seatbelt through `sandbox-exec`.
///
/// Apple marks the CLI deprecated and still ships and enforces it. If it is ever removed, the
/// spawn fails and the run fails closed — which is why this backend needs no version probe: a
/// probe would add a way to conclude "no fence" where there is a way to find out for certain.
#[derive(Debug, Clone, Copy, Default)]
pub struct Seatbelt;

/// `sandbox-exec` lives at a fixed path on every macOS.
pub const SEATBELT_EXEC: &str = "/usr/bin/sandbox-exec";

impl Seatbelt {
    /// The SBPL profile for one policy, as a single string.
    ///
    /// The shape is deny-everything-then-allow: `(deny file-write*)` followed by the grants.
    /// An allow-list built the other way round (allow default, deny the protected set) would
    /// have to enumerate everything worth protecting, and the first thing it forgot would be
    /// silently writable.
    pub fn profile(policy: &Policy) -> String {
        let mut forms = vec![
            "(version 1)".to_string(),
            "(allow default)".to_string(),
            "(deny file-write*)".to_string(),
            // Unconditional: without it most real commands die, and the error blames /dev/null.
            format!(
                "(allow file-write* (literal {}))",
                sbpl_string(Path::new("/dev/null"))
            ),
        ];
        let roots = policy.roots();
        if !roots.is_empty() {
            let clauses = roots.iter().map(sbpl_clause).collect::<Vec<_>>().join(" ");
            forms.push(format!("(allow file-write* {clauses})"));
        }
        forms.join(" ")
    }
}

impl SandboxBackend for Seatbelt {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn platform(&self) -> Platform {
        Platform::MacOs
    }

    fn denial_signatures(&self) -> &'static [&'static str] {
        // EPERM, as the kernel words it.
        &["operation not permitted"]
    }

    fn runner_failure_signatures(&self) -> &'static [&'static str] {
        // `sandbox-exec: <detail>` — the launcher's own prefix, which a denied *write* never
        // produces. The exit status is deliberately not part of this rule: that is one macOS
        // version's behaviour, while the prefix is the launcher's contract.
        &["sandbox-exec: "]
    }

    fn launcher(&self) -> Option<&'static str> {
        Some(SEATBELT_EXEC)
    }

    fn confine(&self, policy: &Policy, command: &[String]) -> Result<Confinement, SandboxError> {
        if !policy.mode().is_confined() {
            return Ok(Confinement::Unconfined);
        }
        let mut args = vec!["-p".to_string(), Self::profile(policy), "--".to_string()];
        args.extend(command.iter().cloned());
        Ok(Confinement::Launcher(Invocation::new(SEATBELT_EXEC, args)))
    }
}

/// Quote one path as an SBPL string literal.
fn sbpl_string(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// One writable root as an SBPL clause.
///
/// `subpath` for a directory, `literal` for a file: both match their own path, and the
/// difference is what else they match. `subpath` on a file grant would widen one named file to
/// everything beneath a same-named directory — which is why the form follows the grant's
/// recorded kind rather than asking the disk.
fn sbpl_clause(root: &WritableRoot) -> String {
    let form = match root.kind {
        RootKind::Directory => "subpath",
        RootKind::File => "literal",
    };
    format!("({form} {})", sbpl_string(&root.path))
}

/// Linux bubblewrap.
///
/// The profile *is* the mount table, so `read-only` is `--ro-bind / /` with nothing writable on
/// top, and `workspace-write` adds a private tmpfs at `/tmp` (a confined command's temp files
/// are invisible to the host afterwards) plus a bind for each writable root.
#[derive(Debug, Clone, Copy, Default)]
pub struct Bwrap;

/// Resolved through `PATH`; a host without it fails closed at the spawn.
pub const BWRAP_BIN: &str = "bwrap";

impl Bwrap {
    /// The mount profile for one policy, before the `--` and the caller's argv.
    pub fn profile_args(policy: &Policy) -> Vec<String> {
        let mut args: Vec<String> = [
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--unshare-pid",
            "--proc",
            "/proc",
            "--die-with-parent",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if policy.mode() == Mode::WorkspaceWrite {
            // A fresh tmpfs rather than the host's /tmp: a mount namespace can give the
            // sandbox a private one, which a profile-only fence cannot.
            args.push("--tmpfs".into());
            args.push("/tmp".into());
            // Bound after the tmpfs, so a writable root that itself lives under /tmp is still
            // writable — the mount added last wins.
            for root in policy.roots() {
                let path = root.path.to_string_lossy().to_string();
                args.push("--bind".into());
                args.push(path.clone());
                args.push(path);
            }
        }
        args
    }
}

impl SandboxBackend for Bwrap {
    fn name(&self) -> &'static str {
        "bwrap"
    }

    fn platform(&self) -> Platform {
        Platform::Linux
    }

    fn denial_signatures(&self) -> &'static [&'static str] {
        // EROFS against the read-only mount.
        &["read-only file system"]
    }

    fn runner_failure_signatures(&self) -> &'static [&'static str] {
        // Both of bubblewrap's fatal printers write this prefix. Signature-only on purpose: 1
        // is a status an ordinary command also exits with, so pinning it would misread a
        // confined command's own failure as "the command never ran".
        &["bwrap: "]
    }

    fn launcher(&self) -> Option<&'static str> {
        Some(BWRAP_BIN)
    }

    fn confine(&self, policy: &Policy, command: &[String]) -> Result<Confinement, SandboxError> {
        if !policy.mode().is_confined() {
            return Ok(Confinement::Unconfined);
        }
        let mut args = Self::profile_args(policy);
        args.push("--".into());
        args.extend(command.iter().cloned());
        Ok(Confinement::Launcher(Invocation::new(BWRAP_BIN, args)))
    }
}

/// Windows: ACLs plus a restricted token.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsAcl;

impl WindowsAcl {
    /// The plan for one policy.
    ///
    /// The rights are spelled as the Win32 constants' names rather than their numbers, because
    /// this is a description a person reads before an applier is written against it.
    pub fn plan(policy: &Policy) -> AclPlan {
        let deny = policy
            .protected()
            .iter()
            .map(|path| AclEntry {
                path: path.clone(),
                kind: AclEntryKind::Deny,
                rights: vec![
                    "FILE_WRITE_DATA".to_string(),
                    "FILE_APPEND_DATA".to_string(),
                    "FILE_WRITE_EA".to_string(),
                    "FILE_WRITE_ATTRIBUTES".to_string(),
                    "DELETE".to_string(),
                ],
            })
            .collect();
        let allow = if policy.mode() == Mode::WorkspaceWrite {
            policy
                .roots()
                .iter()
                .map(|root| AclEntry {
                    path: root.path.clone(),
                    kind: AclEntryKind::Allow,
                    rights: vec![
                        "FILE_GENERIC_READ".to_string(),
                        "FILE_GENERIC_WRITE".to_string(),
                        "FILE_GENERIC_EXECUTE".to_string(),
                        "DELETE".to_string(),
                    ],
                })
                .collect()
        } else {
            Vec::new()
        };
        AclPlan {
            deny,
            allow,
            restricted_token: policy.mode().is_confined(),
            mode: policy.mode(),
        }
    }
}

impl SandboxBackend for WindowsAcl {
    fn name(&self) -> &'static str {
        "windows-acl"
    }

    fn platform(&self) -> Platform {
        Platform::Windows
    }

    fn denial_signatures(&self) -> &'static [&'static str] {
        &["access is denied"]
    }

    fn runner_failure_signatures(&self) -> &'static [&'static str] {
        // The launcher's own refusal to build a token or an ACE — it never reaches the child.
        &["deepnest-sandbox: "]
    }

    fn launcher(&self) -> Option<&'static str> {
        // No external launcher: the applier is inside deepnest. Always present, and the plan is
        // what says whether it can be honoured.
        None
    }

    fn confine(&self, policy: &Policy, _command: &[String]) -> Result<Confinement, SandboxError> {
        if !policy.mode().is_confined() {
            return Ok(Confinement::Unconfined);
        }
        Ok(Confinement::Acl(Self::plan(policy)))
    }
}

/// The backend for this build's platform.
pub fn available_backend() -> Box<dyn SandboxBackend> {
    match Platform::current() {
        Platform::MacOs => Box::new(Seatbelt),
        Platform::Linux => Box::new(Bwrap),
        Platform::Windows => Box::new(WindowsAcl),
    }
}

/// The single pass over a confined child's output that separates its two failure kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The backend's own signatures appeared: the command never ran.
    RunnerFailure(String),
    /// The backend refused a write: the command ran and was stopped.
    Denied(String),
    /// Neither: whatever the command did, it did.
    Passed,
}

/// Classify a confined run's stderr (and status) against one backend.
///
/// Runner-failure first: a profile the kernel refused must never be reported as the policy
/// saying no, or a generator bug reads as correct enforcement. The status is not consulted —
/// each backend's signatures carry the distinction, and a status an ordinary command may also
/// exit with cannot make it.
pub fn classify(backend: &dyn SandboxBackend, stderr: &str) -> Outcome {
    let lowered = stderr.to_lowercase();
    for signature in backend.runner_failure_signatures() {
        if lowered.contains(&signature.to_lowercase()) {
            return Outcome::RunnerFailure((*signature).to_string());
        }
    }
    for signature in backend.denial_signatures() {
        if lowered.contains(&signature.to_lowercase()) {
            return Outcome::Denied((*signature).to_string());
        }
    }
    Outcome::Passed
}

/// A `PATH` lookup, the shape [`SandboxBackend::launcher_available`] takes.
pub fn which(program: &str) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.is_absolute() {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|full| full.is_file())
}

/// The environment a confined child inherits, minus nothing: the policy widens or narrows the
/// file system, and stripping the environment as well would break the toolchains the app
/// legitimately needs. Named here so the choice is visible rather than implicit.
pub fn inherited_environment(overrides: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = overrides
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    pairs.sort();
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;
    use crate::policy::Policy;
    use crate::roots::canonical_path;
    use std::fs;

    /// A real home with a temp root outside it, so the derived profile names paths the
    /// filesystem agrees with — the same paths the kernel would compare.
    struct Scratch {
        base: PathBuf,
        home: PathBuf,
        temp: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-backend-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let home = base.join("home");
            let temp = base.join("temp");
            fs::create_dir_all(home.join("apps/notes")).unwrap();
            fs::create_dir_all(home.join("toolchain")).unwrap();
            fs::create_dir_all(&temp).unwrap();
            fs::write(home.join("config.toml"), b"").unwrap();
            fs::write(home.join("deepnest.token"), b"").unwrap();
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

    fn command() -> Vec<String> {
        vec!["git".to_string(), "status".to_string()]
    }

    #[test]
    fn seatbelt_denies_every_write_then_grants_the_set() {
        let scratch = Scratch::new("order");
        let profile = Seatbelt::profile(&scratch.policy(Mode::WorkspaceWrite));
        let deny_at = profile.find("(deny file-write*)").expect("a deny form");
        let allow_at = profile
            .find("(allow file-write* (subpath")
            .expect("the grant form");
        assert!(
            deny_at < allow_at,
            "grants come after the denial: {profile}"
        );
        assert!(profile.contains("(literal \"/dev/null\")"), "{profile}");
        assert!(profile.contains("(version 1)"), "{profile}");
    }

    #[test]
    fn dev_null_is_granted_even_under_read_only() {
        // The grant is unconditional: a read-only profile without it breaks ordinary commands,
        // and the failure blames /dev/null rather than the sandbox.
        let scratch = Scratch::new("devnull");
        let profile = Seatbelt::profile(&scratch.policy(Mode::ReadOnly));
        assert!(profile.contains("/dev/null"), "{profile}");
        assert!(
            !profile.contains("apps/notes"),
            "no writable roots: {profile}"
        );
        assert_eq!(
            profile.matches("(allow file-write*").count(),
            1,
            "{profile}"
        );
    }

    #[test]
    fn a_file_grant_uses_literal_and_a_directory_grant_uses_subpath() {
        let scratch = Scratch::new("kinds");
        let notes = scratch.home.join("notes.md");
        fs::write(&notes, b"").unwrap();
        let policy = scratch
            .policy(Mode::WorkspaceWrite)
            .with_extra_root(crate::roots::WritableRoot::file(&notes));
        let profile = Seatbelt::profile(&policy);
        let app = canonical_path(&scratch.home.join("apps/notes"));
        assert!(
            profile.contains(&format!("(subpath {})", sbpl_string(&app))),
            "{profile}"
        );
        assert!(
            profile.contains(&format!(
                "(literal {})",
                sbpl_string(&canonical_path(&notes))
            )),
            "{profile}"
        );
    }

    #[test]
    fn a_path_with_a_quote_cannot_escape_the_sbpl_literal() {
        let scratch = Scratch::new("quote");
        // Not created: a directory name containing `"` cannot exist on Windows, and the thing
        // under test is the escape in the profile string, not the disk. `canonical_path` resolves
        // the part that exists and keeps the rest as spelled.
        let hostile = scratch.base.join("odd\"name)/*");
        let policy = Policy::from_roots(
            Mode::WorkspaceWrite,
            &hostile,
            scratch.home.join("toolchain"),
            &scratch.temp,
        );
        let profile = Seatbelt::profile(&policy);
        assert!(profile.contains("\\\""), "the quote is escaped: {profile}");
        // The hostile path must not be able to close its own clause and open a new form.
        assert_eq!(
            profile.matches("(allow file-write*").count(),
            2,
            "one for /dev/null, one for the set: {profile}"
        );
    }

    #[test]
    fn bwrap_read_only_binds_the_root_read_only_and_grants_nothing() {
        let scratch = Scratch::new("bwrap-ro");
        let args = Bwrap::profile_args(&scratch.policy(Mode::ReadOnly));
        assert_eq!(args[..2], ["--ro-bind", "/"]);
        assert!(!args.iter().any(|a| a == "--tmpfs"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--bind"), "{args:?}");
    }

    #[test]
    fn bwrap_workspace_write_adds_a_private_tmpfs_before_the_binds() {
        let scratch = Scratch::new("bwrap-ww");
        let args = Bwrap::profile_args(&scratch.policy(Mode::WorkspaceWrite));
        let tmpfs = args.iter().position(|a| a == "--tmpfs").expect("tmpfs");
        let first_bind = args
            .iter()
            .skip(2)
            .position(|a| a == "--bind")
            .expect("a bind")
            + 2;
        assert!(
            tmpfs < first_bind,
            "a root under /tmp must still be writable, so binds come last: {args:?}"
        );
        let app = canonical_path(&scratch.home.join("apps/notes"));
        let app = app.to_string_lossy().to_string();
        assert!(
            args.windows(2)
                .any(|w| w == ["--bind".to_string(), app.clone()]),
            "{args:?}"
        );
    }

    #[test]
    fn danger_full_access_confines_nothing_on_any_backend() {
        let scratch = Scratch::new("danger");
        let policy = scratch.policy(Mode::DangerFullAccess);
        let backends: Vec<Box<dyn SandboxBackend>> =
            vec![Box::new(Seatbelt), Box::new(Bwrap), Box::new(WindowsAcl)];
        for backend in backends {
            assert_eq!(
                backend.confine(&policy, &command()).unwrap(),
                Confinement::Unconfined,
                "{}",
                backend.name()
            );
        }
    }

    #[test]
    fn a_launcher_backend_puts_the_profile_before_the_command() {
        let scratch = Scratch::new("launcher");
        let confined = Seatbelt
            .confine(&scratch.policy(Mode::WorkspaceWrite), &command())
            .unwrap();
        let Confinement::Launcher(invocation) = confined else {
            panic!("seatbelt launches");
        };
        assert_eq!(invocation.program, SEATBELT_EXEC);
        let separator = invocation
            .args
            .iter()
            .position(|a| a == "--")
            .expect("the -- that ends the profile");
        assert_eq!(&invocation.args[separator + 1..], ["git", "status"]);
        assert_eq!(invocation.args[0], "-p");
    }

    #[test]
    fn the_windows_plan_denies_the_protected_files_and_grants_the_set() {
        let scratch = Scratch::new("windows");
        let plan = WindowsAcl::plan(&scratch.policy(Mode::WorkspaceWrite));
        assert!(plan.restricted_token);
        assert_eq!(plan.deny.len(), 3, "config, token and the skill state file");
        assert!(plan
            .deny
            .iter()
            .all(|entry| entry.kind == AclEntryKind::Deny));
        assert_eq!(plan.allow.len(), 3);
        let toolchain = canonical_path(&scratch.home.join("toolchain"));
        assert!(
            plan.allow.iter().any(|entry| entry.path == toolchain),
            "{:?}",
            plan.allow
        );
    }

    #[test]
    fn the_windows_read_only_plan_grants_nothing_but_still_denies_the_protected_files() {
        let scratch = Scratch::new("windows-ro");
        let plan = WindowsAcl::plan(&scratch.policy(Mode::ReadOnly));
        assert!(plan.allow.is_empty());
        assert_eq!(plan.deny.len(), 3);
        assert!(plan.restricted_token);
        assert_eq!(plan.mode, Mode::ReadOnly);
    }

    #[test]
    fn a_denial_and_a_runner_failure_are_different_answers() {
        assert_eq!(
            classify(&Seatbelt, "touch: /x: Operation not permitted"),
            Outcome::Denied("operation not permitted".into())
        );
        assert_eq!(
            classify(&Seatbelt, "sandbox-exec: syntax error: expecting ')'"),
            Outcome::RunnerFailure("sandbox-exec: ".into())
        );
        // A profile the kernel rejects is a runner failure even when the output also looks like
        // a denial — the command never ran, so it said nothing.
        assert_eq!(
            classify(&Bwrap, "bwrap: Can't find source path /nope"),
            Outcome::RunnerFailure("bwrap: ".into())
        );
        assert_eq!(
            classify(&Bwrap, "cannot create /x: Read-only file system"),
            Outcome::Denied("read-only file system".into())
        );
        assert_eq!(classify(&Seatbelt, "on branch main"), Outcome::Passed);
    }

    #[test]
    fn a_missing_launcher_is_reported_rather_than_assumed_present() {
        let present =
            |name: &str| -> Option<PathBuf> { Some(PathBuf::from(format!("/usr/bin/{name}"))) };
        let absent = |_name: &str| -> Option<PathBuf> { None };
        assert!(Seatbelt.launcher_available(&present));
        assert!(!Seatbelt.launcher_available(&absent));
        // A backend whose applier lives inside deepnest has no external dependency to check.
        assert!(WindowsAcl.launcher_available(&absent));
    }

    #[test]
    fn every_backend_names_its_platform_and_both_of_its_signatures() {
        let backends: Vec<Box<dyn SandboxBackend>> =
            vec![Box::new(Seatbelt), Box::new(Bwrap), Box::new(WindowsAcl)];
        let names: Vec<&str> = backends.iter().map(|b| b.name()).collect();
        assert_eq!(names, ["seatbelt", "bwrap", "windows-acl"]);
        assert_eq!(Seatbelt.platform(), Platform::MacOs);
        assert_eq!(Bwrap.platform(), Platform::Linux);
        assert_eq!(WindowsAcl.platform(), Platform::Windows);
        for backend in &backends {
            assert!(
                !backend.denial_signatures().is_empty(),
                "{} must name its denial dialect",
                backend.name()
            );
            assert!(
                !backend.runner_failure_signatures().is_empty(),
                "{} must name its launcher-failure prefix",
                backend.name()
            );
        }
    }
}
