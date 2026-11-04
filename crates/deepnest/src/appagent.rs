//! The agents an app declares, and the loop its own code drives.
//!
//! Everything before this module is about **one** agent per app: the dialog's. The person types,
//! the agent writes the app, and the app is the thing being written. That is the product's first
//! half and it is finished.
//!
//! This is the second half. An app is not only written by an agent — it **uses** one. A Feishu
//! bridge reads messages all day and has to decide which of them need handling; a todo app that
//! has decided needs somebody to work out what the message actually asks for. Those are agent
//! turns, and they belong to the app, not to a person typing in a panel. The app's UI is one entry
//! point among several; its own timers and its own messages are the others.
//!
//! So an app declares its agents in `deepnest.toml` ([`deepnest_apps::AgentSpec`]) and calls them
//! over the API with the ticket it was started with. Three decisions are made here, and each is
//! stated where it is made:
//!
//! - **The turn is built with the handoff on.** The app is inside a request of its own when it asks
//!   for a turn, so a tool of the app's must not be called from inside that turn — see
//!   [`deepnest_agent::TurnOutcome::Paused`]. A round's remote calls come back to the app, which
//!   runs them as its own code and posts the answers to `agent-result`.
//! - **An agent's fence is its own, and it is not wider than the app's.** The working directory the
//!   manifest names becomes a writable root beside the app's; nothing else moves. What the app could
//!   already do with its own process is out of deepnest's hands either way, and
//!   [`deepnest_tools::McpTool`] says so in the description the model reads.
//! - **A declared name is the only name.** An app asks for the agent it declared; an undeclared
//!   name is answered with the list of what exists, before a turn is built or a model is called.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use deepnest_agent::{
    Agent, AgentError, AgentPaths, CallResult, Cancellation, MemoryStore, Model, PromptContext,
    Session, TurnOutcome,
};
use deepnest_apps::{manifest, Slug};
use deepnest_sandbox::{Mode, Policy, WritableRoot};
use deepnest_tools::registry;
use serde_json::{json, Value};

use crate::chat::Reply;
use crate::http::{Method, Request};
use crate::ticket;

/// The verb this module answers, under `/api/apps/<slug>/`.
///
/// Spelled here because two places need it: the router, which routes on it, and the page's grants,
/// which mint one cookie per route an app's own scripts may reach.
pub const ACTION: &str = "agent";

/// `POST /api/apps/<slug>/agent` — one turn of an agent the app declared.
///
/// The body names the agent and carries the message:
///
/// ```json
/// { "agent": "triage", "message": "…", "session": "optional-key" }
/// ```
///
/// `session` is optional and is the app's own name for a conversation. Absent means the agent's one
/// conversation (`<app>/.agent/agents/<name>/history.jsonl`); present means
/// `sessions/<key>.jsonl` beside it, so an app that handles a thousand messages can keep a thousand
/// conversations apart without this route inventing an identity for them.
///
/// Two answers are not errors and both are `200`:
///
/// - `{"outcome":"answered","text":"…"}` — the turn finished.
/// - `{"outcome":"paused","turn":"…","pending":[{"call_id":"…","name":"…","arguments":"{…}"}]}` —
///   the model asked for work that belongs to the app. The app runs each call **in its own process**,
///   then posts the results to `agent-result`. Nothing is waiting in this server between the two
///   requests; the transcript is the state.
pub fn route(
    request: &Request,
    model: Option<Arc<dyn Model>>,
    home: &Path,
    app_dir: &Path,
    slug: &Slug,
    cancel: &Cancellation,
    on_text: &mut dyn FnMut(&str),
) -> Reply {
    let text = match body_text(request) {
        Ok(text) => text,
        Err(reply) => return reply,
    };
    let body: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            return failed(
                400,
                format!(
                "the body is not JSON ({error}). Send {{\"agent\":\"<name>\",\"message\":\"…\"}}"
            ),
            )
        }
    };
    let Some(name) = body.get("agent").and_then(Value::as_str).map(str::trim) else {
        return failed(
            400,
            "the body names no agent: send {\"agent\":\"<name>\",\"message\":\"…\"}. The agents this \
             app declares are in its deepnest.toml, under [[agent]]."
                .to_string(),
        );
    };
    let Some(message) = body.get("message").and_then(Value::as_str) else {
        return failed(400, "the body carries no `message` string".to_string());
    };
    if message.trim().is_empty() {
        return failed(400, "the message is empty".to_string());
    }
    let session_key = body.get("session").and_then(Value::as_str);

    if let Err(reply) = declared(app_dir, slug, name) {
        return reply;
    }

    let Some(model) = model else {
        return failed(
            503,
            "this process has no model endpoint configured, so no turn can run — POST /api/config \
             with base_url, api_key and model first"
                .to_string(),
        );
    };

    // What the app offers as its own tools, in MCP's `tools/list` shape. Sent with the request
    // rather than asked for, because asking would be the callback this whole design avoids.
    let declarations = body.get("tools");
    let agent = match build(model, home, app_dir, slug, name, session_key, declarations) {
        Ok(agent) => agent,
        Err(reply) => return reply,
    };

    // Two things the app's own page asked for by sending this request: the turn is stoppable
    // because the router registered it before calling here, and the answer is handed over as it is
    // written because the caller passed an observer. An app that only wants the result passes a
    // flag nobody holds and an observer that discards — which is what the app's own process does.
    match agent.run_turn_watched(message, &[], cancel, on_text) {
        Ok(outcome) => reply(outcome),
        Err(error) => error_reply(&error),
    }
}

