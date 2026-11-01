//! `install_skill` — one catalog entry, installed by the package that publishes it.
//!
//! The tool's **shape** belongs here, for the reason `subagent`'s does: everything interesting
//! about an install happens on the other side of seams this crate must not reach through.
//! `crates/skills` owns the catalog, the state file and the eight refusals an install can end in;
//! the program that publishes the file is run behind a `Publisher`, and whoever supplies that owes
//! it a deadline, because the real one waits as long as somebody else's program takes. Neither is a
//! dependency of this crate, and neither should be: a tool that imported `crates/skills` would
//! decide for every caller which catalog an install reads and which publisher runs it, and the
//! binary is the only crate that knows both.
//!
//! So the install is **injected** ([`SkillInstaller`]), exactly as the child loop is for
//! `subagent`. What this module supplies is the one thing a caller should not have to restate: what
//! an install call *is* — a name the catalog uses, and a refusal a model can act on rather than a
//! bare "no".
//!
//! **The file lands outside the app.** `~/.deepnest/skills/<name>/SKILL.md` is not one of the three
//! writable roots, so a call at `workspace-write` is refused, and the refusal names the mode to ask
//! for. The two argument keys that ask for it — `request_mode` and `justification` — are read and
//! removed by `crates/agent` before any tool is handed its arguments, so this module neither reads
//! them nor declares them: the system prompt tells a model they exist, once, for every tool. What
//! the description below does say is that *this* call needs one.

use std::sync::Arc;

use deepnest_llm::ToolDefinition;
use serde_json::{json, Value};

use crate::args::string_argument;
use crate::{Tool, ToolContext, ToolResult};

/// One install call, already read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInstallRequest {
    /// The name the catalog lists the skill under — which is also the name the published file
    /// must declare, and the key the state file records.
    pub name: String,
}

/// Installing a skill, as this crate needs to see it.
///
/// Synchronous like [`Tool::call`], and given the whole [`ToolContext`] because the decision this
/// tool exists to make is a *fence* decision: the install writes into a directory the session's mode
/// may not reach, so the implementation must ask `ctx.policy` before it runs anything. The
/// implementation lives in `crates/deepnest`; nothing in this crate may assume it exists.
pub trait SkillInstaller: Send + Sync {
    fn install(&self, request: &SkillInstallRequest, ctx: &ToolContext) -> ToolResult;
}

/// Install one optional skill the catalog names.
pub struct InstallSkillTool {
    installer: Arc<dyn SkillInstaller>,
}

impl InstallSkillTool {
    pub fn new(installer: Arc<dyn SkillInstaller>) -> Self {
        Self { installer }
    }
}

impl Tool for InstallSkillTool {
    fn name(&self) -> &'static str {
        "install_skill"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "install_skill",
            "Install one optional skill that deepnest's catalog names, by running that package's \
             own publish command. The skill is written into deepnest's skills directory — outside \
             this app — so when the session's mode does not reach that directory the call is \
             refused, and the same call has to be sent again with `request_mode` and \
             `justification`: the user is asked on the page, and the package runs only if they \
             approve. Installing is the user's decision to approve, not yours to take: ask for it \
             when the task needs a capability you do not have, and read the refusal rather than \
             retrying it unchanged. A skill installed this way is in your skill index from the next \
             turn on.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill's name exactly as the catalog lists it. A name the catalog does not know is refused, and the refusal lists the ones it does."
                    }
                },
                "required": ["name"]
            }),
        )
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let name = match string_argument(args, &["name"]) {
            Ok(name) => name,
            Err(reason) => return ToolResult::refused("install_skill", reason),
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            // An empty name is not a smaller install: it names nothing, and the catalog lookup
            // would be a search for the empty string rather than a refusal worth reading.
            return ToolResult::refused(
                "install_skill",
                "name is empty; it has to be the skill's name exactly as the catalog lists it",
            );
        }
        self.installer.install(&SkillInstallRequest { name }, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_sandbox::{Mode, Policy};
    use std::sync::Mutex;

    struct RecordingInstaller {
        seen: Mutex<Vec<SkillInstallRequest>>,
    }

    impl SkillInstaller for RecordingInstaller {
        fn install(&self, request: &SkillInstallRequest, _ctx: &ToolContext) -> ToolResult {
            self.seen.lock().unwrap().push(request.clone());
            ToolResult::ok("install_skill", "installed")
        }
    }

    fn ctx() -> ToolContext {
        let base =
            std::env::temp_dir().join(format!("deepnest-tools-skill-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        ToolContext::new(
            Policy::from_roots(
                Mode::WorkspaceWrite,
                base.clone(),
                base.clone(),
                base.clone(),
            ),
            base,
        )
    }

    fn tool() -> (InstallSkillTool, Arc<RecordingInstaller>) {
        let installer = Arc::new(RecordingInstaller {
            seen: Mutex::new(Vec::new()),
        });
        (InstallSkillTool::new(installer.clone()), installer)
    }

    #[test]
    fn a_call_is_handed_to_the_installer_by_name() {
        let (tool, installer) = tool();
        let result = tool.call(&json!({"name": "  browser-harness  "}), &ctx());
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.name, "install_skill");
        let seen = installer.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].name, "browser-harness",
            "the name is trimmed, and it is the catalog's spelling"
        );
    }

    #[test]
    fn a_call_with_no_name_is_refused_before_the_installer_is_reached() {
        let (tool, installer) = tool();
        let result = tool.call(&json!({}), &ctx());
        assert!(result.is_error);
        assert!(result.content.contains("name"), "{}", result.content);
        assert!(
            installer.seen.lock().unwrap().is_empty(),
            "the installer must not run"
        );
    }

    #[test]
    fn an_empty_name_is_refused_rather_than_looked_up() {
        let (tool, installer) = tool();
        let result = tool.call(&json!({"name": "   "}), &ctx());
        assert!(result.is_error);
        assert!(
            result.content.contains("exactly as the catalog lists it"),
            "{}",
            result.content
        );
        assert!(installer.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_name_that_is_not_a_string_is_refused_rather_than_read_as_one() {
        let (tool, installer) = tool();
        let result = tool.call(&json!({"name": 7}), &ctx());
        assert!(result.is_error);
        assert!(
            result.content.contains("must be a string"),
            "{}",
            result.content
        );
        assert!(installer.seen.lock().unwrap().is_empty());
    }
}
