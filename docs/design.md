# deepnest design

**Status: design of record for v1.** This document is the executable spec the code is written against. When it disagrees with anything else, the host's decision log wins; every decision below names where it came from.

Provenance marks used throughout:

- **`host <HH:MM>`** — the host decided it on 2026-10-30 at that time. Not open for redesign.
- **`impl, ref: <path>`** — decided while writing this document, with the EMRG source named as the precedent. Changeable if the cited code argues otherwise.

---

## 1. What v1 is

A local-first app generator: one self-contained binary (~140 MB) that opens a page in the browser, where each generated app is a directory plus a dedicated agent. **v1 delivers the full agreed boundary in one release** (`host 20:42`), not a minimal loop with later stages.

Three platform targets ship together: macOS arm64, Windows x64, Linux x64 (`host 20:56`). Platform backends are abstracted from the first line — no macOS-shaped assumptions in shared code.

## 2. Process topology

**One listening port. Everything else is internal** (`host 20:45`).

```
browser ──► 127.0.0.1:4270  (the only port anyone knows about)
                │
                ├── /                      control panel
                ├── /app/<slug>/           app <slug>, proxied
                └── /api/…                 control panel's own API
                        │
                        └──► 127.0.0.1:<ephemeral>   the app's own server
```

- The app's real listening port is allocated per start and never leaves the process. It is not stable, not user-visible, not persisted.
- The gateway injects the floating dialog into every `/app/<slug>/` response, so the dialog survives the agent rewriting the app's page wholesale. It also rewrites the app's CSP for exactly that reason.
- The gateway therefore owns: `<base href>` injection, absolute-path rewriting, `Location` header rewriting, cookie `Path` prefixing, WebSocket upgrade, and unbuffered SSE.

Single-instance admission is **the bind itself** on the fixed loopback port — the kernel refuses the second bind, so there is no PID file to forge and no race (`impl, ref: emrg/server/daemon.py::_create_fixed_port_socket`). On POSIX set `SO_REUSEADDR` and never `SO_REUSEPORT`; on Windows use `SO_EXCLUSIVEADDRUSE`. A bind failure with no live listener behind it is a TIME_WAIT remnant: retry bounded, do not fail.

A random token is written to `~/.deepnest/deepnest.token` (mode `0600`) and re-asserted on a keepalive so an external deletion self-heals (`impl, ref: emrg/server/daemon.py::serve`). Loopback is not a security boundary: the token plus `Host`/`Origin` validation is what stops CSRF and DNS rebinding.

### What the binary serves today `(decided in implementation, 2026-10-31)`

`crates/deepnest` is the binary, and it lands **before** `crates/apps` on purpose: five library
crates that read well and nothing that starts leaves the first integration failure to packaging,
which is the most expensive place to meet it. It answers three routes on the one port, and says
by name what it does not answer:

- `GET /` — the panel. Until `crates/web` exists it is a placeholder that reports what is
  configured and states plainly that the real panel is not here yet.
- `GET /api/health` — liveness as JSON. The one `/api/` route without the token check: it carries
  no user data, and making the simplest probe require a `0600` read is the wrong trade.
- `GET /app/<slug>/` → `404` naming `crates/gateway`. A bare `404` on a correctly spelled path
  reads like a bug in the path.
- Anything else → `404` JSON; a method other than `GET`/`HEAD` → `405` with `Allow: GET, HEAD`.

Three deviations from §2, each deliberate, each to be closed by the crate that owns it:

- **`SO_EXCLUSIVEADDRUSE` is not set on Windows.** The default — neither option — already
  refuses a second bind with `WSAEADDRINUSE`, which is the admission this needs. Setting it
  explicitly needs a raw winsock call, and that hardening belongs with the packaging work rather
  than being claimed here.
- **The HTTP layer is hand-written**, one request per connection: choosing the real server stack
  is `crates/gateway`'s decision, and this crate should not make it by accident.
- **Nothing opens the browser yet.** Opening a placeholder page on every start is worth exactly
  as much as the panel behind it.

`config.toml` is one `[llm]` table: `base_url`, `api_key`, `model`. §3 spells the second `key`;
both spellings are accepted, because the design and EMRG's own config name the same thing
differently and a user migrating one to the other should not learn it from a parse error. A
missing file is **first run** and the binary starts anyway; a file that exists and does not parse
**stops startup**, because a typo reported at the first request is a typo reported far from its
cause.

## 3. Data layout

```
~/.deepnest/
├── config.toml              model endpoint: base_url / key / model   (host 20:18)
├── deepnest.token           0600, per-run control panel token
├── skills/<name>/SKILL.md   installed skills                       (host 20:26)
├── skills/.state.json       managed: true ⇒ updater may refresh it
├── toolchain/               unpacked Rust + bun, first-run, atomically
├── apps/<slug>/
│   ├── src/                 the app's own source
│   ├── app.sqlite           WAL, busy_timeout, single writer
│   ├── target/              build output
│   └── .agent/
│       ├── history.jsonl    conversation, replayable per turn
│       └── memory/*.md      long-term memory + MEMORY.md index
└── logs/
```

