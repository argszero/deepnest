# Writing a skill

A skill is Markdown the agent may choose to follow. It is instructions, not a program and not a
permission: what it says is subject to the sandbox like anything else the agent does, and it cannot
widen it. That is also why nothing here is signed, versioned or trusted — a skill is cheap to write
precisely because it is worth no more than the agent's own judgement.

deepnest reads the same two layouts the Anthropic Agent Skills format uses. This page is what
`crates/skills` does with what it finds there, stated in the order a person writing one needs it.

## Where a skill lives

There are two roots, and both are read on every round:

| Root | Whose | Scope |
|---|---|---|
| `~/.deepnest/skills/` | yours | shared by every app |
| `<app>/.agent/skills/` | one app's | that app only |

**An app's skill shadows a user skill of the same name.** The direction is deliberate: an app that
needs a different `notes` skill should not have to rename yours. A root that does not exist is not an
error — first run has not created one yet, and refusing to start would make the first thing a new
user sees a failure.

## Two layouts, and which wins

Both are spellings of the same thing:

```
~/.deepnest/skills/notes.md            # one file
~/.deepnest/skills/notes/SKILL.md      # a directory
```

A directory can carry the scripts and assets the skill refers to, which is why it is the layout to
move to when one file is no longer enough. **When both declare the same name in the same root, the
directory wins** — it is the layout that can hold more, so a skill being migrated to it keeps working
mid-move. The file it beat is **reported, not dropped**: editing `notes.md` and seeing nothing change
would otherwise leave you with nothing to look at.

## The header

A skill is Markdown with a few lines above the instructions:

```markdown
---
name: notes
description: Use when the user asks about their notes, tags or notebooks.
---

# Instructions

…what to do, in the agent's own terms…
```

`description` is **required and may not be empty**. It is the only thing the agent chooses a skill
by, so a skill without one cannot be chosen — it is not loaded, and the panel says why. `name` is
optional: it falls back to the file stem (`notes.md` → `notes`) or the directory name
(`notes/SKILL.md` → `notes`).

Four rules about the header, each of which exists because the alternative was paid for:

- **Indented lines are not fields.** A nested block — a `metadata:` list, say — cannot overwrite the
  top-level `description`. Reading every line with a colon in it is how a skill ends up describing
  itself with a sentence from further down its own file.
- **The closing `---` is a line, not an occurrence.** A description containing `---` is fine; the
  header ends where the file says it does.
- **A quoted value is unquoted, and nothing else is interpreted.** `name: "notes"` is `notes`. There
  is no escape pass, because a name and a description are text the agent reads, not a format
  anything re-parses.
- **The first occurrence of a field wins.** A second `description:` further down does not replace the
  first.

## What the agent sees

The prompt gets a name, a description and a path for each skill — **never the body**. The agent reads
the file itself, at that path, when it decides the skill is relevant. This is progressive
disclosure, and it is the whole reason a dozen skills can be listed at all: a dozen sets of
instructions would crowd out the conversation, while a dozen names and descriptions cost a paragraph.

It follows that the description decides more than the name. Write it as the condition under which
the skill applies.

## What gets reported instead of loaded

The loader does not fail loudly, and it does not fail silently either. Every entry it refuses comes
back with the path and a reason, and the panel lists them beside the ones that loaded:

| What you wrote | What comes back |
|---|---|
| a directory with no `SKILL.md` inside | reported: there is no `SKILL.md`, so it is not a skill |
| a file that is not `.md` | reported: not Markdown, so it cannot be a skill |
| a header that does not parse | reported, with the parser's reason |
| no `description`, or an empty one | reported: the agent has nothing to choose it by |
| a symlink | reported, **not followed** — "where did this come from" is a question a skill list has to answer |
| an entry that cannot be read | reported, with the OS error |

A skill that is skipped is not a skill that is broken: the others still load. An empty list beside a
populated list of refusals means "your skill has a typo", which is a different message from "no
skills yet".

## Checking your work

Open `http://127.0.0.1:4270/` and look at the Skills section. It is rendered on each request rather
than cached, so editing a file and reloading the page shows the change — this is the page you open
*after* editing a skill.

That page belongs to no app, so it lists the **user's** skills only. An app's own skill at
`<app>/.agent/skills/` is not there: one list of every app's skills would say each of them applies
everywhere.

## A worked example

A skill that carries a script it wants run:

```
~/.deepnest/skills/release-notes/
├── SKILL.md
└── notes.sh
```

```markdown
---
name: release-notes
description: Use when the user asks for release notes or a changelog entry.
---

# Release notes

Run the script beside this file:

    bash ~/.deepnest/skills/release-notes/notes.sh

Then summarise what it printed, grouped by the user-visible change each commit makes.
```

The path in the body is absolute on purpose: the agent is told where the file is, not where it is
relative to anything.
