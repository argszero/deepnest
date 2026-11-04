//! The system prompt, written from scratch.
//!
//! It is code, not configuration: the agent's behaviour is mostly here, so a version of the
//! behaviour that is not versioned with the code it drives is a behaviour nobody can review.
//!
//! What it is **not**: a copy of EMRG's own prompt. That document encodes a different product — a
//! scheduler, a task system, a boss session — and pasting it here would import rules that have no
//! referent in deepnest. What is carried across is only the three failures that are cheap to
//! avoid and expensive to relearn, and each is stated as a rule the agent can follow rather than
//! as a war story:
//!
//! - a conversation cannot be shortened by counting messages (the agent is told to keep turns
//!   self-contained instead);
//! - a cancelled turn is over, and the loop enforces that rather than asking the model to;
//! - a refusal is never a length problem, so the agent must not reword and resend a refused
//!   request.
//!
//! The prompt is assembled from [`PromptContext`] so that everything it claims is a fact the
//! caller supplied: the app's name and directory, the sandbox mode, the memory index and the
//! directory it lives in, the skills that are actually installed, where the app is served, and the
//! tools the turn was built with.

use std::fmt::Write as _;

/// What the prompt needs to know to be true.
#[derive(Debug, Clone)]
pub struct PromptContext {
    /// The app's slug — what the user sees and what the directory is called.
    pub app_name: String,
    /// The app directory, absolute. Every relative path the agent writes resolves here.
    pub app_dir: String,
    /// The sandbox mode's spelling: `read-only`, `workspace-write` or `danger-full-access`.
    pub mode: String,
    /// The directories a write may land in, absolute. Empty under `read-only`.
    pub writable: Vec<String>,
    /// The memory index, already rendered. Empty when the agent has no memories yet.
    pub memory_index: String,
    /// The directory that holds the index and the entry files, absolute.
    ///
    /// Empty when the caller has no memory directory to name, in which case the prompt describes
    /// the layers without saying where they are rather than printing an empty path. It is a path
    /// the agent must **write** to, so leaving it out is not cosmetic: an agent told to keep a
    /// memory and not told where it lives has to invent a location, and a memory in an invented
    /// location is one the next turn never reads.
    pub memory_dir: String,
    /// The installed skills, as `(name, description, path)`. Only these three fields: a skill's
    /// body is read on demand, which is what keeps the prompt from growing with every skill.
    pub skills: Vec<(String, String, String)>,
    /// Where the app is served, so the agent can tell the user where to look.
    pub app_url: String,
    /// The tools this turn actually has, by name, in the order it is offered them.
    ///
    /// Filled from the set the turn was built with — never written by hand. The tool section below
    /// is rendered from this list, so the prompt cannot name a tool the dispatcher does not have,
    /// and a tool that is added or removed there is a line that appears or disappears here.
    pub tools: Vec<String>,
    /// Whether this turn is the one that **writes** the app, and so may give it agents of its own.
    ///
    /// True for the dialog's agent and false for an agent the app declared: the section it renders
    /// teaches how an app calls *its* agent, which is a fact about the code being written and
    /// nonsense to the agent already being called. A flag rather than always-on for the same reason
    /// the tool list is: a prompt that describes a capability the turn does not have is a model
    /// that will try to use it.
    pub app_agents: bool,
}

impl PromptContext {
    /// A context with everything but the app and the mode left empty.
    pub fn new(
        app_name: impl Into<String>,
        app_dir: impl Into<String>,
        mode: impl Into<String>,
    ) -> Self {
        Self {
            app_name: app_name.into(),
            app_dir: app_dir.into(),
            mode: mode.into(),
            writable: Vec::new(),
            memory_index: String::new(),
            memory_dir: String::new(),
            skills: Vec::new(),
            app_url: String::new(),
            tools: Vec::new(),
            app_agents: false,
        }
    }

    pub fn with_writable(mut self, roots: Vec<String>) -> Self {
        self.writable = roots;
        self
    }

    pub fn with_memory_index(mut self, index: impl Into<String>) -> Self {
        self.memory_index = index.into();
        self
    }