Two rules carry weight here:

- **Agent state lives inside the app directory**, so deleting an app deletes its agent's history and memory with it, and backing an app up backs them up too (`host 20:58` — memory is part of "EMRG-level capability", not an optional feature).
- **The agent's history is not stored in `app.sqlite`.** That database belongs to the user's app, and a second writer on it is exactly the `database is locked` failure the WAL measurement already documented.

## 4. Modules

Each is a crate with its own tests. Nothing above them reaches into another's internals.

### `crates/llm` — model access `(host 20:18)`

OpenAI-compatible client against a user-supplied `base_url` + `key` + `model`. Streaming, retry with backoff, and **error degradation that does not misread a 400 as context-too-long** — that specific bug is in EMRG's history and must not be re-learned (`impl, ref: emrg/server/llm.py`). No vendor-specific branches.

### `crates/sandbox` — policy and its two enforcers `(host 19:03, 19:05)`

Three modes: `read-only`, `workspace-write`, `danger-full-access`. The fence is **OS-level**, not a path check inside the process: Seatbelt on macOS, `bwrap` on Linux, ACL + restricted token on Windows.

One function, `writable_roots()`, feeds both the file tools' refusal path and the OS profile, so the two can never answer differently (`impl, ref: emrg/sandbox/roots.py`). The writable set is exactly: the app directory, the toolchain directory, and the temp directory.

Escalation is a **jump table, one hop, per call**: the tool call carries the requested tier and a justification, the host approves it on the web page, and the session default never moves. `read-only` may jump straight to `danger-full-access`; `danger-full-access` has nowhere to go; a request that does not widen is refused (`impl, ref: emrg/sandbox/escalation.py`).

Protected files — unwritable in every mode, compared by file **identity** rather than by string: device+inode on unix, volume serial+file index on Windows — are `config.toml`, `deepnest.token`, and the skill state file (`impl, ref: emrg/sandbox/policy.py::same_file`). `std` has no stable Windows API for that pair, so the syscall is delegated to `same-file` rather than reimplemented.

The Windows backend is the largest one by a wide margin (EMRG: ~3.2k lines across ACL, FFI, restricted token, spawn). The `windows` crate removes the hand-rolled FFI, not the ACL or token logic. Size the schedule on that, not on the average backend.

### `crates/command_scan` — what a fence cannot see `(host 19:03)`

A shell-text scanner for the read-direction escapes and for the one prohibition the fence cannot express: **the agent must not stop or restart deepnest itself**. Tokenize the command, expand nested commands, recognize wrappers (`eval`, `-exec`, `builtin cd`), then judge whether a word sits in command position (`impl, ref: emrg/tools/command_scan.py`).

### `crates/tools` — the agent's hands `(host 20:20)`

`bash`, `read`, `write`, `edit`, `glob`, `grep`, `subagent`. Not a reduced set: the host decision is that the per-app agent has near-complete EMRG capability, including starting and stopping the app, reading its logs, driving a browser and taking screenshots. Browser driving and screen capture come **from the browser-harness skill, not from the core** (`host 20:23`, re-confirmed `host 20:48`) — the core only opens the app page in the default browser.

A shell result carries the sandbox verdict explicitly, distinguishes "the runner never started" from "the sandbox refused", kills the process group on timeout, and appends the escalation hint when it was refused (`impl, ref: emrg/tools/bash_tool_v2.py`).

### `crates/agent` — the tool loop

Turn loop, streaming assembly, tool dispatch, conversation persistence, memory writes. Memory is two layers: `history.jsonl` for the conversation, and frontmatter+Markdown entries with a `MEMORY.md` index of one line per entry (retire by `status`, never delete). Only the index goes into the prompt; the body is read by the agent with the read tool (`impl, ref: emrg/memory.py`, `emrg/session.py`).

The system prompt is **written from scratch**. It is where most of the agent's behaviour quality lives, and EMRG's cannot be copied — but the three failures EMRG already paid for are cheap to avoid: a compaction rule that counts characters rather than entries, a tool loop that terminates when its turn is cancelled, and a 400 that is not mistaken for an over-long context.

Four decisions were made while implementing it and are recorded here rather than in a commit message (`decided in implementation`, 2026-10-31):

- **The model is reached through a `Model` trait, not through `deepnest_llm::Client`.** The loop is then testable with no network and no HTTP adapter, and the adapter (which belongs to `crates/gateway`) is swappable. `Client` implements the trait, so the binary wires the real one in one line.
- **The trim budget is by characters and defaults to 120,000 per request**, with two repairs that a provider otherwise rejects outright: the kept tail must begin at a `user` message, and a `tool` result whose `assistant` call was trimmed away is dropped with it. A request that reports itself too long halves the budget and retries at most twice; a **refusal is never** retried that way.
- **Cancellation is a shared flag checked before every request and before every tool call in a batch**, so a cancelled turn runs no further command. The calls that did not run are recorded in `history.jsonl` as a note, so a reader can tell a cancelled batch from a model that asked for less.
- **The layout under `~/.deepnest/apps/<slug>/.agent/`** is `history.jsonl`, `memory/<id>.md`, `memory/MEMORY.md`, and `subagents/<NNNN>/history.jsonl` — one conversation file per subagent call, because a child appending to its parent's file is two writers on one conversation.