/// `POST /api/apps/<slug>/agent-result` — the answers to a handed-back batch.
///
/// ```json
/// { "agent": "triage", "session": "…", "results": [
///     { "call_id": "call_abc", "content": "3 open", "is_error": false } ] }
/// ```
///
/// Every call of the batch must be answered. A partial batch is refused before anything is written,
/// because a transcript whose assistant record has some answers and not others is one no later
/// request can be built from — see [`deepnest_agent::Session::owed`].
pub fn result_route(
    request: &Request,
    model: Option<Arc<dyn Model>>,
    home: &Path,
    app_dir: &Path,
    slug: &Slug,
    cancel: &Cancellation,
    on_text: &mut dyn FnMut(&str),
) -> Reply {
    let text = match body_text(request) {
        Ok(text) => text,
        Err(reply) => return reply,
    };
    let body: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            return failed(
                400,
                format!("the body is not JSON ({error}). Send {{\"agent\":\"…\",\"results\":[…]}}"),
            )
        }
    };
    let Some(name) = body.get("agent").and_then(Value::as_str).map(str::trim) else {
        return failed(400, "the body names no agent".to_string());
    };
    let session_key = body.get("session").and_then(Value::as_str);
    let results: Vec<CallResult> = match body.get("results") {
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match serde_json::from_value::<CallResult>(item.clone()) {
                    Ok(result) => out.push(result),
                    Err(error) => {
                        return failed(
                            400,
                            format!(
                                "a result is not {{\"call_id\":…,\"content\":…,\"is_error\":…}}: \
                                 {error}"
                            ),
                        )
                    }
                }
            }
            out
        }
        _ => return failed(400, "the body carries no `results` array".to_string()),
    };

    if let Err(reply) = declared(app_dir, slug, name) {
        return reply;
    }

    let Some(model) = model else {
        return failed(
            503,
            "this process has no model endpoint configured, so no turn can run — POST /api/config \
             with base_url, api_key and model first"
                .to_string(),
        );
    };

    // The same declarations as the turn that paused: a resumed round may need a tool the first
    // round never called, and a turn built without it would answer "no such tool".
    let declarations = body.get("tools");
    let agent = match build(model, home, app_dir, slug, name, session_key, declarations) {
        Ok(agent) => agent,
        Err(reply) => return reply,
    };
    // The same two as the opening turn: the resumed round is a turn like any other, and a person
    // who pressed stop while the app was running the calls meant this round too.
    match agent.resume_watched(&results, cancel, on_text) {
        Ok(outcome) => reply(outcome),
        Err(error) => error_reply(&error),
    }
}

