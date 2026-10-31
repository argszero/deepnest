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
which is the most expensive place to meet it. It answers the routes below on the one port, and says
by name what it does not answer:

- `GET /` — the panel. Until `crates/web` exists it is a placeholder that reports what is
  configured, how many apps are installed, and states plainly that the real panel is not here yet.
- `GET /api/health` — liveness as JSON. The one `/api/` route without the token check: it carries
  no user data, and making the simplest probe require a `0600` read is the wrong trade.
- `GET /api/apps` — the apps under `~/.deepnest/apps`, and every entry there that is not one, with
  the reason. Behind the token, because it names the user's apps and the paths they live at. It
  reads the directory per request, so an app that appeared a second ago is in the next answer
  without a restart.
- `POST /api/apps/<slug>/create | build | start | stop | restart` — the five state changes,
  behind the token like the listing. `POST` and nothing else, because a browser prefetches a
  `GET`, so a state change behind one is a change it makes by drawing a link. The slug is
  validated against the same rules that make it a directory name, and a directory that is not
  there is `404` — "no app named that" is a different answer from any failure to start one.
  `create` answers `201` and returns the directory and the file list, because the client's next
  question is always where it went. `build` is the slow one: it runs the toolchain, takes tens of
  seconds on a warm cache, and holds the connection that asked for it.
- `GET /app/<slug>/` → `404` naming `crates/gateway`. A bare `404` on a correctly spelled path
  reads like a bug in the path.