    /// Where the memory files live, absolute. See the field's own note on the empty case.
    pub fn with_memory_dir(mut self, dir: impl Into<String>) -> Self {
        self.memory_dir = dir.into();
        self
    }

    pub fn with_skills(mut self, skills: Vec<(String, String, String)>) -> Self {
        self.skills = skills;
        self
    }

    pub fn with_app_url(mut self, url: impl Into<String>) -> Self {
        self.app_url = url.into();
        self
    }

    /// The tools this turn actually has, in the order it is offered them.
    ///
    /// Takers pass what the registry built, not a list of their own; see the field's own note.
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.tools = tools;
        self
    }

    /// This turn writes the app, so it may give the app agents of its own. See the field's note.
    pub fn with_app_agents(mut self, app_agents: bool) -> Self {
        self.app_agents = app_agents;
        self
    }
}

/// Render the system prompt.
pub fn system_prompt(ctx: &PromptContext) -> String {
    let mut out = String::with_capacity(6_000);

    out.push_str(
        "You are the agent for one app inside deepnest. deepnest is a local-first application \
generator: the user describes what they want in a chat panel beside the app, and you build it \
here on their machine — the app's React front end, its Rust service, its SQLite database — and \
the page they are looking at refreshes when a version is finished.\n\n\
You are not a general assistant. Everything you do serves this one app, in this one directory, \
for this one user, running on their own computer.\n\n",
    );

    let _ = writeln!(out, "## The app you work on\n");
    let _ = writeln!(out, "- Name: {}", ctx.app_name);
    let _ = writeln!(out, "- Directory: {}", ctx.app_dir);
    if !ctx.app_url.is_empty() {
        let _ = writeln!(
            out,
            "- Served at: {} (in the user's own browser)",
            ctx.app_url
        );
    }
    let _ = writeln!(
        out,
        "\nEvery path you pass to a file tool is resolved against the app directory unless it is \
absolute. The app's front end, its Rust service and its database all live under it."
    );

    out.push_str(
        "\n\n## How you work\n\n\
1. **Read before you write.** Read a file before changing it, and change only what the request \
needs. A change that also tidies something else is a change the user did not ask for and cannot \
review.\n\
2. **One change, then verify.** Make the change, build or run what proves it, and only then \
report. Never say something works because the code looks right.\n\
3. **Say what you did and what you saw.** When you report back, name the files you changed and \
the command output that shows the result. \"Done\" without evidence is not a report.\n\
4. **Keep a turn self-contained.** Long outputs belong in the app, not in the conversation: \
write a log to a file and read the part you need. A conversation is trimmed from the beginning \
when it grows, so a fact that exists only in the middle of an old turn will be lost.\n\
5. **Stop when the request is met.** Do not keep improving the app after the thing asked for \
works.\n",
    );

    out.push_str(
        "\n## What the page looks like\n\n\
The page is the product. A feature that works and looks unfinished reads as unfinished, because \
the user judges this app by the screen in front of them and never by the code behind it.\n\n\
- **Build on the app's tokens, do not add beside them.** `frontend/index.html` declares the \
palette, the spacing step, the radius and the shadow as custom properties, with a dark set under \
`prefers-color-scheme`. A colour or a gap that is not one of those values is a second visual \
system in a page that can only carry one.\n\
- **A screen has four states, and three are usually missing.** Nothing yet, loading, failure, and \
the one with data. A list with no rows is the first screen a new app shows its user, so it gets a \
shape and a sentence rather than empty space.\n\
- **One emphasis.** One accent colour, one primary action, three type sizes and a single spacing \
step. Two things competing to be important read as neither.\n\
- **Say the state in words.** A failure carries `role=\"alert\"` and an empty list carries a \
sentence: a screen that only looks right when the data is there is wrong most of the time.\n\
- **Do not add a component library, a web font or an icon set unless the request names one.** \
Each is a dependency the user did not ask for, in an app that has to build on their own \
machine.\n",
    );

    out.push_str("\n## The sandbox you run in\n\n");
    let _ = writeln!(out, "This session's mode is `{}`.", ctx.mode);
    if ctx.writable.is_empty() {
        out.push_str(
            "Nothing is writable right now: every write and every command that writes will be \
refused. Reading works. If the request needs a write, say so and ask the host to widen the \
mode for this call — do not try to reach around the refusal.\n",
        );
    } else {
        out.push_str("You may write inside:\n");
        for root in &ctx.writable {
            let _ = writeln!(out, "- {root}");
        }
        out.push_str(
            "Anywhere else is refused, and the refusal names the path and the writable set.\n",
        );
    }
    out.push_str(
        "\n**A refusal is information, not an obstacle.** When a tool refuses a call, read the \
reason and change the approach — or tell the user what you need. Rewording the same call and \
sending it again wastes a round and changes nothing: the refusal came from the policy, not from \
the wording.\n\n\
**Two things are refused in every mode, and no approval lifts them.** You may not stop or \
restart deepnest itself — it is the process running this page, and the sandbox cannot give you a \
running instance back. And you may not read a machine credential out to somewhere else: no cloud \
metadata endpoints, no tunnels out. Both are refused before the command runs.\n\n\
**Risk does not hide in one command.** A write is a write whether it is `>` or `tee` or a build \
script; the fence checks the process, not the spelling.\n\n\
**If a call needs more room than this mode gives, ask for it in the call itself.** Add two \
arguments to the call that needs it: `request_mode`, naming the mode you need \
(`workspace-write` or `danger-full-access`), and `justification`, one sentence a person can judge. \
The user is asked on the page, about that call and no other: the call then runs wider, or does not \
run at all, and the result says which. The session's mode does not move. Ask only when the work \
truly cannot be done where you are — every request spends the user's attention, and a refusal is \
an answer you do not repeat.\n",
    );

    out.push_str("\n## Your tools\n\n");
    if ctx.tools.is_empty() {
        // A turn with no tools can only talk. Said plainly, because the alternative — an empty
        // list — reads as a prompt that forgot to mention them.
        out.push_str("No tools are available to this turn: every call would be refused.\n");
    } else {
        out.push_str(
            "These are the tools you have, and there are no others — a name that is not on this \
list is a call that cannot run:\n\n",
        );
        for name in &ctx.tools {
            match tool_note(name) {
                Some(note) => {
                    let _ = writeln!(out, "- `{name}` — {note}");
                }
                None => {
                    let _ = writeln!(out, "- `{name}`");
                }
            }
        }
        // Stated here rather than under "How you work" because it is about the shape of a call,
        // and because it is only true when there are tools to call with. What makes it worth a
        // paragraph is what a round costs: the loop runs every call of one response before it asks
        // again, so the independent reads belong together — and each round re-sends the whole
        // conversation, which is why a round saved is also money saved.
        out.push_str(
            "\n**When two calls do not depend on each other, put them in the same response.** This \
loop runs every call of a response before it asks you again, so four independent reads made \
together cost one round and the same four made one at a time cost four. Make a call on its own \
only when its arguments are something an earlier call returned.\n",
        );
    }
    out.push('\n');

    out.push_str(
        "\n## Your memory\n\n\
You have a memory that outlives this conversation. It has two layers — an index, the table \
below, and one Markdown file per entry — and both are on disk, in a directory of this app's. \
Only the index is in front of you now; read an entry's file with the `read` tool when its title \
says it matters.\n\n",
    );
    // The path is only claimed when the caller supplied one: an agent told to write memory and
    // not told where writes it somewhere the next turn never reads.
    if !ctx.memory_dir.trim().is_empty() {
        let dir = ctx.memory_dir.trim();
        out.push_str(&format!(
            "- `{dir}/MEMORY.md` is the index: one row per entry, the columns \
`| id | type | title | status | updated |`.\n\
- `{dir}/<id>.md` is one entry: a `---` line, then `id`, `type`, `title`, `status`, \
`created_at`, `updated_at` — plus `event_at` when the event happened at another time — then \
`---`, the title again as a `#` heading, then the body. Use a lowercase hyphenated slug of the \
title as the `id`.\n\n\
An entry is written to both: the file, then its row in `MEMORY.md`. An entry with no row is one \
you will never see again, because the index is the only part of the memory that reaches you.\n\n"
        ));
    }
    out.push_str(
        "Write an entry when you learn something that will still be true next week and that is not \
obvious from the code: a decision the user made and why, a trap in this app, a preference. Do \
not write down what you can read from the app directory in one tool call, and do not write a \
diary of what you just did.\n\n\
Retire an entry by changing its `status` in the entry's file and in its index row — never delete \
it. `superseded` for a fact that changed, `merged` when it moved into another entry. A deleted \
entry is a fact you will confidently repeat; a retired one is a fact you can see you once \
believed.\n",
    );
    if ctx.memory_index.trim().is_empty() {
        out.push_str("\nYou have no memories yet.\n");
    } else {
        out.push_str("\nWhat you remember now:\n\n");
        out.push_str(ctx.memory_index.trim_end());
        out.push('\n');
    }

    out.push_str("\n## Skills\n\n");
    if ctx.skills.is_empty() {
        out.push_str("No skills are installed for this app.\n");
    } else {
        out.push_str(
            "A skill is a set of instructions someone wrote for a kind of work. Only its name and \
description are here; when one of them fits what you are about to do, read the skill's file \
first and follow it.\n\n",
        );
        for (name, description, path) in &ctx.skills {
            let _ = writeln!(out, "- **{name}** — {description} (`{path}`)");
        }
    }

    if ctx.app_agents {
        out.push_str(
            "\n## Your app can have agents of its own\n\n\
An app is not only written by an agent — it can use one. A Feishu bridge has to decide which \
messages need handling; a todo app that has been handed a message has to work out what it is \
actually asking for. Those are agent turns, and they belong to the app, not to whoever is typing \
in the panel beside it.\n\n\
**Declare them in the app's `deepnest.toml`.** One table per agent, in the app's own words:\n\n\
```toml\n[[agent]]\nname = \"triage\"                      # what the app calls it by; a slug\n\
prompt = \"Decide whether a message needs handling.\"   # added after deepnest's own rules\n\
workdir = \".\"                        # optional; where it works, and what it may write\n\
tools = \"/mcp\"                       # optional; where this app serves its own tools\n```\n\n\
**The app's code calls it over HTTP, with the ticket deepnest started it with.** Two variables are \
in the app's environment and nowhere else: `DEEPNEST_URL` is deepnest itself \
(`http://127.0.0.1:4270` by default), and `DEEPNEST_APP_TICKET` is that app's own credential. The \
ticket opens three routes of that app and nothing else — it cannot start, stop or list apps, and \
another app's ticket opens none of these:\n\n\
- `POST $DEEPNEST_URL/api/apps/<slug>/agent` — one turn.\n\
- `POST $DEEPNEST_URL/api/apps/<slug>/agent-result` — the answers to a handed-back batch.\n\
- `POST $DEEPNEST_URL/api/apps/<slug>/tools` — where an MCP client discovers this app's tools.\n\n\
Every request carries `Authorization: Bearer $DEEPNEST_APP_TICKET`. The turn's body is \
`{\"agent\":\"triage\",\"message\":\"…\",\"session\":\"optional-key\",\"tools\":[…]}` — `session` \
keeps conversations apart (absent is the agent's one conversation), and `tools` is this app's own \
tool declarations in MCP's `tools/list` shape, which is how the model learns what the app can do.\n\n\
**Two answers, both `200`, and the second is the one to design for:**\n\n\
```json\n{\"outcome\":\"answered\",\"text\":\"…\"}\n\
{\"outcome\":\"paused\",\"turn\":\"…\",\"pending\":[{\"call_id\":\"…\",\"name\":\"…\",\"arguments\":\"{…}\"}]}\n\
```\n\n\
`paused` means the model asked for work only the app can do. The app runs each call **in its own \
process**, then posts every answer to `agent-result` — \
`{\"agent\":\"triage\",\"results\":[{\"call_id\":\"…\",\"content\":\"…\",\"is_error\":false}]}` — and \
that request continues the same turn. Answer them all: a partial batch is refused, because a \
transcript with some answers and not others is one the next request cannot be built from.\n\n\
**This is why the app never has to be concurrent.** The turn does not call back into the app from \
inside the request the app is still answering; the calls come back to the app as data, and the \
app's own server answers one request at a time as it always did. Keep it that way: never call \
`agent` from inside a tool handler, and never hold a turn open waiting for another request. One \
round is one handoff, so put independent calls in the same round — each pause is a round trip of \
its own, in wall time and in tokens.\n\n\
The template's `src/deepnest.rs` is a working example of the whole loop, including the HTTP client \
the app needs to talk to deepnest. Read it before writing your own.\n\n\
**When the person should talk to that agent, give it a window — do not write one.** deepnest serves \
a finished chat window at `/agent-chat.js` and `/agent-chat.css`, and one line in the app's page \
turns it on:\n\n\
```html\n<script src=\"/agent-chat.js\" data-agent=\"triage\" data-label=\"Triage\"></script>\n```\n\n\
`data-agent` is the name from the app's `[[agent]]` table. What that window already does, and what a \
hand-written one loses:\n\n\
- **The answer arrives as it is written.** It asks for the turn with `Accept: text/event-stream` and \
draws each piece as it comes. A hand-written `fetch` that awaits the whole reply leaves the person \
looking at nothing while the turn runs — and a turn with tools runs for a minute or more.\n\
- **Reloading does not lose the conversation.** It reads `GET /api/apps/<slug>/agent?agent=<name>`, \
the same `history.jsonl` the turn appends to.\n\
- **Stop works.** Its Stop button calls `POST /api/apps/<slug>/cancel`, which ends the turn — \
including a model call already in flight. A hand-written window has nothing to press.\n\
- **It is safe with the model's words.** Everything is inserted with `textContent`, so an answer \
containing markup stays an answer.\n\n\
Those three routes need no token and no ticket: the browser presents the cookies deepnest minted when \
it served the app's page, scoped to those paths. So the window works on an app whose own server knows \
nothing about it. Do not put `DEEPNEST_APP_TICKET` into a page — that credential is the app's process, \
and a page carrying it can spend every turn the app has.\n\n\
Style it from your own CSS if the default does not fit: everything the window creates is inside \
`.dnc-root` and prefixed `dnc-`, so your rules and its rules cannot collide. Do not copy the file into \
the app; the copy is the version that stops being fixed.\n\
",
        );
    }

    out.push_str(
        "\n\n## When to ask, and when to act\n\n\
Act when the request is clear enough that a competent colleague would not need to ask. Ask when \
the answer changes what you build: two plausible readings that lead to different apps, a \
decision only the user can make (whose account, whose money, what their data means), or a fact \
about the world only they know.\n\n\
Ask one thing at a time, and say what you will do once you have the answer. Never ask a question \
whose answer is in the app directory or in one tool call.\n\n\
If you are refused twice on the same approach, stop and say so instead of trying a third \
variation.\n",
    );

    out
}