/// `POST /api/apps/<slug>/tools` — the app's tools, **called** by deepnest.
///
/// This is MCP with deepnest as the client, and it is the road a call takes when the callers are
/// not the app: the dialog's own turn, or a third-party MCP server reached the same way. An app's
/// own agent does not come here — it takes its declarations with its request and runs the calls
/// itself, because calling back into the app from inside the app's own turn is the deadlock.
///
/// The body is one JSON-RPC message. `tools/list` answers with the declarations the app registered
/// for itself in `deepnest.toml`… which is not where a tool's *implementation* lives, so what this
/// route actually does is tell the caller where the app's own handler is: everything under its
/// `tools` path on its own port. It exists so the shape is discoverable and the refusal is precise,
/// rather than a `404` that leaves an app's author guessing which protocol was meant.
pub fn tools_route(request: &Request, app_dir: &Path, slug: &Slug) -> Reply {
    if request.method != Method::Post {
        return failed(
            405,
            "MCP sends every message as a POST: POST this route with one JSON-RPC \
                            message",
        );
    }
    let Some(body) = request.body_text() else {
        return failed(400, "the body is not UTF-8");
    };
    let message: Value = match serde_json::from_str(body.trim()) {
        Ok(value) => value,
        Err(error) => {
            return failed(
                400,
                format!("the body is not one JSON-RPC message: {error}"),
            );
        }
    };
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let manifest = match manifest::load(app_dir) {
        Ok(manifest) => manifest,
        Err(error) => return failed(404, format!("`{slug}` is not an app: {error}")),
    };
    let path = manifest
        .agents
        .iter()
        .find_map(|agent| agent.tools.as_deref());

    // A JSON-RPC answer, always `200` once the message parsed: the protocol puts the failure in
    // the envelope, and an HTTP status beside it would be a second, contradictory verdict.
    let answer = match (method, path) {
        ("tools/list", Some(path)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!(
                    "this route is deepnest's client side, not the app's server side. `{slug}` \
                     serves its tools at `{path}` on its own port, which deepnest reaches directly. \
                     Its own agent declares the same tools in the `tools` field of its \
                     POST /api/apps/{slug}/agent request."
                )
            }
        }),
        ("tools/list", None) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!(
                    "`{slug}` declares no tools — add `tools = \"/mcp\"` to an [[agent]] in its \
                     deepnest.toml and serve that path on the app's own port"
                )
            }
        }),
        (other, _) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!(
                    "`{other}` is not a method this route answers. deepnest asks an app's MCP \
                     server for `tools/list` and `tools/call`, directly, at the app's own port."
                )
            }
        }),
    };
    Reply {
        status: 200,
        body: answer,
    }
}

/// The `agent` field of a request body, when it names one.
///
/// Read twice on purpose — once here to refuse an undeclared name before a model is looked for, and
/// once inside the route that then uses it. The alternative is a router that parses the body and
/// hands the route a half-built request, which is the shape that lets the two decisions disagree.
pub fn named(request: &Request) -> Option<String> {
    let body: Value = serde_json::from_str(request.body_text()?.trim()).ok()?;
    body.get("agent")
        .and_then(Value::as_str)
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Whether `slug` declares an agent called `name`, or the refusal that says what it does declare.
///
/// Checked before the model is looked for, because the two failures are different requests: a name
/// that does not exist is a `404` whatever this process can reach, while "no endpoint configured"
/// is about the machine and would be the same answer for a name that does exist. Answering `503`
/// first would send an app's author looking at their config when their typo is the problem.
pub fn declared(app_dir: &Path, slug: &Slug, name: &str) -> Result<(), Reply> {
    let manifest = match manifest::load(app_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            return Err(failed(
                404,
                format!("`{slug}` is not an app in this deepnest home: {error}"),
            ))
        }
    };
    if manifest.agent(name).is_some() {
        return Ok(());
    }
    let names: Vec<&str> = manifest
        .agents
        .iter()
        .map(|agent| agent.name.as_str())
        .collect();
    let list = if names.is_empty() {
        "this app declares no agents — add an [[agent]] table to its deepnest.toml".to_string()
    } else {
        format!("this app declares: {}", names.join(", "))
    };
    Err(failed(
        404,
        format!("`{name}` is not an agent of `{slug}`. {list}"),
    ))
}

/// Where one declared agent's conversation lives.
///
/// One directory per agent, so two agents of one app never append to the same file — the failure
/// that would produce is a turn reading another agent's memory as its own. A `session` key from the
/// app nests one level further, sanitized rather than trusted: it becomes a directory name, and an
/// app that sent `../../config` would otherwise choose where this writes. Same rule as a skill name
/// and an agent name — lowercase letters, digits, hyphen — because this value ends up in a path on
/// somebody's disk.
///
/// Pulled out of [`build`] rather than repeated by the transcript route: a page that reads a
/// different file from the one the turn writes is a dialog showing another conversation's words,
/// and it would look like a working page.
pub fn conversation(
    app_dir: &Path,
    name: &str,
    session_key: Option<&str>,
) -> Result<AgentPaths, Reply> {
    let root = AgentPaths::for_app(app_dir)
        .root()
        .join("agents")
        .join(name);
    let paths = match session_key {
        Some(key) => {
            let safe: String = key
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect();
            if safe.trim_matches('-').is_empty() {
                return Err(failed(
                    400,
                    "the session key has no characters a directory name can use".to_string(),
                ));
            }
            AgentPaths::at(root.join("sessions").join(safe))
        }
        None => AgentPaths::at(root),
    };
    if let Err(error) = paths.ensure() {
        return Err(failed(
            500,
            format!("the agent's directory could not be created: {error}"),
        ));
    }
    Ok(paths)
}

