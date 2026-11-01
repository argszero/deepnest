//! The tool set the model is offered, in one place.
//!
//! A registry rather than a list built at the call site, for one reason: the set the model is
//! *told about* and the set the dispatcher *can run* must be the same set. Two lists that agree
//! today is a feature that breaks silently the day one of them gains a member.
//!
//! Three entry points, and the difference between them is the whole reason `subagent` is a seam:
//! [`all`] is the six leaf tools, which need nothing but a [`crate::ToolContext`]; [`with_runner`]
//! adds `subagent` once the caller supplies the agent loop it delegates to; and
//! [`with_installer`] adds `install_skill` once the caller supplies the catalog it installs from.
//! A caller that has not built a loop — or found a home to read a catalog under — gets a set the
//! model is told about and that runs, not a set with a hole in it, which is what a `subagent` tool
//! with no runner or an `install_skill` with no catalog would be.
//!
//! The **browser** is not here at any level: browser driving and screenshots are a skill, not a
//! core tool (design §4). The core opens a page in the system browser and runs the skill's own
//! commands inside the fence.

use std::sync::Arc;

use deepnest_llm::ToolDefinition;

use crate::files::{EditTool, ReadTool, WriteTool};
use crate::search::{GlobTool, GrepTool};
use crate::shell::BashTool;
use crate::skill::{InstallSkillTool, SkillInstaller};
use crate::subagent::{SubagentRunner, SubagentTool};
use crate::Tool;

/// The leaf tools this crate provides, in the order the model is offered them.
///
/// `bash` first because it can do everything the others can, and `read` before `write` because a
/// model that writes before it reads is the failure `read`'s description is written against.
pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(BashTool::new()),
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(GlobTool),
        Box::new(GrepTool),
    ]
}

/// The same set with `subagent` added, once the caller can run a child agent.
pub fn with_runner(runner: Arc<dyn SubagentRunner>) -> Vec<Box<dyn Tool>> {
    let mut tools = all();
    tools.push(Box::new(SubagentTool::new(runner)));
    tools
}

/// The same set with `install_skill` added, once the caller can install from a catalog.
pub fn with_installer(installer: Arc<dyn SkillInstaller>) -> Vec<Box<dyn Tool>> {
    let mut tools = all();
    tools.push(Box::new(InstallSkillTool::new(installer)));
    tools
}

/// The definitions to send with a request, straight from [`all`].
pub fn definitions() -> Vec<ToolDefinition> {
    all().iter().map(|tool| tool.definition()).collect()
}

/// The definitions that include `subagent`.
pub fn definitions_with_runner(runner: Arc<dyn SubagentRunner>) -> Vec<ToolDefinition> {
    with_runner(runner)
        .iter()
        .map(|tool| tool.definition())
        .collect()
}

/// The tool with this name, if this set provides one.
///
/// `subagent` is only found with a runner: without one there is no child loop to hand the brief
/// to, and a tool that can be looked up but not run is the hole the two entry points exist to
/// avoid.
pub fn find(name: &str, runner: Option<Arc<dyn SubagentRunner>>) -> Option<Box<dyn Tool>> {
    let tools = match runner {
        Some(runner) => with_runner(runner),
        None => all(),
    };
    tools.into_iter().find(|tool| tool.name() == name)
}

