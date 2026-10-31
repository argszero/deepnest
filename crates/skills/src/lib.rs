//! Anthropic Agent Skills, as deepnest reads them.
//!
//! A skill is a Markdown file with a header, holding instructions the agent can choose to follow.
//! This crate is the half that reads them: [`loader`] finds the files and [`frontmatter`] reads
//! their headers. The catalog that ships inside the binary, and the installer that fetches what the
//! catalog names, are the other half and are not written yet.
//!
//! The rule the whole crate is shaped by is **progressive disclosure**: only a skill's name, its
//! one-line description and the path to its file reach the prompt. The instructions are read by the
//! agent, with its own `read` tool, when it decides the skill is relevant — which is the difference
//! between a dozen skills costing a paragraph and a dozen skills crowding out the conversation.
//!
//! Two decisions worth knowing before reading the code:
//!
//! - **Anything that is not a usable skill is reported, not dropped.** A directory with no
//!   `SKILL.md`, a header without a `description`, a symlink: each is a line the user can act on
//!   rather than an absence they would have to guess at.
//! - **A skill is instructions, not a permission.** Nothing here is signed or trusted, because
//!   nothing here can widen the sandbox: whatever a skill says, the agent does through the same
//!   fence as always.

pub mod frontmatter;
pub mod loader;

pub use frontmatter::{Frontmatter, NoFrontmatter};
pub use loader::{
    app_root, load, summary, user_root, Loaded, Skill, Skipped, Source, APP_DIR, SKILL_FILE,
    USER_DIR,
};
