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

- `GET /` — the panel (`crates/web`): the model endpoint that is configured or is not, the apps on
  disk with every entry there that is not one, a form for the endpoint, a form for a new app, and
  the five verbs per app. Its facts are rendered into the markup, so the page is true with script
  off or with a script that failed to load; the app list is fetched, because a directory changes
  between two page loads.
- `GET /panel.js` — the panel's script, from `crates/web`'s own file. A route rather than an inline
  block, so `/` can be served with `script-src 'self'`: a string that reaches the page from the
  filesystem — an app's name, a slug — cannot become code. The page carries the control token in a
  `<meta>` element. That costs nothing a process on this machine could not already read out of the
  `0600` file, and it saves the user from pasting a secret into a form to open their own panel.
- `GET /api/health` — liveness as JSON. The one `/api/` route without the token check: it carries
  no user data, and making the simplest probe require a `0600` read is the wrong trade.
- `GET` and `POST /api/config` — the model endpoint, behind the token. `GET` answers whether one is
  configured and its model name, never the key. `POST` is the first-run form and **the one route
  that reads a request body**: a key in a URL is written into the browser's history and into
  whatever logs the URL, which is the same reason the token travels in a header rather than in the
  path. The body is bounded (`MAX_BODY`, 16 KiB) and accepted only with a declared
  `Content-Length`. The file is written through `config::save` — `0600`, atomically, and after the
  same checks the binary applies at startup — so a value it would refuse to boot on is refused by
  the form instead. The running process picks the change up immediately: nothing else caches the
  config, so there is no restart to ask for.
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
- `GET /app/<slug>/…` and `HEAD` — the app itself, forwarded to the loopback port it is running
  on. Not behind the token: this is the page a browser navigates to, and a link that demands an
  `Authorization` header is a link that never opens. What bounds it is that the only thing it can
  reach is an app's own port on this machine. An app that is not running answers `503` naming the
  `POST` that starts it; one that vanished between the state check and the connect answers `502`
  and names the app's log; a slug that is not an app answers `404`. `POST` and `DELETE` to an app
  answer `405` with `Allow: GET, HEAD`, because this layer carries no request body and forwarding
  one without it would tell the app a request arrived that said nothing.
- Anything else → `404` JSON; a method it does not serve → `405` with
  `Allow: GET, HEAD, POST, DELETE`. A body sent to any route but `/api/config` → `400` naming the
  one route that reads one: answering "done" to content this server never looked at is worse than
  refusing it. `411` for a chunked body, because a body with no declared length has no bound, and
  `413` when a declared length is past `MAX_BODY` — that bound is about how much memory this
  process can be made to spend.

Three deviations from §2, each deliberate, each to be closed by the crate that owns it:

- **`SO_EXCLUSIVEADDRUSE` is not set on Windows.** The default — neither option — already
  refuses a second bind with `WSAEADDRINUSE`, which is the admission this needs. Setting it
  explicitly needs a raw winsock call, and that hardening belongs with the packaging work rather
  than being claimed here.
- **The HTTP layer is hand-written**, one request per connection — and it now lives in
  `crates/gateway`, which is where the choice was meant to be made. It moved because a forwarded
  answer carries the app's own content type and headers, so the response type has to be able to
  hold them; a type that can is the gateway's business, not the binary's. The binary re-exports it
  (`pub use deepnest_gateway::http`), so a name that reads `crate::http` still resolves.
- **Nothing opens the browser yet.** The panel is real now, so opening it on start is worth doing
  rather than pointless — but it is a decision about the user's desktop that this slice does not
  take. The URL is printed; `open`ing it is one line for whoever lands that.

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

**What has landed so far: the loader** (`decided in implementation`, 2026-10-31). `crates/skills` reads both roots and both layouts and returns the list the prompt is built from. Five decisions came with it:

- **The directory layout wins a collision inside one root, and the file it beat is reported.** `<name>/SKILL.md` is the layout that can carry the scripts and assets a skill refers to, so a skill being migrated into it keeps working mid-move. The losing file is a `skipped` entry, because a user editing `notes.md` while `notes/SKILL.md` is the one being read has nothing else to look at.
- **The app's skill shadows the user's, silently.** That is the design's direction, and it is the useful one: an app that needs a different `notes` skill should not have to rename the user's. A shadow is not a fault, so it produces no `skipped` entry.
- **Everything that is not a usable skill is reported with its reason.** The same shape `crates/apps` uses for a directory that is not an app, and for the same reason: one list would make "no skills yet" and "your skill's header has a typo" read the same, and the user's only clue to the second would be a prompt that seems emptier than it should be.
- **A symlink is reported rather than followed.** Following one would let a skill live somewhere the user never put it, and "where did this come from" is the question a skill list has to be able to answer.
- **The header is read line by line, and an indented line is nested, not a field.** A skill may carry a `metadata:` block, and a nested `description:` must not overwrite the top-level one — the failure EMRG already paid for (`impl, ref: emrg/skills/loader.py`). Reading lines also makes the closing delimiter a line rather than the first three hyphens anywhere, which is what lets a description contain them.

**Not wired into the binary, on purpose.** A skill reaches the agent through its system prompt, and the binary has no agent loop yet — so a route serving skills today would be an endpoint with no consumer, and one that reads a directory per request for a page nobody opens. The loader lands first because the agent is written against it, the way the app lifecycle was written against the directory contract.

The catalog that ships inside the binary is itself an ordinary skill file carrying a `skills:` list (`name`, `description`, `repo`, `install`, `dest`, `check`); `dest` points at `~/.deepnest/skills/`. `browser-harness` and `baoyu-design` are the first two entries (`host 20:26`). Installation state lives in `.state.json`: only `managed: true` entries may be refreshed by the updater; anything else is the user's and is never touched (`impl, ref: emrg/skills/installer.py`).

**What has landed since: the catalog and the state file** (`decided in implementation`, 2026-10-31). `catalog` holds the bundled list and reads it back; `state` records which installed skills deepnest may refresh. Four decisions came with them:

- **The catalog is an ordinary skill file, and that is the point.** The loader picks it up like any other skill, so the agent sees one line saying a catalog exists; the machine-readable part is a `skills:` block in the same header `frontmatter` already parses. The alternative — a second format with its own schema, its own reader and its own line in the prompt — is a whole mechanism to say what one file says, and the reference implementation's registry is that mechanism (`impl, ref: emrg/skills/registry.py`).
- **An entry needs a `name` and a `description`; the other four fields say how deepnest would fetch it.** Not every recommended skill can be fetched: `baoyu-design` is a folder of instructions with no published source, so its entry has no `repo` and the catalog calls it **recommended only** rather than inventing a source for it. An entry that names a `repo` and not an `install` is a mistake in the file and is reported — "I know where this lives and not how it gets here" is a gap, not a decision.
- **Nothing in the catalog is dropped in silence.** The reference implementation requires all six fields and skips an entry missing one (`impl, ref: emrg/skills/registry.py`); here a missing `description`, a `repo` without an `install` and an entry with no `name` are each a `Rejected` with its reason, because an entry that vanishes looks exactly like a catalog that never had it.
- **`managed` is the only gate, and it fails closed.** `State::managed` is the only list an updater is handed and there is no all-skills iterator to reach for by mistake; refreshing an unmanaged skill is refused rather than quietly promoting it; a state file whose entry has no `managed` field reads as unmanaged, because a file of unknown provenance is not permission; unknown keys survive a round trip so an older binary cannot delete what a newer one wrote. Both files are written through a temporary neighbour and a rename.

**The bundled catalog is a floor, not a source of truth.** `catalog::ensure` writes it only when the file is absent, which covers a fresh install and an upgrade whose file was deleted, and never overwrites a catalog the user curated.