/// The names this set provides, for a message that has to list them.
pub fn names(runner: Option<Arc<dyn SubagentRunner>>) -> Vec<&'static str> {
    match runner {
        Some(runner) => with_runner(runner).iter().map(|tool| tool.name()).collect(),
        None => all().iter().map(|tool| tool.name()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolContext, ToolResult};
    use deepnest_sandbox::{Mode, Policy};
    use std::collections::HashSet;

    struct StubRunner;

    impl SubagentRunner for StubRunner {
        fn run(&self, _request: &crate::SubagentRequest, _ctx: &ToolContext) -> ToolResult {
            ToolResult::ok("subagent", "stub")
        }
    }

    struct StubInstaller;

    impl SkillInstaller for StubInstaller {
        fn install(&self, _request: &crate::SkillInstallRequest, _ctx: &ToolContext) -> ToolResult {
            ToolResult::ok("install_skill", "stub")
        }
    }

    fn installer() -> Arc<dyn SkillInstaller> {
        Arc::new(StubInstaller)
    }

    fn runner() -> Option<Arc<dyn SubagentRunner>> {
        Some(Arc::new(StubRunner))
    }

    fn ctx() -> ToolContext {
        let base =
            std::env::temp_dir().join(format!("deepnest-tools-registry-{}", std::process::id()));
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

    #[test]
    fn every_tool_is_named_once_and_its_definition_agrees() {
        for tools in [
            all(),
            with_runner(Arc::new(StubRunner)),
            with_installer(installer()),
        ] {
            let mut seen = HashSet::new();
            for tool in &tools {
                let name = tool.name();
                assert!(seen.insert(name), "{name} is registered twice");
                let definition = tool.definition();
                assert_eq!(definition.function.name, name, "definition name drifted");
                assert!(
                    !definition.function.description.is_empty(),
                    "{name} has no description"
                );
                assert_eq!(
                    definition.function.parameters["type"], "object",
                    "{name} does not take an object"
                );
                assert!(
                    definition.function.parameters["properties"].is_object(),
                    "{name} declares no properties"
                );
            }
        }
    }

    #[test]
    fn the_set_is_the_seven_the_design_names() {
        // Pinned on purpose: adding a tool is a decision, and this test is where it is recorded.
        let names = names(runner());
        for expected in ["bash", "read", "write", "edit", "glob", "grep", "subagent"] {
            assert!(
                names.contains(&expected),
                "{expected} is missing from {names:?}"
            );
        }
        assert_eq!(names.len(), 7, "{names:?}");
    }

    #[test]
    fn subagent_is_absent_until_a_runner_is_supplied() {
        // The set offered and the set runnable are the same set: without a child loop there is
        // no `subagent` to offer.
        assert!(!names(None).contains(&"subagent"));
        assert!(find("subagent", None).is_none());
        assert!(find("subagent", runner()).is_some());
    }

    #[test]
    fn install_skill_is_absent_until_an_installer_is_supplied() {
        // The same rule for the second injected tool: an `install_skill` with no catalog behind it
        // would be a call the model is told about and that cannot run.
        assert!(!names(None).contains(&"install_skill"));
        let with = with_installer(installer());
        let named: Vec<&str> = with.iter().map(|tool| tool.name()).collect();
        assert!(named.contains(&"install_skill"), "{named:?}");
        assert_eq!(
            named.len(),
            7,
            "the six leaves and install_skill: {named:?}"
        );
        // And a loop-only caller does not get it, because this caller has no catalog.
        let loop_only: Vec<&str> = with_runner(Arc::new(StubRunner))
            .iter()
            .map(|tool| tool.name())
            .collect();
        assert!(!loop_only.contains(&"install_skill"), "{loop_only:?}");
    }

    #[test]
    fn find_returns_the_tool_of_that_name_and_nothing_for_an_unknown_name() {
        assert_eq!(find("read", None).map(|tool| tool.name()), Some("read"));
        assert!(find("browser", None).is_none());
    }

    #[test]
    fn the_definitions_match_the_registry_one_for_one() {
        let definitions = definitions();
        assert_eq!(definitions.len(), 6);
        for (definition, name) in definitions.iter().zip(names(None)) {
            assert_eq!(definition.function.name, name);
        }
        let with = definitions_with_runner(Arc::new(StubRunner));
        assert_eq!(with.len(), 7);
        assert_eq!(with[6].function.name, "subagent");
    }

    #[test]
    fn every_leaf_tool_can_actually_be_called() {
        // A registered tool that panics on an empty argument object is a tool the model will
        // find and use. Each must answer with a refusal instead.
        let ctx = ctx();
        for tool in all() {
            let result = tool.call(&serde_json::json!({}), &ctx);
            assert!(result.is_error, "{} accepted an empty call", tool.name());
            assert_eq!(result.name, tool.name());
            assert!(
                !result.content.is_empty(),
                "{} refused without a reason",
                tool.name()
            );
        }
    }
}
