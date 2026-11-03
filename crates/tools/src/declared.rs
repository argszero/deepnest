//! A tool an app declared, which this process runs by handing it back rather than calling it.
//!
//! The distinction from [`deepnest_tools::mcp`] is the whole of this module, and it is a fact about
//! **who is on the other end**:
//!
//! - An MCP server somewhere else — somebody's published server, a wrapper around a service — has
//!   no part in the request being answered. Nothing waits on it, so a call to it is work this turn
//!   does, and `McpTool` does exactly that.
//! - An app's own tool is the app. The app is inside a request of its own when it asked for this
//!   turn, and a program that answers one request at a time cannot answer a callback from within
//!   it. So the turn does not call: the batch leaves as
//!   [`deepnest_agent::TurnOutcome::Paused`], the app runs its own code, and the results come back
//!   in a later request. See that variant for the full account.
//!
//! Which is why this tool answers `runs_here` with `false` and its [`Tool::call`] is a refusal: the
//! only thing a synchronous call here could do is re-create the deadlock. A caller that reaches it
//! anyway is told what happened instead of being quietly wrong.
//!
//! The declaration crosses in MCP's own `tools/list` shape — `name`, `description`, `inputSchema` —
//! because that shape is already what [`ToolDefinition`] carries and translating it would be this
//! crate deciding what an app's parameters mean.

use deepnest_llm::ToolDefinition;
use serde_json::Value;

use crate::{Tool, ToolContext, ToolResult};

/// One tool an app declared, as the app wrote it.
#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    pub name: String,
    pub description: String,
    /// The `inputSchema`, verbatim.
    pub schema: Value,
}

/// The tools out of an MCP `tools/list` result — or out of a request body that carries the same
/// shape, which is how an app hands its declarations over without being called.
///
/// A declaration with no name is skipped rather than refused: one malformed entry must not hide
/// the rest, and a tool with no name cannot be called by anybody.
pub fn declarations(listed: &Value) -> Vec<Declaration> {
    let items = match listed {
        Value::Array(items) => items.as_slice(),
        Value::Object(object) => match object.get("tools") {
            Some(Value::Array(items)) => items.as_slice(),
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        out.push(Declaration {
            name: name.to_string(),
            description: item
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            schema: item
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
        });
    }
    out
}

/// One tool an app declared, as a hand of the agent's.
pub struct DeclaredTool {
    declaration: Declaration,
    app: String,
}

impl DeclaredTool {
    pub fn new(declaration: Declaration, app: impl Into<String>) -> Self {
        Self {
            declaration,
            app: app.into(),
        }
    }

    pub fn declaration(&self) -> &Declaration {
        &self.declaration
    }
}

impl Tool for DeclaredTool {
    fn name(&self) -> &str {
        &self.declaration.name
    }

    fn definition(&self) -> ToolDefinition {
        // The app's own words, plus the one sentence this crate adds. A model reading a remote tool
        // exactly like `bash` would reach for it expecting `bash`'s reach; what it can touch is
        // whatever the app it names can, and that is worth saying in the description rather than
        // leaving to be discovered.
        let description = if self.declaration.description.is_empty() {
            format!(
                "A tool provided by the app `{}`. It runs inside that app, not in this process, so \
                 what it can reach is whatever the app can.",
                self.app
            )
        } else {
            format!(
                "{}\n\nProvided by the app `{}`, and run inside it rather than in this process.",
                self.declaration.description, self.app
            )
        };
        ToolDefinition::function(
            self.name().to_string(),
            description,
            self.declaration.schema.clone(),
        )
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> ToolResult {
        // Not a refusal by a policy and worded so it cannot be read as one: nothing denied this
        // call, and the model looking for a permission it already has would spend a round finding
        // out it was wrong.
        ToolResult::error(
            self.name().to_string(),
            format!(
                "`{}` belongs to the app `{}` and is not run from inside a turn. The call is handed \
                 back to the app, which runs it and reports the result in a later request.",
                self.name(),
                self.app
            ),
        )
    }

    fn runs_here(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_sandbox::{Mode, Policy};

    fn ctx() -> ToolContext {
        ToolContext::new(
            Policy::for_app(Mode::WorkspaceWrite, "/home/u/.deepnest", "notes", None),
            "/home/u/.deepnest/apps/notes",
        )
    }

    #[test]
    fn the_apps_declaration_crosses_with_its_schema_untouched() {
        let listed = serde_json::json!({"tools": [
            {"name": "todos_list", "description": "List the open todos.",
             "inputSchema": {"type": "object", "properties": {"state": {"type": "string"}}}},
            {"description": "no name, so it cannot be called"}
        ]});
        let declarations = declarations(&listed);
        assert_eq!(declarations.len(), 1, "an unnamed tool is skipped");

        let tool = DeclaredTool::new(declarations.into_iter().next().unwrap(), "notes");
        assert_eq!(tool.name(), "todos_list");
        let definition = tool.definition();
        assert_eq!(
            definition.function.parameters["properties"]["state"]["type"],
            "string"
        );
        assert!(definition
            .function
            .description
            .contains("List the open todos"));
        // And the sentence that stops a remote tool being read as a local one.
        assert!(definition
            .function
            .description
            .contains("run inside it rather than in this process"));
    }

    #[test]
    fn an_array_without_the_envelope_is_read_the_same_way() {
        // An app posting its own declarations has no reason to wrap them in `{"tools": …}`; the
        // shape that matters is the entries.
        let listed = serde_json::json!([{"name": "done", "inputSchema": {"type": "object"}}]);
        assert_eq!(declarations(&listed).len(), 1);
    }

    #[test]
    fn a_declared_tool_says_it_does_not_run_here_and_its_call_is_not_a_refusal() {
        let tool = DeclaredTool::new(
            Declaration {
                name: "todos_list".to_string(),
                description: String::new(),
                schema: serde_json::json!({"type": "object"}),
            },
            "notes",
        );
        // The flag the turn reads: this call is handed back, not made.
        assert!(!tool.runs_here());
        let result = tool.call(&serde_json::json!({}), &ctx());
        assert!(result.is_error);
        // Not refused: nothing denied it, and a model told "refused" looks for a permission.
        assert!(!result.refused);
        assert!(result.content.contains("handed back"), "{}", result.content);
        // A declaration with no description still tells the model where it runs.
        assert!(tool
            .definition()
            .function
            .description
            .contains("runs inside that app"));
    }
}
