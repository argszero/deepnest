//! Anthropic Agent Skills, as deepnest reads them.
//!
//! A skill is a Markdown file with a header, holding instructions the agent can choose to follow.
//! This crate is the half that reads them and the half that knows which ones exist: [`loader`] finds
//! the files and [`frontmatter`] reads their headers, [`catalog`] holds the list of optional skills
//! that ship inside the binary, [`installer`] puts one of them in place, [`state`] records which
//! installed skills deepnest may refresh, and [`release`] compares an installed version against the
//! version the package last published. The check's request is a seam and the socket behind it is
//! still missing — this workspace has no TLS client — so what lands today is the decision and every
//! way of reading an answer; [`release`] says exactly where that line is.
//!
//! The rule the whole crate is shaped by is **progressive disclosure**: only a skill's name, its
//! one-line description and the path to its file reach the prompt. The instructions are read by the
//! agent, with its own `read` tool, when it decides the skill is relevant — which is the difference
//! between a dozen skills costing a paragraph and a dozen skills crowding out the conversation.
//!
//! Four decisions worth knowing before reading the code:
//!
//! - **Anything that is not a usable skill is reported, not dropped.** A directory with no
//!   `SKILL.md`, a header without a `description`, a symlink, a catalog entry that cannot be
//!   fetched whole: each is a line the user can act on rather than an absence they would have to
//!   guess at.
//! - **A skill is instructions, not a permission.** Nothing here is signed or trusted, because
//!   nothing here can widen the sandbox: whatever a skill says, the agent does through the same
//!   fence as always.
//! - **The catalog is an ordinary skill file**, not a second format with its own reader — the
//!   loader picks it up like any other, and its machine-readable part is a `skills:` block in the
//!   header the same parser already reads.
//! - **Only `managed: true` may be refreshed.** A skill the user put in place is theirs; that is
//!   enforced by handing an updater the managed list and nothing else, and a state file missing the
//!   flag reads as unmanaged.
//! - **An install never overwrites a file it did not write.** Provenance is the state file's
//!   `managed` flag, and a file already on disk that the state file does not account for is the
//!   user's, because doubt is not permission.

mod atomic;

pub mod catalog;
pub mod frontmatter;
pub mod installer;
pub mod loader;
pub mod release;
pub mod state;

pub use catalog::{Catalog, Entry, Rejected, BUNDLED, CATALOG_FILE};
pub use frontmatter::{Frontmatter, NoFrontmatter};
pub use installer::{
    install, resolve_dest, Outcome, Placed, Publisher, Refused, Report, StdPublisher, PUBLISH_ARG,
    SELF_PUBLISHING,
};
pub use loader::{
    app_root, load, summary, user_root, Loaded, Skill, Skipped, Source, APP_DIR, SKILL_FILE,
    USER_DIR,
};
pub use release::{check, Releases, Verdict, Version, Why, GITHUB_RELEASE};
pub use state::{Installed, State, STATE_FILE};
