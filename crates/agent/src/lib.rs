//! The app agent: one conversation, one policy, one loop.
//!
//! Four pieces, and each exists because of a failure that is invisible from the outside:
//!
//! - **[`turn::Agent`]** — the loop. It asks the model, runs what it asked for, feeds the results
//!   back, and stops. Three of its rules are load-bearing: the loop **checkpoints cancellation**
//!   before every request and before every tool call (a cancelled turn that keeps looping holds
//!   the round it was cancelled to end); it treats a **refusal** as an answer, never as an
//!   over-long request (a provider's content filter also arrives as a 400, so shrinking and
//!   retrying turns a refusal into an endless shrink loop); and it bounds the history **by
//!   characters, not by entries** (four entries can be four sentences or four megabytes).
//! - **[`session`]** — the conversation, appended to `history.jsonl` one record per line. Append
//!   rather than rewrite: a rewritten file that fails halfway loses the conversation, and the
//!   conversation is the only thing the user cannot regenerate.
//! - **[`memory`]** — what the agent keeps beyond the conversation: frontmatter-plus-Markdown
//!   entries under `memory/`, with `memory/MEMORY.md` as a one-line-per-entry index. Only the
//!   index enters the prompt; a body is read with the `read` tool when the agent decides it
//!   needs it. Retirement is a `status`, never a delete.
//! - **[`prompt`]** — the system prompt, written from scratch. It is where most of the agent's
//!   behaviour lives, so it is code, versioned with the code that obeys it.
//!
//! Everything lives under the app's own directory (`~/.deepnest/apps/<slug>/.agent/`), so
//! deleting an app deletes its agent, and backing an app up backs its agent up.
//!
//! The model is reached through [`turn::Model`] rather than through a concrete client, so the
//! loop is testable with no network and no HTTP adapter — the adapter itself belongs to
//! `crates/gateway`.

pub mod memory;
pub mod prompt;
pub mod runner;
pub mod session;
pub mod turn;

use std::path::{Path, PathBuf};

pub use memory::{MemoryEntry, MemoryStore};
pub use prompt::PromptContext;
pub use runner::LoopRunner;
pub use session::{Record, Session};
pub use turn::{Agent, AgentError, Cancellation, Limits, Model, TurnOutcome};

/// Where one app's agent keeps its state.
///
/// One type rather than three `join` calls at each site: the layout is a contract (a user's
/// backup, and the `read` tool's reach, both depend on it), so it is stated once and the rest of
/// the crate asks this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    root: PathBuf,
}

impl AgentPaths {
    /// The agent directory of an app: `<app_dir>/.agent`.
    pub fn for_app(app_dir: impl AsRef<Path>) -> Self {
        Self {
            root: app_dir.as_ref().join(".agent"),
        }
    }

    /// From the agent directory itself, when the caller already has it.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The conversation, one JSON record per line.
    pub fn history(&self) -> PathBuf {
        self.root.join("history.jsonl")
    }

    /// The directory holding memory entries and their index.
    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    /// The memory index — one line per entry, and the only part that enters a prompt.
    pub fn memory_index(&self) -> PathBuf {
        self.memory_dir().join("MEMORY.md")
    }

    /// Where a subagent keeps its own conversation, one directory per call.
    pub fn subagent_dir(&self, serial: u64) -> PathBuf {
        self.root.join("subagents").join(format!("{serial:04}"))
    }

    /// Create the directories this layout names. Existing ones are left alone, so this is safe to
    /// call on every start.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.memory_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_sits_under_the_app_directory() {
        let paths = AgentPaths::for_app("/apps/notes");
        assert_eq!(paths.root(), Path::new("/apps/notes/.agent"));
        assert_eq!(
            paths.history(),
            Path::new("/apps/notes/.agent/history.jsonl")
        );
        assert_eq!(
            paths.memory_index(),
            Path::new("/apps/notes/.agent/memory/MEMORY.md")
        );
        assert!(paths.subagent_dir(3).starts_with(paths.root()));
    }

    #[test]
    fn ensure_creates_the_memory_directory_and_is_idempotent() {
        let base =
            std::env::temp_dir().join(format!("deepnest-agent-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = AgentPaths::for_app(&base);
        paths.ensure().unwrap();
        paths.ensure().unwrap();
        assert!(paths.memory_dir().is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }
}