**Still to come: the fetch itself.** Running a package that publishes its own skill file and comparing releases both need things the binary does not have yet — a subprocess for the first, an HTTP client for the second — and both belong behind the agent, which is the thing that would act on an install. The decision-then-side-effect boundary is drawn where the decisions are testable offline: what is installable, what may be refreshed, and what a failed install leaves behind are all answered above, and the two side effects are the installer's remaining half.

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

What none of it does: build in the background, or report progress while it runs. A build occupies the thread that asked for it — the panel's own tab waits, which is what a synchronous request means — while the rest of the door stays open. Reporting progress *to that tab* is a streaming answer, and the gateway now has one.

### `crates/gateway` — the single door

The router described in §2. Its hardest requirement is not routing but **not lying to the app**: header rewriting, stream handling and WebSocket upgrade have to be transparent enough that an ordinary axum or Express app runs unchanged under `/app/<slug>/`.

**What has landed: the forward, and the HTTP layer under it** (`decided in implementation`, 2026-10-31). `/app/<slug>/…` reaches the app's loopback port, and the app is told it is being served from `/`. Five decisions:

- **The prefix comes off the path and nothing else does.** The app sees `/api/items`; it never learns it lives under `/app/notes`. That is what makes an ordinary app work unchanged — it wrote `/api/items` and that is what arrives — and it is why the generated template's contract (relative asset URLs) is a contract at all.
- **The Host the app sees is its own port.** A framework that builds absolute URLs from the `Host` header would otherwise send the browser to the panel's port, where the same path means something else entirely.
- **A page gets a `<base href>` naming its prefix, unless it declared one.** Without it, `/app/notes` and `/app/notes/` resolve a relative `./main.js` to two different places. With it, the answer is independent of how the address bar spelled the request. A page that already declares a base has made a decision about its own URLs, and the forward does not overrule it.
- **`Location` and `Set-Cookie` are rewritten; nothing else is.** A redirect to `/login` leaves the prefix and lands on deepnest's own 404. A cookie with `Path=/` is sent to every route on the port, the panel's API included, so it is confined to the prefix — added when absent, because a cookie with no `Path` gets `/` from the browser anyway. Everything else is passed through untouched: a proxy that rewrites headers it does not understand is a proxy that breaks the app it was meant to leave alone.
- **A chunked body is decoded, not forwarded as framing.** A Node or axum app answers a page it did not measure with `Transfer-Encoding: chunked`; passing those bytes on with the header stripped would hand the browser the chunk sizes as content.

**An answer is not always a body** (`decided in implementation`, 2026-10-31). A WebSocket upgrade and an SSE stream both begin with a head and then continue for as long as the user is there, so the forward returns one of two things: a complete response, or the app's connection, copied as the bytes arrive. Four decisions:

- **The choice is made from the head alone.** `101` is a tunnel and `text/event-stream` is a stream; the body of either has no end, so reading on to find out would be reading until the browser gave up on it.
- **A streamed head is not rewritten.** Its `Transfer-Encoding: chunked` is the framing of everything after it, so those bytes go out exactly as the app wrote them. The rewrites above belong to answers with a body; a body that never ends has no `Location` or `Set-Cookie` to move, and a length this layer invented would cut the stream off.
- **A handshake is the one request whose hop-by-hop headers must survive.** `Connection: Upgrade` and `Upgrade: websocket` are the message, not metadata about it — an app that does not receive them answers `400` instead of `101`. So a handshake crosses whole (`Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Sec-WebSocket-Protocol`, `Origin`), the only header changed is `Host`, for the usual reason, and nothing is appended: `Connection: close` would end the upgrade before it began.
- **The connection is copied, not polled.** One thread per direction, each writing one socket and no other. A single thread would have to wait on both, and an upgraded connection is idle in both directions at once.

