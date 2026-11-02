//! The tool set the model is offered, in one place.
//!
//! A registry rather than a list built at the call site, for one reason: the set the model is
//! *told about* and the set the dispatcher *can run* must be the same set. Two lists that agree
//! today is a feature that breaks silently the day one of them gains a member.
//!
//! One constructor, and the difference between its arguments is the whole reason `subagent`,
//! `install_skill` and `app` are seams: [`all`] is the six leaf tools, which need nothing but a
//! [`crate::ToolContext`], and [`with_seams`] adds `subagent` once the caller supplies the agent
//! loop it delegates to, `install_skill` once the caller supplies the catalog it installs from,
//! `app` once the caller supplies the supervisor that owns the app's process, or any of them. A
//! caller that has not built a loop — or found a home to read a catalog under, or a process table
//! to ask — gets a set the model is told about and that runs, not a set with a hole in it, which is
//! what a `subagent` tool with no runner would be.
//!
//! The prompt names its tools from the set it was handed ([`names`]), so the sentence in front of
//! the model and the schema sent beside it cannot drift apart: there is one list, and it is the
//! one the dispatcher runs.
//!
//! The **browser** is not here at any level: browser driving and screenshots are a skill, not a
//! core tool (design §4). The core opens a page in the system browser and runs the skill's own
//! commands inside the fence.

use std::sync::Arc;

use deepnest_llm::ToolDefinition;

use crate::app::{AppControl, AppTool};
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

/// The set a caller with these seams can run: the six leaves, plus each seam it can honour.
///
/// `None` is a real answer rather than a degraded one — a caller with no child loop has no
/// `subagent`, a caller with no catalog has no `install_skill`, a caller with no supervisor has no
/// `app` — and they are independent because they are wired by different things: the loop is
/// `crates/agent`'s, the catalog and the process table are `crates/deepnest`'s, so a crate that has
/// one and not the others is the ordinary case. Supplying all three is what the dialog does.
///
/// The order is the order the model is offered them, and it is the leaves first on purpose: a seam
/// is reached for when a leaf cannot do the job, and the leaves are what a model that is new to the
/// set should try first.
pub fn with_seams(
    runner: Option<Arc<dyn SubagentRunner>>,
    installer: Option<Arc<dyn SkillInstaller>>,
    app: Option<Arc<dyn AppControl>>,
) -> Vec<Box<dyn Tool>> {
    let mut tools = all();
    if let Some(runner) = runner {
        tools.push(Box::new(SubagentTool::new(runner)));
    }
    if let Some(installer) = installer {
        tools.push(Box::new(InstallSkillTool::new(installer)));
    }
    if let Some(app) = app {
        tools.push(Box::new(AppTool::new(app)));
    }
    tools
}

/// The same set with `subagent` added, once the caller can run a child agent.
pub fn with_runner(runner: Arc<dyn SubagentRunner>) -> Vec<Box<dyn Tool>> {
    with_seams(Some(runner), None, None)
}

/// The same set with `install_skill` added, once the caller can install from a catalog.
pub fn with_installer(installer: Arc<dyn SkillInstaller>) -> Vec<Box<dyn Tool>> {
    with_seams(None, Some(installer), None)
}

/// The same set with `app` added, once the caller can reach a supervisor.
pub fn with_app(app: Arc<dyn AppControl>) -> Vec<Box<dyn Tool>> {
    with_seams(None, None, Some(app))
}

/// The definitions to send with a request, for one built set.
pub fn definitions(tools: &[Box<dyn Tool>]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|tool| with_intent(tool.definition()))
        .collect()
}

/// How long the model's stated reason may be before it is no longer a line.
///
/// A sentence, not a paragraph: this is what a person watching the transcript reads in place of the
/// raw arguments, and the point of it is that it fits on one row.
pub const INTENT_CHARS: usize = 200;