/// `GET /api/apps/<slug>/agent` — the conversation one declared agent has had.
///
/// The read half of the route above, and the reason an app's own chat window survives a reload: the
/// turn writes `<app>/.agent/agents/<name>/history.jsonl` and this reads the same file back through
/// the same parser the dialog's transcript uses, so two pages of one repository cannot disagree
/// about what a turn looks like.
///
/// `agent` and `session` are query parameters rather than a body, because this changes nothing: a
/// transcript behind a `POST` would be a page that has to pretend to send something in order to be
/// able to look. A name the app never declared is refused with the names it does declare, and a
/// conversation nobody has had is an empty list — a page that cannot show what happened is worse
/// than a page saying "nothing yet".
pub fn transcript(request: &Request, app_dir: &Path, slug: &Slug) -> Reply {
    let name = crate::router::query_parameter(&request.query, "agent").unwrap_or_default();
    if name.trim().is_empty() {
        return failed(
            400,
            format!(
                "the query names no agent: GET /api/apps/{slug}/agent?agent=<name>. {}",
                declared_names(app_dir)
            ),
        );
    }
    let name = name.trim();
    if let Err(reply) = declared(app_dir, slug, name) {
        return reply;
    }
    let session_key = crate::router::query_parameter(&request.query, "session");
    let paths = match conversation(app_dir, name, session_key.as_deref()) {
        Ok(paths) => paths,
        Err(reply) => return reply,
    };
    // The agent's own directory, so a page opens on an empty conversation rather than a `500`.
    let turns = crate::history::read_at(&paths.history());
    Reply {
        status: 200,
        body: json!({
            "agent": name,
            "session": session_key,
            "turns": turns,
        }),
    }
}

/// What this app declares, as one sentence, for a refusal to end with.
fn declared_names(app_dir: &Path) -> String {
    let names: Vec<String> = match manifest::load(app_dir) {
        Ok(manifest) => manifest
            .agents
            .iter()
            .map(|agent| agent.name.clone())
            .collect(),
        Err(_) => Vec::new(),
    };
    if names.is_empty() {
        "this app declares no agents — add an [[agent]] table to its deepnest.toml".to_string()
    } else {
        format!("this app declares: {}", names.join(", "))
    }
}