**What that forced on the server loop.** One connection at a time was the right size for a panel that answered one request and closed; it is the wrong size for a socket that stays open, because the accept loop — and the token keepalive with it — would sit behind the first tab that opened one. Each connection now runs on its own thread, bounded at 64 (a connection past the bound is refused by name, not queued), and `shutdown` closes the sockets instead of waiting for a stream that never ends. The end-to-end check for this is a browser-shaped one: an idle connection that has sent nothing must not delay the next request, and a `GET /app/notes/ws` that has upgraded must not stop `GET /api/health` from answering.

**The floating dialog is injected at the last hop, and the policy moves with it** (`decided in implementation`, 2026-10-31). `crates/gateway::dialog` adds the dialog to every `text/html` answer an app gives, and widens the app's `Content-Security-Policy` by exactly what the dialog needs to load. Four decisions:

- **The dialog belongs to deepnest, so it is injected where the agent cannot reach it.** An agent rewrites the app's page whenever the user asks for something, so anything it wrote into the page it can also delete. Injecting at the forward means the ball survives every rewrite — and the ball is the only way back to the control panel from a page the user is looking at.
- **A page with no policy is not given one.** An absent `Content-Security-Policy` restricts nothing, so writing one would only take things away. The rewrite is for apps that set a policy, which are the apps that are doing it right.
- **A directive that has to be invented carries what `default-src` already allowed.** `default-src 'unsafe-inline'` with no `script-src` permits inline script; answering it with `script-src 'self'` would narrow the app's policy while claiming to widen it. And `'none'` is replaced rather than joined: it is only legal alone, and a policy listing it with others is one browsers resolve by ignoring it.
- **Only a page that actually took the dialog has its policy moved.** A `text/html` answer whose body already carries one — an app that ships its own, or a second hop — keeps the policy it sent. Loosening a policy for a dialog the page does not carry is loosening it for nothing.

The dialog's own assets are `crates/web`'s, served at `/dialog.js` and `/dialog.css`, and **it carries no control token**: the app's own script shares this origin, so a token written into an app's page is a token the app could read and spend. That is also why the dialog fetches nothing — everything it shows is either in the markup the gateway wrote or is the panel the browser loads itself.

What is **not** in this slice, each named because each is a different piece of work:

- **No `POST` to an app.** A body cannot be forwarded by a layer that never reads one, and forwarding the request without it would report success for content the app never saw.
- **The shutdown sequence that runs the apps down** is still not written. The accept loop it belongs with now exists; the call that stops every app on the way out does not, and a killed deepnest still leaves its apps running.
- **A stream that ends while the browser lingers holds its thread** until the browser closes its socket. The bound is `MAX_CONNECTIONS`; the alternative — a timeout on an idle WebSocket — would break the one connection type this slice exists for.

The forward is a `Forwarder` trait, for the same reason `Spawner` and `Prober` are: what the router has to get right is *which app a request is sent to*, and a test of that decision should not depend on the kernel handing a real app the port the supervisor allocated.

### `crates/web` — the control panel

**Landed**: the panel itself — the endpoint state and its form, the app list including the entries
that are not apps and why, app creation, the five verbs per app, and an `Open` link to
`/app/<slug>/`. One HTML page, one stylesheet, one script: no CSS framework, no icon set, no
bundler, because a panel that needs a build step is a panel nobody reads in the repository.

The app page's floating dialog is this crate's bytes: `dialog.js` draws the ball and `dialog.css`
isolates it from whatever stylesheet the app ships (`all: initial` on every selector, so a reset
written for a different page cannot hide it). The split with `crates/gateway` is deliberate — the
gateway decides *where* the dialog goes and what policy it needs, which is rewriting, while the ball
is a page a person clicks in, which is this crate's.

**Still to come**: escalation approvals inside that dialog — modal here, one hop at a time, the
approval itself the audit record (`host 20:20`). It waits on an agent being in the binary at all:
until a tool call can ask for a wider tier, there is nothing to approve, and the dialog says what it
can rather than offering a button that does nothing.

