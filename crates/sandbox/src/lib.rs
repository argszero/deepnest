//! Which file effects one call is allowed, and where that answer comes from.
//!
//! The rule this module exists for: **the writable set has one home**
//! ([`roots::writable_roots`]). The file tools' refusal path and the operating system's
//! profile are both derived from it, so "the write tool cannot write `/tmp` but `bash` can"
//! is not a bug waiting to be found — it cannot be expressed. Everything else here is
//! plumbing around that one derivation.
//!
//! The fence is **the operating system's**, not a path check inside the process
//! (`host 19:03`): a self-imposed check is advice to code that has already been talked out
//! of it. Seatbelt on macOS, `bwrap` on Linux, ACL plus a restricted token on Windows — and
//! all three are described from the first line, because a backend added later is a rewrite
//! rather than a variant (`host 20:56`).
//!
//! Deliberate limits, named rather than discovered later:
//!
//! - **The Windows enforcer is a plan, not a spawn.** [`backend::Confinement::Acl`] carries
//!   the ACEs and the restricted-token shape a launcher will apply; the launcher is the
//!   Windows cycle's work. It is modelled now because the shared code must not assume a
//!   launcher that only two platforms have.
//! - **A denial is a dialect, not a union.** Each backend names the text *it* produces when
//!   it refuses a write. A shared list would claim refusals a backend never emits.
//! - **An unavailable backend fails closed.** Where the launcher is missing, the call is
//!   refused rather than run unconfined — the profile is the confinement, so there is no
//!   confined-but-launcherless state to fall back to.
//!
//! ```
//! use deepnest_sandbox::{Mode, Policy};
//!
//! let policy = Policy::for_app(Mode::WorkspaceWrite, "/home/u/.deepnest", "notes", None);
//! assert!(policy.is_writable("/home/u/.deepnest/apps/notes/src/main.rs"));
//! // The token file is unwritable in every mode, whatever the roots say.
//! assert!(!policy.is_writable("/home/u/.deepnest/deepnest.token"));
//! ```

pub mod backend;
pub mod escalation;
pub mod mode;
pub mod policy;
pub mod roots;

pub use backend::{
    available_backend, AclEntry, AclPlan, Confinement, Invocation, Platform, SandboxBackend,
    SandboxError,
};
pub use escalation::{EscalationRefusal, Grant, RankedMode};
pub use mode::Mode;
pub use policy::{Policy, PolicyError};
pub use roots::{canonical_path, same_file, RootKind, WritableRoot};
