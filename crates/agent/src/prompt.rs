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
//! caller supplied: the app's name and directory, the sandbox mode, the memory index, the skills
//! that are actually installed, and where the app is served.

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
    /// The installed skills, as `(name, description, path)`. Only these three fields: a skill's
    /// body is read on demand, which is what keeps the prompt from growing with every skill.
    pub skills: Vec<(String, String, String)>,
    /// Where the app is served, so the agent can tell the user where to look.
    pub app_url: String,
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
            skills: Vec::new(),
            app_url: String::new(),
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

    pub fn with_skills(mut self, skills: Vec<(String, String, String)>) -> Self {
        self.skills = skills;
        self
    }

    pub fn with_app_url(mut self, url: impl Into<String>) -> Self {
        self.app_url = url.into();
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

    out.push_str(
        "\n## Your tools\n\n\
You have `bash`, `read`, `write`, `edit`, `glob`, `grep`, and — when a job is big enough that \
doing it here would flood the conversation — `subagent`, which runs a child agent with its own \
context and reports back only its final answer.\n\n\
- Prefer the narrow tool: `read` over `cat`, `grep` over `bash`+`grep`, `edit` over rewriting a \
file. The narrow tool refuses what it cannot do honestly, and the shell does not.\n\
- `edit` needs an `old_string` that occurs exactly once. If it occurs more than once, include \
more of the surrounding text, or set `replace_all` when you really mean all of them.\n\
- A command runs with `bash`, from the app directory, with stdin closed. A command that would \
prompt will not get an answer — pass its input instead.\n\
- A non-zero exit is a result, not a tool failure. Read it. A timeout means the command did not \
finish, which is different from one that failed.\n",
    );

    out.push_str(
        "\n## Your memory\n\n\
You have a memory that outlives this conversation. It has two layers: an index — the table \
below — and one Markdown file per entry. Only the index is in front of you now; read an entry's \
file with the `read` tool when its title says it matters.\n\n\
Write an entry when you learn something that will still be true next week and that is not \
obvious from the code: a decision the user made and why, a trap in this app, a preference. Do \
not write down what you can read from the app directory in one tool call, and do not write a \
diary of what you just did.\n\n\
Retire an entry by changing its `status` — never delete it. `superseded` for a fact that \
changed, `merged` when it moved into another entry. A deleted entry is a fact you will \
confidently repeat; a retired one is a fact you can see you once believed.\n",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PromptContext {
        PromptContext::new("notes", "/home/u/.deepnest/apps/notes", "workspace-write")
            .with_writable(vec!["/home/u/.deepnest/apps/notes".to_string()])
            .with_app_url("http://127.0.0.1:4270/app/notes/")
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
}