- Anything else → `404` JSON; a method it does not serve → `405` with
  `Allow: GET, HEAD, POST, DELETE`; a request that declares a body → `411`. Nothing in this build
  reads a body: answering "done" to a request whose content it never saw is worse than refusing
  it.

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
│   ├── deepnest.toml        name and template version — what makes it an app
│   ├── src/                 the app's own source
│   ├── app.sqlite           WAL, busy_timeout, single writer
│   ├── target/              build output
│   └── .agent/
│       ├── history.jsonl    conversation, replayable per turn
│       └── memory/*.md      long-term memory + MEMORY.md index
└── logs/<slug>.log          one log per app, read from its end
```

Three rules carry weight here:

- **Agent state lives inside the app directory**, so deleting an app deletes its agent's history and memory with it, and backing an app up backs them up too (`host 20:58` — memory is part of "EMRG-level capability", not an optional feature).
- **The agent's history is not stored in `app.sqlite`.** That database belongs to the user's app, and a second writer on it is exactly the `database is locked` failure the WAL measurement already documented.
- **A directory is an app because `deepnest.toml` says so** (`decided in implementation, 2026-10-31`). Treating every directory under `apps/` as an app has no way to report a broken one: it would be listed as an app whose source happens to be missing, and the failure would surface much later as a build error about a file nobody was editing. The manifest also carries the template version the app was created from — recorded now, because the cheap moment to start recording it is before there is anything to migrate.

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

**What has landed so far: the directory contract** (`decided in implementation`, 2026-10-31). `crates/apps` owns the slug, the manifest, discovery and the log tail, and answers `GET /api/apps` from the binary. It lands first because the lifecycle is written against this shape, and a lifecycle written against an unagreed shape is a lifecycle rewritten. Three decisions were made while building it:

- **The slug is validated against both namespaces it lives in, once.** It becomes a directory on three platforms and a path segment in `/app/<slug>/`, and the union of those rules is stricter than either. The alphabet is lowercase letters, digits and `-`; a name starts and ends with a letter or a digit; `CON`/`PRN`/`AUX`/`NUL`/`COM1-9`/`LPT1-9` are refused because Windows cannot create a directory with those names, and uppercase is refused because NTFS and a default macOS APFS are case-insensitive, so `Notes` and `notes` would be one directory. Normalization is never applied on the way in — no trimming, no case folding — because each of those would name a directory other than the one the user wrote.
- **Discovery reports what it could not use, by name.** `list` returns the apps and, separately, every entry that is not one with the reason. A single list would make "no apps yet" and "your app's manifest has a typo" the same reading, and the user's only clue to the second would be a panel that seems emptier than it should be. A symlink is reported rather than followed: following one would let a directory the sandbox never agreed to appear as an app, and the writable set is built from app directories.
- **The log tail is bounded by bytes.** Counting lines from the start means reading the whole file, and a build log is exactly the file allowed to grow without limit. The reader seeks to the last 64 KiB, drops the partial line the window starts in, and says on its own last line when earlier lines were left out — a silently partial tail reads as a short log.

**What has landed since: the process lifecycle** (`decided in implementation`, 2026-10-31). An app's port, the supervisor that starts it, and stop/restart. Four decisions came with it:

- **The port is allocated per start and never persisted.** The app asks the kernel for a free one, deepnest hands it over in `PORT`, and nothing remembers it afterwards. A stored port is a number that goes stale the moment something else takes it, and the value is invisible to the user either way — so the one place that could disagree is removed rather than reconciled.
- **"Started" means it answered, not that it was spawned.** A spawn succeeds and the process then dies on a missing database or a bind failure, which is the common case for a generated app; reporting that as started leaves the user looking at a page that never loads. `start` probes the port until something answers or the process exits, and each of those three outcomes is a different error: build it, read the log, or wait.
- **The supervisor is a seam, not a `Command::spawn`.** Spawning and probing are traits for the reason every other such seam here exists: the interesting behaviour is a state machine with timeouts, and testing it against real processes would make the suite fail in proportion to the CI machine's load rather than to the code.
- **A stop that stops nothing is a success.** The caller's intent is the postcondition — "it must not be running" — and a stop that failed because the app had already exited would report the wrong thing. A restart is a stop and a start rather than a signal, because the port is fresh and the coming-up process must not collide with the going-down one.

What the process slice does **not** do: an app process outlives a killed deepnest. There is no graceful shutdown path in this build to run the apps down on (the process waits on a park and ends by signal), so `Supervisor::stop_all` exists and is untested against a real exit; the shutdown sequence that calls it belongs to `crates/gateway`, with the real server loop.

**What has landed last: the template and the build** (`decided in implementation`, 2026-10-31). `create` writes an app's files; `build` runs the toolchain over them. They are two steps and not one because they fail differently: a create that stops halfway leaves a directory that is not an app — re-run it — while a build that fails leaves a source tree that is perfectly good and a toolchain that said no, and the only useful thing to hand the user is which step and its output.

On the create side, four decisions:

- **The manifest is written last.** It is what makes a directory an app, so a creation that fails halfway leaves a directory that is not one — the truthful reading, and the one that offers "create it again" as the fix. Writing it first would leave a listed app whose source is missing, which reads as a broken app and points the user at a file they never saw.
- **A non-empty directory is never overwritten.** An app directory holds the user's data, their database and their agent's conversation, and there is no undo. A create that clobbered those would be the most destructive thing this program could do, so it refuses, and says which directory it refused.
- **The slug and the display name are two names, and are not interchanged.** `Smoke & Test` is a fine name and is not a Cargo package, a directory or a URL segment. The slug is the machine name on every axis the directory contract already validated; the display name appears only in the manifest and in the HTML and JSX the template writes. The first version filled the `Cargo.toml` name from the display name, and the compiler rejected it — a real build caught in one run what no amount of reading the template would have.
- **The binary's path is one constant that both sides name.** [`process::EXPECTED_BINARY`] is `target/release/app` and the template's `Cargo.toml` declares its `[[bin]]` as `app` for exactly that reason. One of the two moving is a rename; both moving silently is a supervisor looking for a file that is not there, so each names the other in a comment and the build asserts the two agree.

On the build side, three:

- **The plan is data, not a shell script.** The steps come back as a list of (directory, program, arguments). A generated `#!/bin/sh` file would have been a line shorter and untestable, and would have been written for a shell that is not the one the platform runs.
- **The front end builds before the server.** The order is not load-bearing for linking — the server reads `frontend/dist` per request — it is chosen so that a build failing on the front end has not already spent a minute compiling SQLite.
- **Success means the binary is there.** Exit codes say the toolchain was happy; the supervisor's contract is a path, and a build reporting success without it turns into "nothing has been built yet" on the next start, one step away from the cause.

**Running a generated app is what found its second bug.** `bun build` writes `frontend/dist/main.js` and leaves `frontend/index.html` where the template put it, so a server that served only `dist` answered every asset and `404`'d the page itself. The server now looks in `frontend/dist` first and `frontend/` second. The template had been right about the contract — relative paths, a matching path prefix — and wrong about where the bundler puts the page, which is the class of mistake a compiling build does not catch and a running one does. The check was **throwaway** — generate an app, build it, answer a few requests, delete it — and stays out of the suite on purpose: a test that needs bun and a C compiler on all three platforms fails for reasons unrelated to this code. Measured once by hand: 2.1 MB at `target/release/app`, about 31 s.

An install with no apps has no `apps/` directory at all, and the listing reports that as "none yet" rather than as a fault. That is now the whole of `crates/apps`: the directory contract, the process lifecycle, the template and the build.

What none of it does: build in the background, or report progress while it runs. A build occupies the request that asked for it, and this binary serves one connection at a time, so the panel waits. Showing progress while the panel stays live belongs to `crates/gateway`, with the real server loop.

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
6. `crates/apps` — template, build, lifecycle, logs. **Landed**, in three slices: the directory
   contract (slug, manifest, discovery, log tail, `GET /api/apps`), the process lifecycle (port
   allocation, start/stop/restart), and the template and the build (`POST /api/apps/<slug>/create`
   and `/build`). All three are wired into the binary, so this is not a module that sits unread.
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