### `crates/skills` — Anthropic Agent Skills

Loader reads two roots (`~/.deepnest/skills/`, `<app>/.agent/skills/`) and both layouts (`<name>.md`, `<name>/SKILL.md`); a same-named agent skill shadows the user skill. Progressive disclosure: only `name` + `description` + `path` enter the prompt. A file with no `description` is not loaded (`impl, ref: emrg/skills/loader.py`).

The catalog that ships inside the binary is itself an ordinary skill file carrying a `skills:` list (`name`, `description`, `repo`, `install`, `dest`, `check`); `dest` points at `~/.deepnest/skills/`. `browser-harness` and `baoyu-design` are the first two entries (`host 20:26`). Installation state lives in `.state.json`: only `managed: true` entries may be refreshed by the updater; anything else is the user's and is never touched (`impl, ref: emrg/skills/installer.py`).

### `crates/apps` — app lifecycle

Template, build, start, stop, restart, logs, port allocation. The template injects **WAL + `busy_timeout` + single-writer discipline**; it is not left to each generated app to decide, because the measurement showed a second writer failing immediately at `timeout=0` and succeeding after 1817 ms with `busy_timeout=5000`.

Front-end build is bun; **no HMR**. Refresh is triggered by a "this version is done" signal, never by a file change, because a file change would refresh a half-written page (`host 18:49`, `impl`).

The template also pins the gateway contract: relative asset paths, and the app must tolerate running under a path prefix.

### `crates/gateway` — the single door

The router described in §2. Its hardest requirement is not routing but **not lying to the app**: header rewriting, stream handling and WebSocket upgrade have to be transparent enough that an ordinary axum or Express app runs unchanged under `/app/<slug>/`.

### `crates/web` — the control panel

First run: the model endpoint form. After that: app list, app creation, and the app page with the floating dialog. Escalation approvals are modal here, one hop at a time, and the approval itself is the audit record (`host 20:20`).

### `crates/packaging` — the single file

zstd-compressed toolchain payload unpacked on first run **atomically** — into a temp directory, then renamed, so a half-unpacked toolchain can never be mistaken for a complete one. C toolchain is not bundled: check for Xcode CLT / VS C++ Build Tools before the first build and guide the install.

macOS ships signed and notarized (`host 20:48`). Every Mach-O inside the bundled toolchain must be **signed before packing**, because first-run unpacking does not re-sign, and an unsigned child process is refused under hardened runtime. Windows ships unsigned (`host 20:56`) — SmartScreen will warn on first run, and both README and the first-run page say so plainly rather than teaching users to bypass security prompts blindly.

## 5. Interfaces that must not drift

Three boundaries carry most of the future cost, so they are fixed here:

```rust
trait SandboxBackend {              // darwin | linux | win32 — chosen at startup
    fn confine(&self, policy: &Policy, command: &[String]) -> Result<Confinement, SandboxError>;
}

trait Tool {                        // one implementation per tool; uniform refusal shape
    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult;   // synchronous; the caller owns the runtime
}

trait Model {                       // the loop's view of the model; Client implements it
    fn turn(&self, messages: &[Message], tools: &[ToolDefinition]) -> Result<AssistantTurn, LlmError>;
}

struct MemoryStore { /* entries + index; bodies never enter the prompt */ }
```

The app directory layout and the `/app/<slug>/` path contract are equally fixed: both are the things users' data and users' URLs sit on, so changing them later costs a migration.

## 6. Build order

One module per cycle, each landing with its tests:

1. `crates/llm` — streaming, retry, degradation.
2. `crates/sandbox` + `crates/command_scan` — three modes, three backends, one policy source.
3. `crates/tools` — the eight tools, all going through the fence.
4. `crates/agent` — the loop, persistence, memory.
5. `crates/deepnest` — the binary, plus the three-platform CI matrix. It comes **before** the app
   lifecycle so that "does this even start, on all three platforms?" is answered while the answer
   is still cheap. From here on every module either lands wired into the binary or says in its PR
   why it does not.
6. `crates/apps` — template, build, lifecycle, logs.
7. `crates/gateway` + `crates/web` — the single door, the panel, the dialog.
8. `crates/skills` — loader, catalog, installer.
9. `crates/packaging` — payload, first-run unpack, signing.

## 7. Non-negotiables

- Three-platform backend abstraction from the first line (`host 20:56`).
- Sandbox is OS-level in three modes, with per-call escalation (`host 19:03`).
- App directory layout and the single-port path contract (§2, §3).
- Every module lands with tests.
- `deepnest` is a single binary; apps are directories (`host 18:55`).

## 8. Not in v1

Remote relay for reaching local apps from outside (`host 18:49`), scheduled tasks, skill marketplace, multiple model providers beyond one OpenAI-compatible endpoint, sub-agent fan-out beyond the delegated `subagent` tool.