### `crates/packaging` — the single file

zstd-compressed toolchain payload unpacked on first run **atomically** — into a temp directory, then renamed, so a half-unpacked toolchain can never be mistaken for a complete one. C toolchain is not bundled: check for Xcode CLT / VS C++ Build Tools before the first build and guide the install.

**What has landed so far: the toolchain directory contract** (`decided in implementation`, 2026-10-31). `crates/packaging` now answers where an unpacked payload lives (`~/.deepnest/toolchain/<version>/`), what makes one usable, and what an unpack that was killed leaves behind. Four decisions came with it:

- **The staging directory is a sibling of the one it becomes, inside `toolchain/`.** Unpacking into the system temp directory is the obvious choice and the wrong one: publishing the result would then be a rename **across a filesystem boundary** whenever home and temp live on different volumes, and a rename across volumes is not a rename — it is a copy followed by a delete, whose interruption leaves exactly the half-unpacked directory this module exists to prevent.
- **A directory is usable because a marker says so, not because it exists.** `adopt` writes the marker inside the staging directory *before* the rename, so the published name appears only once its contents are complete; a directory whose marker is missing is reported as interrupted and never used. `crates/apps` makes the same move with `deepnest.toml`: a directory is what its marker says it is.
- **The marker names the payload, not the version.** A version directory can outlive the build that filled it — two development builds both call themselves `0.1.0`, and a restore can put an older one back over an upgrade — so a directory carrying a different payload is reported and replaced, where a bare version comparison would have called it ready and left the user running last week's compiler.
- **An interrupted unpack is swept, not resumed.** Half of one payload is not something to continue from; the process that owns the port removes the leftovers at startup and names them, because the port is the single-instance admission — it is what proves no other deepnest can be filling a staging directory at that moment.

**Still to come: the payload itself.** Unpacking a zstd archive into a `Staging` and publishing it, the C toolchain deepnest deliberately does not bundle (README already promises the check before the first build, so that one is a promise to keep rather than a decision to take), and signing every bundled Mach-O before packing. All three are written against the contract above and change none of it: the unpack fills `Staging::dir` and calls `Staging::adopt`, and nothing else may write inside `toolchain/`.

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
7. `crates/gateway` + `crates/web` — the single door, the panel, the dialog. **In progress**: the
   gateway's forward, the HTTP layer under it, the WebSocket upgrade, unbuffered streaming, the panel
   and the dialog injection with its CSP rewrite are in, and the binary serves connections
   concurrently; the escalation prompt inside the dialog remains, and it needs the agent in the
   binary first.
8. `crates/skills` — loader, catalog, installer. **In progress**: the loader (two roots, two
   layouts, shadowing, skip reporting, the prompt summary), the bundled catalog and the `.state.json`
   that says which skills may be refreshed have landed; the installer that fetches what the catalog
   names — a subprocess for a self-publishing package, an HTTP client for release tags — remains.
9. `crates/packaging` — payload, first-run unpack, signing. **In progress**: the toolchain
   directory contract has landed — where a payload unpacks, the marker that makes a directory
   whole, and the sweep that removes what an interrupted unpack left behind. The zstd unpack, the
   C toolchain check and signing remain.

## 7. Non-negotiables

- Three-platform backend abstraction from the first line (`host 20:56`).
- Sandbox is OS-level in three modes, with per-call escalation (`host 19:03`).
- App directory layout and the single-port path contract (§2, §3).
- Every module lands with tests.
- `deepnest` is a single binary; apps are directories (`host 18:55`).

## 8. Not in v1

Remote relay for reaching local apps from outside (`host 18:49`), scheduled tasks, skill marketplace, multiple model providers beyond one OpenAI-compatible endpoint, sub-agent fan-out beyond the delegated `subagent` tool.