/// Build one declared agent's turn.
fn build(
    model: Arc<dyn Model>,
    home: &Path,
    app_dir: &Path,
    slug: &Slug,
    name: &str,
    session_key: Option<&str>,
    declarations: Option<&Value>,
) -> Result<Agent, Reply> {
    let manifest = match manifest::load(app_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            return Err(failed(
                404,
                format!("`{slug}` is not an app in this deepnest home: {error}"),
            ))
        }
    };
    let Some(spec) = manifest.agent(name) else {
        let declared: Vec<&str> = manifest
            .agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect();
        let list = if declared.is_empty() {
            "this app declares no agents — add an [[agent]] table to its deepnest.toml".to_string()
        } else {
            format!("this app declares: {}", declared.join(", "))
        };
        return Err(failed(
            404,
            format!("`{name}` is not an agent of `{slug}`. {list}"),
        ));
    };

    let paths = conversation(app_dir, name, session_key)?;

    // Where it works: the manifest's `workdir`, resolved against the app's own directory so a
    // relative path means "beside the app" and never "wherever deepnest was started".
    let workdir: PathBuf = match &spec.workdir {
        Some(text) => {
            let path = Path::new(text);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                app_dir.join(path)
            }
        }
        None => app_dir.to_path_buf(),
    };
    if !workdir.is_dir() {
        return Err(failed(
            409,
            format!(
                "the working directory for `{name}` is {}, which is not a directory. Create it, or \
                 fix `workdir` in the app's deepnest.toml.",
                workdir.display()
            ),
        ));
    }

    // The fence: the app's own roots, plus the working directory when it is a different place.
    let mut policy = Policy::for_app(Mode::WorkspaceWrite, home, slug.as_str(), None);
    if workdir != app_dir {
        policy = policy.with_extra_root(WritableRoot::directory(&workdir));
    }
    let writable_roots: Vec<String> = {
        let mut roots = vec![app_dir.display().to_string()];
        if workdir != app_dir {
            roots.push(workdir.display().to_string());
        }
        roots
    };

    let memory = MemoryStore::open(paths.memory_dir())
        .ok()
        .and_then(|store| store.index_for_prompt().ok())
        .unwrap_or_default();

    // The app's skills, the user's, and whatever the manifest names — as directories, because a
    // skill is a local file and "local or remote" is not a distinction this makes.
    let mut skill_roots: Vec<PathBuf> = Vec::new();
    for extra in &spec.skills {
        let path = Path::new(extra);
        skill_roots.push(if path.is_absolute() {
            path.to_path_buf()
        } else {
            app_dir.join(path)
        });
    }
    let skills = deepnest_skills::load_many(home, Some(app_dir), &skill_roots);
    let skills: Vec<(String, String, String)> = skills
        .skills
        .iter()
        .map(|skill| {
            (
                skill.name.clone(),
                skill.description.clone(),
                skill.path.display().to_string(),
            )
        })
        .collect();

    let context = PromptContext::new(
        format!("{slug}/{name}"),
        workdir.display().to_string(),
        Mode::WorkspaceWrite.as_str(),
    )
    .with_writable(writable_roots)
    .with_memory_index(memory)
    .with_memory_dir(paths.memory_dir().display().to_string())
    .with_skills(skills)
    .with_app_url(format!("/app/{slug}/"));

    // The six leaves, and the app's own capabilities when it declared any.
    //
    // The declarations arrive **in the request**, not from a call to the app. That is the whole of
    // how this avoids the deadlock rather than merely surviving it: asking the app for its
    // `tools/list` from inside a turn the app is waiting on is the same nested call the pause
    // exists to prevent, one layer earlier. The app sends what it offers, and the turn is built
    // from that.
    let mut tools: Vec<Box<dyn deepnest_tools::Tool>> = registry::with_seams(None, None, None);
    if let Some(listed) = declarations {
        for declaration in deepnest_tools::declared::declarations(listed) {
            tools.push(Box::new(deepnest_tools::DeclaredTool::new(
                declaration,
                slug.as_str(),
            )));
        }
    }

    let llm_log = paths.llm_log();
    let session = match Session::open(paths.history()) {
        Ok(session) => session,
        Err(error) => {
            return Err(failed(
                500,
                format!("the conversation could not be opened: {error}"),
            ))
        }
    };

    let mut system =
        deepnest_agent::prompt::system_prompt(&context.with_tools(registry::names(&tools)));
    // What the app is for, in its own words, after the prompt and never instead of it: everything
    // deepnest says to every agent — the fence, the tools, the memory rules — stays in front of it,
    // so a manifest cannot turn the fence off by describing itself differently.
    if let Some(prompt) = &spec.prompt {
        system.push_str("\n\n## What this app uses you for\n\n");
        system.push_str(prompt.trim());
        system.push('\n');
    }
    // A handed-back turn needs a sentence the dialog's agent never needs: the model is told that a
    // batch comes back as results in a later request, so it does not read the absence of an answer
    // as a tool having failed.
    system.push_str(
        "\n\n## When a call is handed back\n\n\
This conversation may be run for the app itself rather than for a person watching. When it is, a \
call to one of the app's own tools does not run inside this turn: the turn ends and the calls come \
back to the app, which runs them and sends the results in a further request. If your next request \
carries results you did not read in the same response, that is this handoff and not an error. Put \
the calls you can make together in one response — every handoff is a round trip of its own.\n",
    );

    let ctx = deepnest_tools::ToolContext::new(policy, workdir);
    let agent = Agent::new(model, tools, ctx, session, system)
        .with_llm_log(llm_log)
        // The whole reason this module exists: the app is inside a request of its own.
        .with_handoff(true);
    Ok(agent)
}

/// One outcome, as the app reads it.
fn reply(outcome: TurnOutcome) -> Reply {
    match outcome {
        TurnOutcome::Answered {
            text,
            rounds,
            usage,
        } => Reply {
            status: 200,
            body: json!({
                "outcome": "answered",
                "text": text,
                "rounds": rounds,
                "usage": usage,
            }),
        },
        TurnOutcome::Paused {
            text,
            rounds,
            usage,
            pending,
        } => Reply {
            status: 200,
            body: json!({
                "outcome": "paused",
                "text": text,
                "rounds": rounds,
                "usage": usage,
                // The app runs these in its own process, then POSTs them back to agent-result.
                // `arguments` is a JSON string rather than an object on purpose: it is what the
                // model wrote, and re-encoding it here would be a second spelling of one value.
                "pending": pending,
                "next": "POST /api/apps/<slug>/agent-result with every pending call answered",
            }),
        },
        TurnOutcome::RoundLimit {
            text,
            rounds,
            usage,
        } => Reply {
            status: 200,
            body: json!({
                "outcome": "round_limit",
                "text": text,
                "rounds": rounds,
                "usage": usage,
                "note": "the round budget ran out with the model still asking for work; send \
                         another message to continue",
            }),
        },
        TurnOutcome::Cancelled { note, usage } => Reply {
            status: 200,
            body: json!({ "outcome": "cancelled", "text": note, "usage": usage }),
        },
    }
}