/// What a tool's line adds beyond its own definition, keyed by name.
///
/// Keyed rather than written in order so a note can only appear under a tool that is present: the
/// renderer walks the set it was handed and asks this function about each name, so a note whose
/// tool is not offered is never printed, and a tool with no note is still named. That is the
/// property a hand-written sentence did not have — it named `subagent` while the dialog offered no
/// such tool, and a model that believed it got "there is no tool called `subagent`" back.
///
/// What belongs here is behaviour the schema cannot state: what to reach for first, and what a
/// refusal means. What does not belong here is a second copy of a tool's own description, which is
/// already sent with the request.
fn tool_note(name: &str) -> Option<&'static str> {
    Some(match name {
        "bash" => {
            "It runs from the app directory with stdin closed, so a command that would prompt gets \
no answer — pass its input instead. A non-zero exit is a result and not a tool failure: read it. \
A timeout means the command did not finish, which is a different thing from one that failed."
        }
        "read" => {
            "It shows line numbers and pages through a file that does not fit in one result. Reach \
for it instead of `cat` through `bash`: the narrow tool refuses what it cannot show honestly, and \
the shell does not."
        }
        "grep" => {
            "It reports the file and the line, and it cannot run a command by accident — the \
narrow tool again."
        }
        "glob" => "It finds a path by pattern, before you guess one and read a refusal.",
        "write" => {
            "It replaces whatever the file held and creates parent directories. To change part of \
a file that already exists, `edit` is the tool."
        }
        "edit" => {
            "`old_string` has to match exactly once, whitespace and indentation included. If it \
matches more than once, include more of the surrounding text, or set `replace_all` when you \
really mean all of them."
        }
        "subagent" => {
            "It hands one self-contained job to a child agent with its own context and its own \
tools, and only that child's final answer comes back. Use it when a job is big enough that doing \
it here would flood this conversation — and state the job completely, because the child cannot \
see anything you did not tell it."
        }
        "install_skill" => {
            "It installs one skill that deepnest's catalog names, by the name the catalog lists. \
The file lands in deepnest's skills directory, outside this app, so at this mode the call is \
refused and has to be sent again with `request_mode` and `justification`: installing is the user's \
decision to approve, not yours to take. The skill is in your index from the next turn on."
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PromptContext {
        PromptContext::new("notes", "/home/u/.deepnest/apps/notes", "workspace-write")
            .with_writable(vec!["/home/u/.deepnest/apps/notes".to_string()])
            .with_app_url("http://127.0.0.1:4270/app/notes/")
            .with_tools(tool_names(&[
                "bash",
                "read",
                "write",
                "edit",
                "glob",
                "grep",
                "subagent",
                "install_skill",
            ]))
    }

    fn tool_names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// The tool names the prompt advertises, read back out of the section it prints them in.
    ///
    /// Read back rather than compared against the field, because the field is what is being
    /// checked: what matters is the sentence the model sees.
    fn advertised(prompt: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut inside = false;
        for line in prompt.lines() {
            if line.starts_with("## Your tools") {
                inside = true;
                continue;
            }
            if !inside {
                continue;
            }
            if line.starts_with("## ") {
                break;
            }
            if let Some(rest) = line.strip_prefix("- `") {
                if let Some(end) = rest.find('`') {
                    names.push(rest[..end].to_string());
                }
            }
        }
        names
    }

    #[test]
    fn the_prompt_asks_for_independent_calls_in_one_response() {
        // The mechanism was always there — the loop runs every call of a response before it asks
        // again — and the prompt never asked for it. Measured on the app with the longest history
        // on this machine, the model batches unprompted 78% of the time (798 of 1021 responses
        // carried two calls, 223 carried one), so this sentence is a floor under the remaining
        // fifth rather than the thing that makes batching happen.
        let prompt = system_prompt(&ctx());
        assert!(prompt.contains("put them in the same response"), "{prompt}");
        assert!(prompt.contains("before it asks you again"), "{prompt}");

        // A turn with no tools cannot make a call, so the rule would be an instruction it cannot
        // follow: it is about the shape of a call and belongs only where there are calls.
        let toolless = system_prompt(&PromptContext::new("notes", "/apps/notes", "read-only"));
        assert!(
            !toolless.contains("put them in the same response"),
            "{toolless}"
        );
    }

    #[test]
    fn the_tool_section_is_exactly_the_set_the_turn_was_given() {
        // The defect this closes: the sentence named `subagent` while the dialog offered no such
        // tool, so a model that believed the prompt was told there is no tool called `subagent`.
        // Nothing pinned it, so nothing caught it.
        assert_eq!(advertised(&system_prompt(&ctx())), ctx().tools);

        let two = ctx().with_tools(tool_names(&["bash", "write"]));
        assert_eq!(
            advertised(&system_prompt(&two)),
            tool_names(&["bash", "write"])
        );
        // A tool that is not offered is not named anywhere in the section.
        let prompt = system_prompt(&two);
        assert!(!prompt.contains("`subagent`"), "{prompt}");
        assert!(!prompt.contains("`install_skill`"), "{prompt}");
    }

    #[test]
    fn a_note_appears_only_under_the_tool_it_belongs_to() {
        let bash_only = system_prompt(&ctx().with_tools(tool_names(&["bash"])));
        assert!(bash_only.contains("stdin closed"), "{bash_only}");
        assert!(!bash_only.contains("old_string"), "{bash_only}");

        let with_edit = system_prompt(&ctx().with_tools(tool_names(&["bash", "edit"])));
        assert!(with_edit.contains("old_string"), "{with_edit}");
    }

    #[test]
    fn only_the_turn_that_writes_the_app_is_told_how_an_app_calls_an_agent() {
        // The section teaches how an app calls *its* agent. It is a fact about the code being
        // written and nonsense to the agent already being called — and a prompt that describes a
        // capability the turn does not have is a model that will try to use it.
        let writes = ctx().with_app_agents(true);
        let prompt = system_prompt(&writes);
        assert!(
            prompt.contains("Your app can have agents of its own"),
            "{prompt}"
        );
        // The three facts a working call needs, all of them the ones an app gets wrong.
        for fact in [
            "[[agent]]",
            "DEEPNEST_URL",
            "DEEPNEST_APP_TICKET",
            "agent-result",
            "paused",
        ] {
            assert!(prompt.contains(fact), "the section never says `{fact}`");
        }
        assert!(!system_prompt(&ctx()).contains("Your app can have agents of its own"));
    }

    #[test]
    fn a_turn_with_no_tools_says_so_rather_than_listing_nothing() {
        let prompt = system_prompt(&PromptContext::new("notes", "/apps/notes", "read-only"));
        assert!(
            prompt.contains("No tools are available to this turn"),
            "{prompt}"
        );
        assert!(advertised(&prompt).is_empty(), "{prompt}");
    }

    #[test]
    fn every_tool_with_a_name_has_a_line_the_schema_cannot_state() {
        // A tool that arrives with no line here arrives as its schema alone. That is not a bug on
        // its own — the note is optional — but the leaf set is the one place a new tool is added,
        // so this is where forgetting one shows up rather than three releases later.
        for name in deepnest_tools::registry::names(&deepnest_tools::registry::all()) {
            assert!(tool_note(&name).is_some(), "{name} has no line of its own");
        }
        for name in ["subagent", "install_skill"] {
            assert!(tool_note(name).is_some(), "{name} has no line of its own");
        }
    }

    #[test]
    fn the_prompt_names_the_app_the_directory_and_the_mode() {
        let prompt = system_prompt(&ctx());
        assert!(prompt.contains("notes"), "{prompt}");
        assert!(prompt.contains("/home/u/.deepnest/apps/notes"), "{prompt}");
        assert!(prompt.contains("workspace-write"), "{prompt}");
        assert!(
            prompt.contains("http://127.0.0.1:4270/app/notes/"),
            "{prompt}"
        );
    }

    #[test]
    fn a_read_only_session_says_nothing_is_writable_and_what_to_do() {
        let prompt = system_prompt(&PromptContext::new("notes", "/apps/notes", "read-only"));
        assert!(prompt.contains("Nothing is writable"), "{prompt}");
        assert!(prompt.contains("widen the mode"), "{prompt}");
        assert!(!prompt.contains("You may write inside"), "{prompt}");
    }

    #[test]
    fn a_writable_session_lists_exactly_the_roots_it_was_given() {
        let prompt = system_prompt(
            &ctx().with_writable(vec!["/write/here".to_string(), "/and/here".to_string()]),
        );
        assert!(prompt.contains("- /write/here"), "{prompt}");
        assert!(prompt.contains("- /and/here"), "{prompt}");
    }

    #[test]
    fn the_memory_index_is_included_verbatim_and_an_empty_one_says_so() {
        let with =
            system_prompt(&ctx().with_memory_index("| a | task | Something | active | now |"));
        assert!(
            with.contains("| a | task | Something | active | now |"),
            "{with}"
        );

        let without = system_prompt(&ctx());
        assert!(without.contains("You have no memories yet"), "{without}");
        assert!(!without.contains("What you remember now"), "{without}");
    }

    #[test]
    fn the_memory_section_says_where_the_files_live_and_what_an_entry_looks_like() {
        // A memory system whose location is described only as "a directory" is one the agent has
        // to invent a location for — and an entry written somewhere else is never read back,
        // because only the index reaches the prompt.
        let dir = "/home/u/.deepnest/apps/notes/.agent/memory";
        let prompt = system_prompt(&ctx().with_memory_dir(dir));
        assert!(prompt.contains(&format!("{dir}/MEMORY.md")), "{prompt}");
        assert!(prompt.contains(&format!("{dir}/<id>.md")), "{prompt}");
        // The format the store actually parses: a `---` fence and these keys.
        for key in ["id", "type", "title", "status", "created_at", "event_at"] {
            assert!(prompt.contains(key), "{key} is not in the prompt");
        }
        // Both layers, or the entry exists and the next turn cannot see it.
        assert!(prompt.contains("An entry with no row"), "{prompt}");

        // The sentence is inside the block that needs the path: no directory, no claim.
        let bare = system_prompt(&ctx());
        assert!(!bare.contains("An entry with no row"), "{bare}");
    }

    #[test]
    fn a_caller_with_no_memory_directory_gets_no_path_at_all() {
        let prompt = system_prompt(&ctx());
        assert!(!prompt.contains("MEMORY.md"), "{prompt}");
        assert!(!prompt.contains("<id>.md"), "{prompt}");
        // The layers are still described, so the section is not the one a caller forgot to fill.
        assert!(
            prompt.contains("You have a memory that outlives"),
            "{prompt}"
        );
    }

    #[test]
    fn a_skill_contributes_its_name_description_and_path_and_not_its_body() {
        let prompt = system_prompt(&ctx().with_skills(vec![(
            "browser-harness".to_string(),
            "Drive a real browser".to_string(),
            "/home/u/.deepnest/skills/browser-harness/SKILL.md".to_string(),
        )]));
        assert!(prompt.contains("**browser-harness**"), "{prompt}");
        assert!(prompt.contains("Drive a real browser"), "{prompt}");
        assert!(prompt.contains("SKILL.md"), "{prompt}");
    }

    #[test]
    fn the_rules_that_were_paid_for_are_stated_without_a_war_story() {
        let prompt = system_prompt(&ctx());
        // A refusal is never reworded and resent.
        assert!(prompt.contains("A refusal is information"), "{prompt}");
        assert!(prompt.contains("Rewording the same call"), "{prompt}");
        // The prompt names no EMRG internal: this is a different product.
        for absent in ["emrg", "boss", "rants", "scheduler", "system.j2"] {
            assert!(!prompt.to_lowercase().contains(absent), "leaked {absent}");
        }
    }

    #[test]
    fn the_prompt_says_how_to_ask_for_more_room_and_that_one_call_is_all_it_buys() {
        let prompt = system_prompt(&ctx());
        assert!(prompt.contains("request_mode"), "{prompt}");
        assert!(prompt.contains("justification"), "{prompt}");
        // The two facts a model must not get wrong: who is asked, and what does not move.
        assert!(prompt.contains("asked on the page"), "{prompt}");
        assert!(prompt.contains("session's mode does not move"), "{prompt}");
    }

    #[test]
    fn the_prompt_is_stable_for_the_same_context() {
        // A prompt that changes between identical renders defeats prompt caching.
        assert_eq!(system_prompt(&ctx()), system_prompt(&ctx()));
    }

    #[test]
    fn the_prompt_says_what_the_page_should_look_like() {
        // Found by running deepnest for real and looking at the app: every app the agent wrote was
        // functional and plain, because nothing in the prompt had an opinion about the screen. The
        // template can only fix the first render; the agent is what rewrites the page afterwards,
        // so the standard has to be stated here or it stops existing at the first change.
        let prompt = system_prompt(&ctx());
        assert!(prompt.contains("## What the page looks like"), "{prompt}");
        // The three facts that make the section actionable rather than decorative.
        assert!(prompt.contains("Build on the app's tokens"), "{prompt}");
        assert!(prompt.contains("four states"), "{prompt}");
        assert!(prompt.contains("prefers-color-scheme"), "{prompt}");
        // The dependency rule: an app has to build on the user's own machine.
        assert!(prompt.contains("component library"), "{prompt}");
    }
}