/// The one argument every tool takes, added here rather than in the nine `definition()` bodies.
///
/// A call without a stated reason is a call a person can only read as its arguments — `rg -n notes
/// src/`, `{"path":"src/main.rs","line":12}` — and those say what was done without saying why, which
/// is the one thing the person watching actually wants. EMRG's own tools make this a required
/// argument, and the host reading this dialog asked for the same on 2026-11-02T21:14: a call row is
/// there to be read by its intent.
///
/// Injected in one place on purpose. Nine copies of a schema fragment are nine copies to keep in
/// step, and a tool added later would silently be the one tool that does not ask.
///
/// `required` keeps whatever the tool already demanded: the intent is an extra thing the model must
/// say, never a replacement for the argument the tool cannot run without.
fn with_intent(mut definition: ToolDefinition) -> ToolDefinition {
    let parameters = &mut definition.function.parameters;
    let Some(object) = parameters.as_object_mut() else {
        // A tool that declares something other than an object schema is not one this can edit; its
        // `definition()` is the place to say so, and the registry's own test pins the shape.
        return definition;
    };
    if let Some(properties) = object
        .get_mut("properties")
        .and_then(|value| value.as_object_mut())
    {
        properties.insert(
            "intent".to_string(),
            serde_json::json!({
                "type": "string",
                "description": format!(
                    "Why you are making this call and what it should reach, in one plain sentence \
                     — the person watching reads this in place of the raw arguments. At most {} \
                     characters.", INTENT_CHARS
                )
            }),
        );
    }
    let required = object
        .entry("required")
        .or_insert_with(|| serde_json::json!([]));
    if let Some(list) = required.as_array_mut() {
        if !list.iter().any(|value| value == "intent") {
            list.insert(0, serde_json::json!("intent"));
        }
    }
    definition
}

/// The tool with this name in one built set, if it has one.
///
/// Read out of the set the caller built rather than out of a set rebuilt from `subagent`'s runner
/// alone: a set that can be looked up and a set that runs have to be the same vector, or the
/// lookup is a second opinion about what the turn holds.
pub fn find<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> Option<&'a dyn Tool> {
    tools
        .iter()
        .find(|tool| tool.name() == name)
        .map(|tool| tool.as_ref())
}