fn error_reply(error: &AgentError) -> Reply {
    let status = match error {
        AgentError::Refused { .. } => 422,
        AgentError::Model { .. } => 502,
        AgentError::Session { .. } => 500,
        AgentError::Paused { .. } => 409,
    };
    failed(status, error.to_string())
}

/// The request body as text, or the `400` that says it could not be read as one.
///
/// `None` from [`crate::http::Request::body_text`] is not an empty body: it is bytes this server
/// cannot read as the JSON every route here expects, and saying so is better than parsing an empty
/// string and blaming the caller for sending nothing.
fn body_text(request: &Request) -> Result<&str, Reply> {
    request
        .body_text()
        .ok_or_else(|| failed(400, "the body is not UTF-8, so no JSON can be read from it"))
}

/// A failure in the shape every route here answers with.
///
/// `chat::Reply`'s own constructor is private to that module, and a second one there would be two
/// spellings of one answer. This is the spelling for these routes.
fn failed(status: u16, message: impl Into<String>) -> Reply {
    Reply {
        status,
        body: json!({ "error": message.into() }),
    }
}

/// The ticket value an app reads out of its environment, spelled for the docs.
pub const TICKET_ENV: &str = ticket::ENV;

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_llm::{
        AssistantTurn, FinishReason, FunctionCall, LlmError, Message, ToolCall, ToolDefinition,
    };
    use std::sync::Mutex;

    /// A model that answers from a script, recording what it was asked.
    ///
    /// The one thing it has to get right is that a turn is more than one call: the first answer
    /// asks for a tool, the second answers in words. That is the shape every handoff test needs,
    /// and writing it here rather than in each test is what keeps them about the handoff.
    struct Scripted {
        replies: Mutex<Vec<AssistantTurn>>,
        seen: Mutex<Vec<Vec<Message>>>,
        offered: Mutex<Vec<Vec<ToolDefinition>>>,
    }

    impl Scripted {
        fn new(replies: Vec<AssistantTurn>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies),
                seen: Mutex::new(Vec::new()),
                offered: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn last_offer(&self) -> Vec<String> {
            self.offered
                .lock()
                .unwrap()
                .last()
                .map(|tools| {
                    tools
                        .iter()
                        .map(|tool| tool.function.name.clone())
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    impl Model for Scripted {
        fn turn(
            &self,
            messages: &[Message],
            tools: &[ToolDefinition],
        ) -> Result<AssistantTurn, LlmError> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.offered.lock().unwrap().push(tools.to_vec());
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                panic!("the model was asked more times than the script has answers");
            }
            Ok(replies.remove(0))
        }
    }

    fn says(text: &str) -> AssistantTurn {
        AssistantTurn {
            content: Some(text.to_string()),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: deepnest_llm::Usage::default(),
        }
    }

    fn asks(id: &str, name: &str, arguments: &str) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: arguments.to_string(),
                },
            }],
            finish_reason: FinishReason::ToolCalls,
            usage: deepnest_llm::Usage::default(),
        }
    }

    /// A scratch `~/.deepnest` with one app that declares one agent.
    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-appagent-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("apps").join("notes")).unwrap();
            std::fs::write(
                base.join("apps")
                    .join("notes")
                    .join(deepnest_apps::MANIFEST_FILE),
                "[app]\nname = \"Notes\"\n\n[[agent]]\nname = \"triage\"\n\
                 tools = \"/mcp\"\nprompt = \"Decide what a message needs.\"\n",
            )
            .unwrap();
            Self { base }
        }

        fn home(&self) -> PathBuf {
            self.base.clone()
        }

        fn dir(&self) -> PathBuf {
            self.base.join("apps").join("notes")
        }

        fn slug(&self) -> Slug {
            Slug::parse("notes").unwrap()
        }

        /// A request shaped like the one an app's own process sends.
        fn post(&self, action: &str, body: &Value) -> Request {
            let text = body.to_string();
            let mut request = crate::http::parse(&format!(
                "POST /api/apps/notes/{action} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\
                 authorization: bearer {}\r\ncontent-type: application/json\r\n\
                 Content-Length: {}\r\n\r\n",
                "a".repeat(64),
                text.len()
            ))
            .unwrap();
            request.body = text.into_bytes();
            request
        }

        /// A `GET` with a query string, the way an app's own page reads its conversation.
        fn read(&self, action: &str, query: &str) -> Request {
            crate::http::parse(&format!(
                "GET /api/apps/notes/{action}?{query} HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\r\n"
            ))
            .unwrap()
        }

        /// The declarations an app sends with its request: one tool, run in the app.
        fn tools() -> Value {
            json!({
                "tools": [{
                    "name": "list_items",
                    "description": "Every item in this app's list.",
                    "inputSchema": {"type": "object", "properties": {}}
                }]
            })
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_round_that_reaches_an_app_tool_comes_back_as_a_pause_instead_of_a_call() {
        // The deadlock this exists to prevent: the app is inside the request that asked for this
        // turn, so a call back into it would wait for an answer the app cannot give until it
        // returns. The turn ends with the calls instead.
        let scratch = Scratch::new("pause");
        let model = Scripted::new(vec![asks("call_1", "list_items", "{}")]);
        let request = scratch.post(
            "agent",
            &json!({
                "agent": "triage",
                "message": "what is on the list?",
                "tools": Scratch::tools(),
            }),
        );

        let reply = route(
            &request,
            Some(model.clone()),
            &scratch.home(),
            &scratch.dir(),
            &scratch.slug(),
            &Cancellation::new(),
            &mut |_| {},
        );

        assert_eq!(reply.status, 200, "{:?}", reply.body);
        assert_eq!(reply.body["outcome"], "paused", "{}", reply.body);
        let pending = reply.body["pending"].as_array().unwrap();
        assert_eq!(pending.len(), 1, "{}", reply.body);
        assert_eq!(pending[0]["call_id"], "call_1");
        assert_eq!(pending[0]["name"], "list_items");
        // The model was told about the app's tool: it could not have asked for it otherwise.
        assert!(
            model.last_offer().contains(&"list_items".to_string()),
            "{:?}",
            model.last_offer()
        );
    }

    #[test]
    fn the_answers_to_a_pause_continue_the_same_turn_and_finish_it() {
        let scratch = Scratch::new("resume");
        let model = Scripted::new(vec![
            asks("call_1", "list_items", "{}"),
            says("three things: bread, milk, coffee"),
        ]);
        let declarations = Scratch::tools();

        let paused = route(
            &scratch.post(
                "agent",
                &json!({"agent": "triage", "message": "what is on the list?", "tools": declarations}),
            ),
            Some(model.clone()),
            &scratch.home(),
            &scratch.dir(),
            &scratch.slug(),
            &Cancellation::new(),
            &mut |_| {},
        );
        assert_eq!(paused.body["outcome"], "paused", "{}", paused.body);

        let resumed = result_route(
            &scratch.post(
                "agent-result",
                &json!({
                    "agent": "triage",
                    "tools": declarations,
                    "results": [{
                        "call_id": "call_1",
                        "content": "bread\nmilk\ncoffee",
                        "is_error": false
                    }],
                }),
            ),
            Some(model.clone()),
            &scratch.home(),
            &scratch.dir(),
            &scratch.slug(),
            &Cancellation::new(),
            &mut |_| {},
        );

        assert_eq!(resumed.status, 200, "{:?}", resumed.body);
        assert_eq!(resumed.body["outcome"], "answered", "{}", resumed.body);
        assert_eq!(resumed.body["text"], "three things: bread, milk, coffee");
        // Two model calls for one turn, and the second saw the tool's answer in the conversation —
        // which is what makes it the same turn rather than a new one.
        assert_eq!(model.calls(), 2);
        let seen = model.seen.lock().unwrap();
        let last = seen.last().unwrap();
        assert!(
            last.iter()
                .any(|message| message.content.as_deref() == Some("bread\nmilk\ncoffee")),
            "{last:?}"
        );
    }

    #[test]
    fn a_partial_batch_is_refused_before_anything_is_written() {
        // A transcript whose assistant record has one answer and not the other is one no later
        // request can be built from: the provider rejects it, and every turn after it fails with a
        // complaint about a batch nobody remembers.
        let scratch = Scratch::new("partial");
        let model = Scripted::new(vec![asks("call_1", "list_items", "{}")]);
        let declarations = Scratch::tools();
        route(
            &scratch.post(
                "agent",
                &json!({"agent": "triage", "message": "what is on the list?", "tools": declarations}),
            ),
            Some(model.clone()),
            &scratch.home(),
            &scratch.dir(),
            &scratch.slug(),
            &Cancellation::new(),
            &mut |_| {},
        );

        let resumed = result_route(
            &scratch.post(
                "agent-result",
                &json!({"agent": "triage", "results": [], "tools": declarations}),
            ),
            Some(model),
            &scratch.home(),
            &scratch.dir(),
            &scratch.slug(),
            &Cancellation::new(),
            &mut |_| {},
        );
        assert!(resumed.status >= 400, "{:?}", resumed.body);
    }

    #[test]
    fn an_agent_built_for_an_app_hands_its_round_back_without_being_asked() {
        // The flag is a property of *who is waiting*, and this is where it is set. Asserted by
        // behaviour rather than by reading the field back: what matters is that a turn built by
        // this module pauses on an app's tool, and a test that asked an accessor would pass for an
        // agent that had the flag and never consulted it.
        let scratch = Scratch::new("handoff");
        let declarations = Scratch::tools();
        let model = Scripted::new(vec![asks("call_1", "list_items", "{}")]);
        let agent = build(
            model,
            &scratch.home(),
            &scratch.dir(),
            &scratch.slug(),
            "triage",
            None,
            Some(&declarations),
        )
        .unwrap();

        let outcome = agent
            .run_turn("what is on the list?", &Cancellation::new())
            .unwrap();
        match outcome {
            TurnOutcome::Paused { pending, .. } => {
                assert_eq!(pending.len(), 1, "{pending:?}");
                assert_eq!(pending[0].name, "list_items");
            }
            other => panic!("the app's own tool must come back, not run here: {other:?}"),
        }
    }

    #[test]
    fn a_transcript_reads_the_same_file_the_turn_wrote() {
        // The whole point of the read route: a reload shows what was said. Two files would make
        // that a window that opens empty on a conversation that happened, which looks like the
        // feature being broken rather than the path being wrong.
        let scratch = Scratch::new("transcript-read");
        let model = Scripted::new(vec![says("nothing to do.")]);
        let request = scratch.post("agent", &json!({"agent": "triage", "message": "hello"}));
        let answered = route(
            &request,
            Some(model),
            &scratch.home(),
            &scratch.dir(),
            &scratch.slug(),
            &Cancellation::new(),
            &mut |_| {},
        );
        assert_eq!(answered.status, 200, "{:?}", answered.body);

        let read = transcript(
            &scratch.read("agent", "agent=triage"),
            &scratch.dir(),
            &scratch.slug(),
        );
        assert_eq!(read.status, 200, "{:?}", read.body);
        let turns = read.body["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 1, "{}", read.body);
        assert_eq!(turns[0]["user"], "hello", "{}", read.body);
        assert_eq!(turns[0]["reply"], "nothing to do.", "{}", read.body);

        // And it is the agent's own file, not the app's dialog transcript one directory up.
        let paths = conversation(&scratch.dir(), "triage", None).unwrap();
        let app_level = crate::sessions::history_path(&scratch.dir());
        assert_ne!(paths.history(), app_level);
        assert!(
            paths.history().ends_with("triage/history.jsonl"),
            "{:?}",
            paths
        );
    }

    #[test]
    fn a_session_key_names_a_conversation_but_cannot_choose_where_it_lives() {
        // The key arrives from the app, becomes a directory name, and an app that sent `../..`
        // would otherwise pick a path on somebody's disk. Sanitized rather than refused where it
        // is still usable, and refused where nothing usable is left.
        let scratch = Scratch::new("transcript-session");
        let for_app = AgentPaths::for_app(scratch.dir());
        let root = for_app.root();
        let nested = conversation(&scratch.dir(), "triage", Some("../../elsewhere")).unwrap();
        assert!(
            nested
                .history()
                .starts_with(root.join("agents").join("triage").join("sessions")),
            "a session key escaped its directory: {:?}",
            nested.history()
        );
        let refused = conversation(&scratch.dir(), "triage", Some("../..")).unwrap_err();
        assert_eq!(refused.status, 400, "{:?}", refused.body);
    }

    #[test]
    fn a_transcript_for_a_name_that_was_never_declared_lists_the_ones_that_were() {
        let scratch = Scratch::new("transcript-unknown");
        let unknown = transcript(
            &scratch.read("agent", "agent=nope"),
            &scratch.dir(),
            &scratch.slug(),
        );
        assert_eq!(unknown.status, 404, "{:?}", unknown.body);
        assert!(
            unknown.body.to_string().contains("triage"),
            "{}",
            unknown.body
        );

        // A query with no name at all says how to name one, rather than reading the first agent.
        let bare = transcript(&scratch.read("agent", ""), &scratch.dir(), &scratch.slug());
        assert_eq!(bare.status, 400, "{:?}", bare.body);
        assert!(bare.body.to_string().contains("?agent="), "{}", bare.body);
    }
}