/// The names of one built set, in the order it offers them.
///
/// This is what the system prompt's tool section is rendered from, which is why it takes the set
/// rather than naming one: a prompt that listed tools from anywhere else would be a second list to
/// fall behind, and the model would be told about a tool the dispatcher does not have.
pub fn names(tools: &[Box<dyn Tool>]) -> Vec<String> {
    tools.iter().map(|tool| tool.name().to_string()).collect()
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

    struct StubApp;

    impl AppControl for StubApp {
        fn act(&self, _request: &crate::AppRequest, _ctx: &ToolContext) -> ToolResult {
            ToolResult::ok("app", "stub")
        }
    }

    fn installer() -> Arc<dyn SkillInstaller> {
        Arc::new(StubInstaller)
    }

    fn app() -> Option<Arc<dyn AppControl>> {
        Some(Arc::new(StubApp))
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

    /// A tool call a person cannot read is a tool call they have to take on trust. The arguments
    /// say what was done — `rg -n notes src/` — and never why, so every tool asks for the reason as
    /// an argument of its own, and the transcript leads with it.
    ///
    /// The two things this pins are the two ways it can rot: a tool added later that does not ask
    /// (it goes through [`definitions`], so it cannot), and a tool whose own required argument got
    /// pushed out by the new one.
    #[test]
    fn every_tool_asks_for_the_reason_it_was_called() {
        for tools in [
            all(),
            with_runner(Arc::new(StubRunner)),
            with_installer(installer()),
            with_seams(runner(), Some(installer()), None),
        ] {
            for definition in definitions(&tools) {
                let name = &definition.function.name;
                let parameters = &definition.function.parameters;
                assert!(
                    parameters["properties"]["intent"].is_object(),
                    "{name} does not offer an intent argument"
                );
                assert_eq!(
                    parameters["properties"]["intent"]["type"], "string",
                    "{name}'s intent is not a sentence"
                );
                let required = parameters["required"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{name} requires nothing at all"));
                assert!(
                    required.contains(&serde_json::json!("intent")),
                    "{name} offers an intent and does not ask for it: {required:?}"
                );
                // The tool's own argument is still demanded: the reason is an extra thing the model
                // must say, never a replacement for the one the tool cannot run without.
                assert!(
                    required.len() >= 2,
                    "{name} asks for the intent and nothing else: {required:?}"
                );
            }
        }
    }

    #[test]
    fn a_tool_that_asks_for_nothing_more_still_asks_for_the_reason() {
        // `subagent` requires its prompt and nothing else; the intent must not replace it, and a
        // definition with no `required` list at all must still gain one.
        struct Bare;
        impl Tool for Bare {
            fn name(&self) -> &'static str {
                "bare"
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::function(
                    "bare",
                    "a tool that declares its parameters and no required list",
                    serde_json::json!({
                        "type": "object",
                        "properties": {"text": {"type": "string"}}
                    }),
                )
            }
            fn call(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
                ToolResult::ok("bare", "")
            }
        }
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Bare)];
        let definition = &definitions(&tools)[0];
        assert_eq!(
            definition.function.parameters["required"],
            serde_json::json!(["intent"]),
            "a definition with no required list does not gain one"
        );
    }

    #[test]
    fn every_tool_is_named_once_and_its_definition_agrees() {
        for tools in [
            all(),
            with_runner(Arc::new(StubRunner)),
            with_installer(installer()),
            with_seams(runner(), Some(installer()), None),
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
        let named = names(&with_runner(Arc::new(StubRunner)));
        for expected in ["bash", "read", "write", "edit", "glob", "grep", "subagent"] {
            assert!(
                named.iter().any(|name| name == expected),
                "{expected} is missing from {named:?}"
            );
        }
        assert_eq!(named.len(), 7, "{named:?}");
    }

    #[test]
    fn both_seams_compose_into_one_set_the_dialog_can_run() {
        // The dialog is the caller that has both: a child loop from `crates/agent` and a catalog
        // from `crates/deepnest`. Before this set existed it had to pick one, so `subagent` was
        // never offered in the page the whole product is built around.
        let named = names(&with_seams(runner(), Some(installer()), None));
        assert_eq!(
            named,
            vec![
                "bash",
                "read",
                "write",
                "edit",
                "glob",
                "grep",
                "subagent",
                "install_skill",
            ],
            "{named:?}"
        );
        // Either seam alone still yields the seven it yielded before, so nothing that supplied one
        // changed shape under it.
        assert_eq!(names(&with_seams(runner(), None, None)).len(), 7);
        assert_eq!(names(&with_seams(None, Some(installer()), None)).len(), 7);
        assert_eq!(names(&with_seams(None, None, None)), names(&all()));
    }

    #[test]
    fn all_three_seams_compose_into_the_set_the_dialog_runs() {
        // What the dialog builds: a child loop from `crates/agent`, a catalog and a process table
        // from `crates/deepnest`. `app` is last because the leaves are what a model new to this set
        // should reach for first, and a seam answers a question a leaf cannot.
        let named = names(&with_seams(runner(), Some(installer()), app()));
        assert_eq!(
            named,
            vec![
                "bash",
                "read",
                "write",
                "edit",
                "glob",
                "grep",
                "subagent",
                "install_skill",
                "app",
            ],
            "{named:?}"
        );
        // Each seam alone is still one tool, so a caller that supplies one is not silently handed
        // the others.
        assert_eq!(names(&with_app(Arc::new(StubApp))).len(), 7);
        assert!(names(&with_seams(runner(), Some(installer()), None))
            .iter()
            .all(|name| name != "app"));
    }

    #[test]
    fn app_is_absent_until_a_supervisor_is_supplied() {
        // The same rule as the other two seams: a caller with no process table gets no `app`, not
        // one that refuses when it is finally called.
        assert!(!names(&all()).iter().any(|name| name == "app"));
        assert!(find(&all(), "app").is_none());
        assert!(find(&with_app(Arc::new(StubApp)), "app").is_some());
    }

    #[test]
    fn subagent_is_absent_until_a_runner_is_supplied() {
        // The set offered and the set runnable are the same set: without a child loop there is
        // no `subagent` to offer.
        let leaves = all();
        assert!(!names(&leaves).iter().any(|name| name == "subagent"));
        assert!(find(&leaves, "subagent").is_none());
        assert!(find(&with_runner(Arc::new(StubRunner)), "subagent").is_some());
    }

    #[test]
    fn install_skill_is_absent_until_an_installer_is_supplied() {
        // The same rule for the second injected tool: an `install_skill` with no catalog behind it
        // would be a call the model is told about and that cannot run.
        assert!(!names(&all()).iter().any(|name| name == "install_skill"));
        let named = names(&with_installer(installer()));
        assert!(
            named.iter().any(|name| name == "install_skill"),
            "{named:?}"
        );
        assert_eq!(
            named.len(),
            7,
            "the six leaves and install_skill: {named:?}"
        );
        // And a loop-only caller does not get it, because this caller has no catalog.
        let loop_only = names(&with_runner(Arc::new(StubRunner)));
        assert!(
            !loop_only.iter().any(|name| name == "install_skill"),
            "{loop_only:?}"
        );
    }

    #[test]
    fn find_returns_the_tool_of_that_name_and_nothing_for_an_unknown_name() {
        let leaves = all();
        assert_eq!(find(&leaves, "read").map(|tool| tool.name()), Some("read"));
        assert!(find(&leaves, "browser").is_none());
    }

    #[test]
    fn the_definitions_match_the_registry_one_for_one() {
        let leaves = all();
        let leaf_definitions = definitions(&leaves);
        assert_eq!(leaf_definitions.len(), 6);
        for (definition, name) in leaf_definitions.iter().zip(names(&leaves)) {
            assert_eq!(definition.function.name, name);
        }
        let both = definitions(&with_seams(runner(), Some(installer()), None));
        assert_eq!(both.len(), 8);
        assert_eq!(both[6].function.name, "subagent");
        assert_eq!(both[7].function.name, "install_skill");
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
