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
  disk with every entry there that is not one, a form for the endpoint, a form for a new app, the
  five verbs per app, and the user-level skills the loader found with every entry it refused. Its
  facts are rendered into the markup, so the page is true with script off or with a script that
  failed to load; the app list is fetched, because a directory changes between two page loads, while
  the skills report is rendered, because this is the page a user opens after editing a skill.
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
  seconds on a warm cache, and holds the connection that asked for it. It answers `412` **before
  the first step** when the machine has no C toolchain — the check the README promises, made where
  the build is asked for rather than by a compiler a minute into it.
- `POST /api/apps/<slug>/chat` — one turn through that app's agent. `POST` and nothing else, and
  the body is `{"message": "…"}`: what the user typed belongs in a body rather than in a URL that
  is written into the browser's history and into every log of it. It is behind a capability but not
  the control token — the dialog that sends it is served into the app's own page, so a token
  written there is a token the app's script could spend; the page is served with a cookie scoped to
  this path instead (§4, `crates/web`). Three refusals, each a different next step: a body that is
  not a message is `400`, having no config file to read is `503`, and a config with no model yet is
  `412` — the first-run form is one request away.
- `GET` and `POST /api/apps/<slug>/escalation` — the question a turn is waiting on, and the answer
  to it. `GET` answers `{"pending": …}`: `null` when nobody is asking, otherwise the app, the mode
  asked for, the mode the session is in, and the justification the model gave. `POST` takes
  `{"approve": false, "reason": "…"}` and answers `409` when no turn is waiting, because a page
  that believed it had answered would tell the user they approved something that never happened.
  Its own cookie, minted apart from the chat one and scoped to its own path, so the capability to
  spend an agent turn cannot answer a widening request (§4, `crates/web`).
- `GET /api/skills/updates` — whether a newer release of any installed skill is published, one
  answer per skill the state file says deepnest may refresh. Behind the token, and deliberately
  **not** part of what `/` renders: the check is one request per skill to somebody else's server,
  so rendering it synchronously would let an offline machine hang the panel it is meant to open.
  The page renders the local report and its script fetches this a moment later, into a section the
  markup already carries — so a page whose script failed still shows everything else.
- `POST` and `GET /api/skills/refresh` — the step the update check's answer leads to. `POST` with
  `{"name": "…"}` runs the skill's own package and rewrites the file it publishes, then records the
  version the release check adopted with it; `GET` reports what the last refresh came to. It answers
  `202` rather than `200` because the publisher returns when that program decides to — longer than a
  page holds a request — so the page polls the same route; and `409` while one is already running,
  because the state file is one file and two writers would lose one of the records. Behind the token,
  and a `POST` rather than a `GET`, because it runs a program on the user's machine (§`crates/skills`).
- `GET /app/<slug>/…` and `HEAD` — the app itself, forwarded to the loopback port it is running
  on. Not behind the token: this is the page a browser navigates to, and a link that demands an
  `Authorization` header is a link that never opens. What bounds it is that the only thing it can
  reach is an app's own port on this machine. An app that is not running answers `503` naming the
  `POST` that starts it; one that vanished between the state check and the connect answers `502`
  and names the app's log; a slug that is not an app answers `404`. `POST` and `DELETE` to an app
  answer `405` with `Allow: GET, HEAD`, because this layer carries no request body and forwarding
  one without it would tell the app a request arrived that said nothing.
- Anything else → `404` JSON; a method it does not serve → `405` with
  `Allow: GET, HEAD, POST, DELETE`. A body sent to a route that reads none → `400` naming the
  three that do: answering "done" to content this server never looked at is worse than refusing
  it. `411` for a chunked body, because a body with no declared length has no bound, and
  `413` when a declared length is past `MAX_BODY` — that bound is about how much memory this
  process can be made to spend.

Two deviations from §2, each deliberate, each to be closed by the crate that owns it:

- **`SO_EXCLUSIVEADDRUSE` is not set on Windows.** The default — neither option — already
  refuses a second bind with `WSAEADDRINUSE`, which is the admission this needs. Setting it
  explicitly needs a raw winsock call, and that hardening belongs with the packaging work rather
  than being claimed here.
- **The HTTP layer is hand-written**, one request per connection — and it now lives in
  `crates/gateway`, which is where the choice was meant to be made. It moved because a forwarded
  answer carries the app's own content type and headers, so the response type has to be able to
  hold them; a type that can is the gateway's business, not the binary's. The binary re-exports it
  (`pub use deepnest_gateway::http`), so a name that reads `crate::http` still resolves.

**The panel opens in the desktop's browser** (`decided in implementation`, 2026-10-31). This is the
deviation the list above used to carry, now closed, and it is the second half of the README's first
sentence — one double-click, one page in your browser. Five decisions came with it:

- **No dependency for it.** Handing a URL to the desktop is one program per platform — `open` on
  macOS, `start` on Windows, `xdg-open` elsewhere — and a crate wrapping three lines would be a
  build input and a supply chain to audit in exchange for them. Windows reaches `start` through
  `cmd /C` and passes the empty title argument, without which `start` reads the URL as the window
  title and opens nothing at all.
- **It happens after the banner, and after the bind.** Everything the run has to say about itself —
  the address, the config and token paths, the logs, the toolchain — is printed and flushed first,
  so a desktop that takes its time cannot look like a start that printed nothing. The listener is
  already accepting when the desktop is asked, so the page a browser lands on is served rather than
  raced.
- **A second start opens the address it found.** A double-click with deepnest already running is a
  person asking for the page that is already being served, and printing that address to a terminal
  nobody is watching is how the second click comes to look like nothing happened.
- **A browser that will not open is one line, not a failed start.** The panel is this program's whole
  interface, so refusing to run because the desktop would not take a URL would take the interface
  away with it; the address is printed either way, which is all a session with no desktop can use.
  The command that failed is named the way a person would type it — `cmd /C start`, not `cmd`.
- **The switch is `--no-open`, and the opener is a seam.** The end-to-end tests start the real
  binary; without the flag they would open a browser window on every machine running the suite,
  three platforms at once in CI. The seam is what the unit tests answer instead of spawning
  anything, and what the tests assert on is the command each platform would run.

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

Escalation is a **jump table, one hop, per call**: the tool call carries the requested tier and a justification, the host approves it on the web page, and the session default never moves. `read-only` may jump straight to `danger-full-access`; `danger-full-access` has nowhere to go; a request that does not widen is refused, before the host is asked anything (`impl, ref: emrg/sandbox/escalation.py`). The question reaches the person through `GET`/`POST /api/apps/<slug>/escalation` — the route the dialog polls while a turn is waiting.

Protected files — unwritable in every mode, compared by file **identity** rather than by string: device+inode on unix, volume serial+file index on Windows — are `config.toml`, `deepnest.token`, and the skill state file (`impl, ref: emrg/sandbox/policy.py::same_file`). `std` has no stable Windows API for that pair, so the syscall is delegated to `same-file` rather than reimplemented.

The Windows backend is the largest one by a wide margin (EMRG: ~3.2k lines across ACL, FFI, restricted token, spawn). The `windows` crate removes the hand-rolled FFI, not the ACL or token logic. Size the schedule on that, not on the average backend.

### `crates/command_scan` — what a fence cannot see `(host 19:03)`

A shell-text scanner for the read-direction escapes and for the one prohibition the fence cannot express: **the agent must not stop or restart deepnest itself**. Tokenize the command, expand nested commands, recognize wrappers (`eval`, `-exec`, `builtin cd`), then judge whether a word sits in command position (`impl, ref: emrg/tools/command_scan.py`).

### `crates/tools` — the agent's hands `(host 20:20)`

`bash`, `read`, `write`, `edit`, `glob`, `grep`, `subagent`, `install_skill`. Not a reduced set: the host decision is that the per-app agent has near-complete EMRG capability, including starting and stopping the app, reading its logs, driving a browser and taking screenshots. Browser driving and screen capture come **from the browser-harness skill, not from the core** (`host 20:23`, re-confirmed `host 20:48`) — the core only opens the app page in the default browser.

Two of those are **seams rather than leaves**, and both exist because the thing they delegate to lives in a crate this one must not depend on: `subagent` needs a child loop (`crates/agent`) and `install_skill` needs a catalog, a state file and a publisher (`crates/deepnest`, which is the only crate that knows where the home is). Neither is offered until the caller supplies it — `registry::all` is the six leaves, `registry::with_seams` adds each seam it can honour or both — so the set the model is told about is always the set the dispatcher can run. And the prompt takes that list from the set itself (`registry::names`), because a hand-written sentence is a second list: until 2026-11-01 it named `subagent`, which the dialog did not wire, and left out `install_skill`, which it did, so a model that believed it was told "there is no tool called subagent" (`decided in implementation`).

A shell result carries the sandbox verdict explicitly, distinguishes "the runner never started" from "the sandbox refused", kills the process group on timeout, and appends the escalation hint when it was refused (`impl, ref: emrg/tools/bash_tool_v2.py`).

### `crates/agent` — the tool loop

Turn loop, streaming assembly, tool dispatch, conversation persistence, memory writes. Memory is two layers: `history.jsonl` for the conversation, and frontmatter+Markdown entries with a `MEMORY.md` index of one line per entry (retire by `status`, never delete). Only the index goes into the prompt; the body is read by the agent with the read tool (`impl, ref: emrg/memory.py`, `emrg/session.py`).

The system prompt is **written from scratch**. It is where most of the agent's behaviour quality lives, and EMRG's cannot be copied — but the three failures EMRG already paid for are cheap to avoid: a compaction rule that counts characters rather than entries, a tool loop that terminates when its turn is cancelled, and a 400 that is not mistaken for an over-long context.

Four decisions were made while implementing it and are recorded here rather than in a commit message (`decided in implementation`, 2026-10-31):

- **The model is reached through a `Model` trait, not through `deepnest_llm::Client`.** The loop is then testable with no network and no HTTP adapter, and the adapter (which belongs to `crates/gateway`) is swappable. `Client` implements the trait, so the binary wires the real one in one line — but its impl reads a stream, and a plaintext adapter cannot serve one, which is why the binary calls `Client::turn` instead (see the wiring above).
- **The trim budget is by characters and defaults to 120,000 per request**, with two repairs that a provider otherwise rejects outright: the kept tail must begin at a `user` message, and a `tool` result whose `assistant` call was trimmed away is dropped with it. A request that reports itself too long halves the budget and retries at most twice; a **refusal is never** retried that way.
- **Cancellation is a shared flag checked before every request and before every tool call in a batch**, so a cancelled turn runs no further command. The calls that did not run are recorded in `history.jsonl` as a note, so a reader can tell a cancelled batch from a model that asked for less.
- **The layout under `~/.deepnest/apps/<slug>/.agent/`** is `history.jsonl`, `memory/<id>.md`, `memory/MEMORY.md`, and `subagents/<NNNN>/history.jsonl` — one conversation file per subagent call, because a child appending to its parent's file is two writers on one conversation.

**The agent is in the binary, and it answers a message** (`decided in implementation`, 2026-10-31).
`POST /api/apps/<slug>/chat` reads `{"message": "…"}` from the body and returns the turn. Three
refusals come first, and each names a different next step: a body that is not a message (`400`), a
process with no config file to read (`503`), a config with no model in it yet (`412`, with the
first-run form one request away). Then `crates/deepnest::chat` assembles **one agent per app** — the
six leaf tools and the two seams, eight in all, over a `workspace-write` policy for that app, its
conversation and memory under `<app>/.agent/`, and a system prompt built from the real skill loader
— and hands it to the loop. A `subagent` call is a child of the same model and the same policy, with
its own conversation under `<app>/.agent/subagents/<NNNN>/` and neither seam of its own, so a child
cannot start a child.
Five decisions came with the wiring:

- **The HTTP adapter carries both schemes, and neither degrades into the other.** `crates/gateway::transport`
  implements `deepnest_llm::Transport` for `http://` over a `std::net::TcpStream` and nothing else,
  so `crates/llm` still owns no socket and stays testable with no network; `https://` goes to
  `native-tls`, which hands the trust decision to the platform — Security framework on macOS,
  SChannel on Windows, OpenSSL on Linux — so an endpoint is checked against the same store the
  user's browser checks it against. That store, rather than a root list bundled into the binary, is
  the decisive argument: a machine behind a corporate proxy has its proxy's certificate authority
  installed there, and a bundled list would refuse it and read as deepnest being broken.
  `HttpTransport` is the two together and is what the binary wires in, while each half refuses the
  other's scheme by name, because plaintext aimed at a TLS port is either silence or a key sent in
  the clear and neither is a fallback. **A refused certificate says which refusal it was**
  (`decided in implementation`, 2026-11-01): an untrusted certificate, a certificate for another
  name and an endpoint that cannot be reached are three messages with three different fixes, and
  since `native-tls` exposes no error kind — only a sentence — the split is read out of that
  sentence; where a platform does not name the mismatch, the message still says the certificate was
  refused instead of falling back to "connection failed". What the adapter still does not do is
  stream: `post_stream` stays a refusal, and a turn is still answered whole.
- **A turn is answered whole, not streamed.** `Client`'s `Model` impl goes through `chat_stream`,
  which this adapter cannot serve, so `crates/deepnest`'s `model::Endpoint` calls `Client::turn`:
  the same `AssistantTurn`, arriving all at once. What is missing is watching the answer being
  written, not the answer.
- **The endpoint is read per request, not held at startup.** `POST /api/config` can change it, and
  an agent assembled once would keep asking the old one until the next start.
- **A writing turn says `changed` and does not rebuild.** `bash` and `write`/`edit` set the flag, the
  reply carries it, and building stays the existing `POST /api/apps/<slug>/build` followed by
  `/restart`. A build inside this request would hold one connection through a minute of
  `bun install`, which is longer than any browser waits.
- **A refusal is the user's to fix, not this server's failure.** `Refused` is `422`, a model that
  could not be reached is `502`, a conversation file that could not be written is `500` — three
  different next steps, so three different statuses.

### `crates/skills` — Anthropic Agent Skills

Loader reads two roots (`~/.deepnest/skills/`, `<app>/.agent/skills/`) and both layouts (`<name>.md`, `<name>/SKILL.md`); a same-named agent skill shadows the user skill. Progressive disclosure: only `name` + `description` + `path` enter the prompt. A file with no `description` is not loaded (`impl, ref: emrg/skills/loader.py`).

**What has landed so far: the loader** (`decided in implementation`, 2026-10-31). `crates/skills` reads both roots and both layouts and returns the list the prompt is built from. Five decisions came with it:

- **The directory layout wins a collision inside one root, and the file it beat is reported.** `<name>/SKILL.md` is the layout that can carry the scripts and assets a skill refers to, so a skill being migrated into it keeps working mid-move. The losing file is a `skipped` entry, because a user editing `notes.md` while `notes/SKILL.md` is the one being read has nothing else to look at.
- **The app's skill shadows the user's, silently.** That is the design's direction, and it is the useful one: an app that needs a different `notes` skill should not have to rename the user's. A shadow is not a fault, so it produces no `skipped` entry.
- **Everything that is not a usable skill is reported with its reason.** The same shape `crates/apps` uses for a directory that is not an app, and for the same reason: one list would make "no skills yet" and "your skill's header has a typo" read the same, and the user's only clue to the second would be a prompt that seems emptier than it should be.
- **A symlink is reported rather than followed.** Following one would let a skill live somewhere the user never put it, and "where did this come from" is the question a skill list has to be able to answer.
- **The header is read line by line, and an indented line is nested, not a field.** A skill may carry a `metadata:` block, and a nested `description:` must not overwrite the top-level one — the failure EMRG already paid for (`impl, ref: emrg/skills/loader.py`). Reading lines also makes the closing delimiter a line rather than the first three hyphens anywhere, which is what lets a description contain them.

**Wired now, through the prompt and not through a route.** A skill reaches the agent through its system prompt and nowhere else, so there is no skills route and there should not be: the chat path calls this loader per turn and puts `name`, `description` and `path` in front of the model.

**The panel is where a user reads what the loader found** (`decided in implementation`, 2026-10-31). `GET /` renders the loader's own answer: one line naming the skills directory and how many loaded, then each skill's name and description, then every entry that was refused with its path and the reason. Still no skills route — a route would be an API for a fact one page needs. Four decisions came with it:

- **Rendered by the server, not fetched by the script.** This is the page a user opens *after* editing a skill file, so a round trip is precisely the part that would hide the edit. It is the opposite call from the app list, and for the opposite reason.
- **The user's skills only, and the page says so in as many words.** The loader is called with `None` for the app: this page belongs to no app, and one list of every app's skills would claim each of them applies everywhere. A skill inside an app is invisible here by design, which has to be stated or "my app's skill does not show up" is the next bug report about a deliberate read.
- **`crates/web` learns nothing about `crates/skills`.** The router hands the page one line and two `(label, detail)` lists — exactly the shape it already hands it the app line — and the page escapes every value at the point it writes it. The skills list is substituted into the page **last**, because a description containing `@@APPS@@` is a description and not a request to repeat the app count.
- **Loaded and refused are two lists, and the line between them is the summary.** "None yet" and "none usable" are different pages: a user whose only skill has a broken header is told the entry is not a skill, with the reason, rather than that they have no skills.

The catalog that ships inside the binary is itself an ordinary skill file carrying a `skills:` list (`name`, `description`, `repo`, `install`, `dest`, `check`); `dest` is spelled against deepnest's own home, so its `~` is `~/.deepnest` and `~/skills/` is the directory the loader reads — a literal `~/.deepnest/skills/` would fail under a relocated `DEEPNEST_HOME` and land where nothing loads it. `browser-harness` and `baoyu-design` are the first two entries (`host 20:26`). Installation state lives in `.state.json`: only `managed: true` entries may be refreshed by the updater; anything else is the user's and is never touched (`impl, ref: emrg/skills/installer.py`).

**What has landed since: the catalog and the state file** (`decided in implementation`, 2026-10-31). `catalog` holds the bundled list and reads it back; `state` records which installed skills deepnest may refresh. Four decisions came with them:

- **The catalog is an ordinary skill file, and that is the point.** The loader picks it up like any other skill, so the agent sees one line saying a catalog exists; the machine-readable part is a `skills:` block in the same header `frontmatter` already parses. The alternative — a second format with its own schema, its own reader and its own line in the prompt — is a whole mechanism to say what one file says, and the reference implementation's registry is that mechanism (`impl, ref: emrg/skills/registry.py`).
- **An entry needs a `name` and a `description`; the other four fields say how deepnest would fetch it.** Not every recommended skill can be fetched: `baoyu-design` is a folder of instructions with no published source, so its entry has no `repo` and the catalog calls it **recommended only** rather than inventing a source for it. An entry that names a `repo` and not an `install` is a mistake in the file and is reported — "I know where this lives and not how it gets here" is a gap, not a decision.
- **Nothing in the catalog is dropped in silence.** The reference implementation requires all six fields and skips an entry missing one (`impl, ref: emrg/skills/registry.py`); here a missing `description`, a `repo` without an `install` and an entry with no `name` are each a `Rejected` with its reason, because an entry that vanishes looks exactly like a catalog that never had it.
- **`managed` is the only gate, and it fails closed.** `State::managed` is the only list an updater is handed and there is no all-skills iterator to reach for by mistake; refreshing an unmanaged skill is refused rather than quietly promoting it; a state file whose entry has no `managed` field reads as unmanaged, because a file of unknown provenance is not permission; unknown keys survive a round trip so an older binary cannot delete what a newer one wrote. Both files are written through a temporary neighbour and a rename.

**The bundled catalog is a floor, not a source of truth.** `catalog::ensure` writes it only when the file is absent, which covers a fresh install and an upgrade whose file was deleted, and never overwrites a catalog the user curated.

**What has landed since: the floor is laid, and the catalog is offered** (`decided in
implementation`, 2026-11-01). `home::prepare` calls `catalog::ensure` beside `prepare_home`, and
`serve()` calls `prepare` — the one place a start already runs. `ensure` had no caller before, so a
machine that had never been given the file by hand had no catalog at all: every refresh was refused
with "the catalog names no skill called ...", and the two entries the binary ships existed only
inside the binary. `prepare` is a function rather than two lines in `main` so a test can ask what a
start leaves behind without starting one. Four decisions:

- **The panel lists what the catalog offers and this home does not have**, with one install press per
  entry deepnest can fetch. A catalog whose entries nobody is shown has only the loader's one line to
  recommend them with, and the loader's line is a sentence, not an action.
- **An entry deepnest cannot fetch says which field is missing, and gets no press.** `repo`,
  `install` and `dest` are named one at a time, so a hand-edited catalog that dropped one is a
  fixable line rather than a mystery; a press there could only run something the entry never
  described.
- **The offered rows are rendered by the server, like the installed ones**, because the catalog is a
  file the user edits; only the press is the script's, since a button without a script would be a
  claim the page cannot keep. An install and a refresh are one operation on the server —
  `installer::install` writes the file either way and reports which happened through `replaced` — so
  both presses share the refresh queue and poll instead of growing a second copy of each, and the
  outcome sentence takes its word from the press that was used.
- **A catalog line that will not parse is reported where unusable skills already are.** The same
  fact one level up: an entry that vanished without a word is indistinguishable from a catalog that
  never held it.

**The release check has landed, and its request goes through a seam of its own** (`decided in
implementation`, 2026-10-31). `crates/skills::release` answers "is there a newer release than the
one on disk?" — one request, one comparison — and `Releases` is the seam the request goes through.
It returns the answer's *body*, not a parsed release, for the same reason `Publisher` returns a
program's stdout: every way a body can be wrong is decided in code a test drives with a string. The
socket under it has landed since, and where it went is a paragraph of its own below. `release_url`
states the endpoint as part of the contract, so the transport that landed later had no shape left
to invent. Five decisions:

- **`GET` was not borrowed from the model client, and does not need to be.** The seam is this
  crate's own, returns `io::Result<String>` and takes an `owner/name` — a skills question in a
  skills vocabulary, as promised when the model client's `POST` seam was deliberately not reused.
- **Four answers, because "no" and "I could not tell" are not the same answer.** `UpToDate` (nothing
  to do), `Newer` (refresh and record what you adopted), `Unknown` (the newest release is known and
  what is installed is not — a refresh adopts it and becomes comparable), `CannotTell` (with a `Why`
  naming the missing piece). Collapsing the fourth into the first is how an update check silently
  stops offering updates; it is the branch that keeps a broken check from reading as a healthy one.
- **A version is dotted numbers behind an optional `v`, and three shapes are refused rather than
  guessed.** Date tags (`2026-10-31`) compared as numbers read as `2026 > 1`, so a repository tagging
  by date would offer an update on every check for ever; pre-releases need a rule set of their own
  and guessing wrong updates somebody to a beta they never asked for; and a recorded version the same
  rules reject is a file somebody edited. All three yield no update, which is the safe direction —
  installing by hand still works, and `CannotTell` says so.
- **Comparison is numeric per component, padding the shorter side with zeros.** `0.9` is older than
  `0.10` — a string comparison says the opposite, and that bug only appears at the tenth release.
  `1.0` equals `1.0.0`. A release *older* than what is installed is not an update: the state file can
  be ahead of the latest release when somebody installed from a branch, and stepping back is a
  downgrade nobody asked for.
- **A skill the user brought is refused before the network is asked.** `managed` is the gate in
  `state`, and here it is checked first, because there is nothing to ask about a file no update may
  touch. The check takes `Option<&Installed>` and tolerates `None`, so a caller cannot reach an
  unmanaged file by passing a different shape of argument.

**Why installing still records no version, and where one comes from.** `<program> skill` says
nothing about which version it is publishing, so `installer` records provenance and leaves `version`
absent rather than inventing a number the whole comparison depends on. A version arrives the only
way one can be known: the release check reads what the package published, and `Newer` and `Unknown`
are the two verdicts `adoptable()` returns — what a refresh adopts and then records through
`record_update`. That is also the path by which every skill installed before this slice becomes
comparable on its first refresh.

**What has landed since: the install itself** (`decided in implementation`, 2026-10-31). `installer` drives the one side effect the catalog describes — for `install: self-publishing`, it runs the package and takes the skill file it prints — and records the result. Five decisions came with it:

- **The publisher is the entry's own name, not a constant.** The reference implementation sets one `CLI_NAME` for every entry, so a second catalog entry would run the first one's program (`impl, ref: emrg/skills/installer.py`). Here it is the entry's `name`, and one that cannot be resolved is refused by name together with the repository it comes from — the repository URL is the fix, which is what a refusal owes the reader.
- **A file deepnest did not write is never overwritten, and both halves of provenance are consulted.** The state file says whether deepnest put this file there and the disk says whether anything is there at all; either alone is a way to lose somebody's work, because a state file can be missing and a file can be deleted. A name the state file records as the user's is refused even with an empty path, and a file the state file does not account for is refused as unknown provenance — the same reading a missing `managed` field gets. The reference writes `dest/<name>.md` over whatever is there.
- **The published header must declare the name the catalog asked for.** The reference writes the file under the *published* name and records state under the *catalog* name, so the update loop then looks up a skill that is not there under that name. Here a disagreement is a refusal naming both.
- **Nothing is recorded that is not on disk.** The file is written first and the state second, so an interrupted install leaves an unrecorded file — which the next install refuses to overwrite, the safe direction — rather than a managed entry pointing at nothing. `install` updates the state and the caller writes it, which keeps this crate's file-writing in one place.
- **No version is invented.** The reference records `version: "unknown"` when its release check fails; the state model already has the better answer for "the source did not say", which is an absent field, and a refresh keeps whatever version was there rather than clearing it. A real version comes from `release`'s check, which is the only place one can be learned.

The `Publisher` is a trait for the same reason `Runner`, `Spawner`, `Probe` and `Prober` are: every refusal worth testing happens *after* a program has run, and a test that ran a real one would measure whatever that program does on the machine running the suite. The real one waits as long as the package takes — a limit worth naming, and one whose fix goes behind this seam rather than into the logic.

**The install an *agent* runs came later, with the escalation prompt it needed** (`decided in implementation`, 2026-11-01). Installing is a side effect on the user's machine: it writes outside the app directory and can replace a file the user edited, which is exactly the kind of call the dialog is meant to ask about — so the tool arrived with that prompt, not before it. The version a *person* asks for came first: `POST /api/skills/refresh` runs the package from the panel's own button, where the person pressing it is the consent that prompt exists to obtain, and it is a route of its own rather than a call a turn can make. The version an *agent* asks for is `install_skill`: the tool's words are `crates/tools/src/skill.rs` (a seam, like `subagent`) and the wiring is `crates/deepnest/src/install.rs`. It is a widening request **by construction** rather than by policy — a skill lives in `~/.deepnest/skills/`, which is outside the three writable roots — so at `workspace-write` the call is refused in the policy's own words plus the mode that would reach it, and the same call sent with `request_mode` runs only after the person approved that one widening. That wiring is also where `install`'s debt was paid: its module doc says whoever wires the real publisher in owes it "either a worker or a timeout", the panel paid with a worker, and a turn pays with a deadline (`SpawnerPublisher`, the same `Spawner` `bash` runs commands through).

**What has landed since: the socket under the release check, and the route the panel reads**
(`decided in implementation`, 2026-11-01). `Releases` is implemented by
`crates/deepnest/src/updates.rs` over `crates/gateway`'s `get`, and it sits there rather than in
`crates/skills` because that is the one crate depending on both sides: the seam's own crate stays
transport-free, which is what its module doc asked for and what keeps it testable with no network.
Three decisions came with the wire.

- **The endpoint is a parameter, not a literal.** `release_url` stays the default, because the URL
  is part of the contract and carries a shape test of its own; but a check that could only ever
  reach `api.github.com` is a check no test can drive, and the honest alternatives are a fake seam
  that proves nothing about the transport, or a test that measures GitHub. `Github::at` is the same
  decision `TlsHttp::with_connector` made: open the one thing a test has to control.
- **A status that is not `200` is an error, never an empty body.** An empty body would arrive at
  `release::check` as `NotARelease` — true but vague — while a `404` is a real answer with a real
  meaning: the repository publishes no releases. Reported as an error it lands in `Why`'s
  `Unreachable`, where the detail is the status and a person can read it.
- **The check never runs while a page is being rendered.** `report` is called from a route the
  panel's script fetches on its own, never from the synchronous skills report, which reads the disk
  and renders into the markup: one TLS request per skill inside that path would let an offline
  machine hang the panel it is meant to open.

**What has landed since: the refresh the answer leads to** (`decided in implementation`, 2026-11-01).
`crates/deepnest/src/refresh.rs` calls `installer::install` and then, once the file is on disk,
`release::check` — recording what `adoptable()` returned through `record_update` and writing the state
file. It is the first caller `install` has ever had. Four decisions came with it:

- **A refresh enqueues and returns.** The real publisher is the package run with no timeout, so a
  `POST` that waited on it would hold one connection for as long as somebody else's program takes.
  The route answers `202` and a worker thread of its own does the work — the shape `crate::rebuild`
  already uses — and the page polls the same route for the outcome.
- **One refresh at a time, refused rather than queued.** The state file is one file: two workers each
  read it, each record their own skill, and the second write silently loses the first one's record.
  A second request while one runs is `409` naming the skill in flight.
- **The file first, the version second.** `install` records no version and keeps whatever was there,
  so a version can only come from the check — and it is asked *after* the rewrite, because the file
  is what the user asked for and a forge that cannot answer must not be able to cancel it. A check
  that cannot answer leaves the refresh standing with the version it had, and `None` when it had none.
- **It is the user's refresh, not the agent's.** The button is the consent the dialog exists to
  obtain, which is what let this land before the escalation prompt did. A turn reaches `install`
  only through `install_skill`, and only for the one call the person widened.

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

The stop path (`decided in implementation`, 2026-11-01). A stop request — Ctrl-C, `kill`, or a console close — is recorded by a handler that does nothing but set a flag, and the thread that waited on that flag stops the listener before it stops the apps, because requests are what start apps. The listener's close is bounded, so a stream that is saying nothing cannot hold the exit open; the apps go one at a time after it; and the process exits `0`, because a stop that was heard is not a failure. The sequence lives in `crates/deepnest`, beside the supervisor it runs down — not in `crates/gateway`, which serves the door and knows nothing about apps. What no handler can hear is `SIGKILL`: a deepnest killed that way still leaves its apps running, and that is the one case where an app is reachable only by pid.

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

**What the page looks like is part of what an app is** (`decided in implementation`, 2026-11-01). Running deepnest and looking at a generated app made the gap plain: it worked and it looked like a wireframe, and every app after it would look the same, because the two things that decide an app's appearance — the starting page and the instructions the app's agent follows — carried no design content at all. Both now do, and three decisions came with it:

- **The look is one token block, not a stylesheet.** `frontend/index.html` declares the palette, the spacing step, the radius and the elevation as custom properties on `:root`, with a dark set under `prefers-color-scheme` and `color-scheme: light dark` so the scrollbar and native controls follow it. A page whose colours are scattered through its rules is a page nobody can restyle, and this one exists to be restyled.
- **The empty state is the design, not an edge case.** A new app's first screen is an empty list on a day-one database, so it gets a shape and a sentence naming the next action rather than blank space; the failure state carries `role="alert"`, because colour alone is not a state a screen reader reaches. The stylesheet and the component are both checked: a class the component never uses is a stylesheet that is dead on arrival.
- **The standard is stated twice on purpose.** The template can only fix the first render; the agent is what rewrites the page afterwards. A rule that lives only in the template stops existing at the first change, so the prompt carries the same one — build on the tokens, four states, one emphasis, and add no component library, web font or icon set unless the request names one, each being a dependency in an app that has to build on the user's own machine.

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

The dialog's own assets are `crates/web`'s, served at `/dialog.js` and `/dialog.css`, and **it carries no control token**: the app's own script shares this origin, so a token written into an app's page is a token the app could read and spend. Everything else it shows is either in the markup the gateway wrote or is the panel the
browser loads itself.

**The message box sends on a capability scoped to one route** (`decided in implementation`,
2026-10-31). `POST /api/apps/<slug>/chat` wants an `Authorization` header, and the dialog has no
token to offer — handing it one would hand it to the app's own script as well. So serving an app's
page mints a cookie instead (`crates/deepnest::grant`), and the router honours it on that one route
and nowhere else. Six decisions:

- **`HttpOnly`, because the value must not be readable even by the page it serves.** The app's
  script runs same-origin with the dialog, so a value it can read is a value it can copy into a
  request of its own, or out of the page entirely.
- **`SameSite=Strict`, because another origin must not be able to spend it.** A page on some other
  site cannot make a request that arrives carrying it, so a link or a form posted from elsewhere
  reaches this route holding nothing.
- **`Path` names the app's chat route, not the app's prefix.** This is a different cookie from the
  ones `crates/gateway`'s rewrite confines to `/app/<slug>/`, and the difference is the point: a
  browser will not attach a prefix-confined cookie to `/api/apps/<slug>/chat`, so the narrower path
  needs its own format rather than a reuse of that one.
- **In memory, eight hours, and no `Max-Age`.** A session cookie, so closing the browser is enough
  to be rid of it; the lifetime is enforced on the way back in, values are pruned on every mint, and
  the far end is named rather than silent — a `401` tells the box the permission expired and to
  reload, and reloading mints another.
- **Only a `text/html` answer gets one.** The dialog is injected into pages, so an answer that is
  not one has no box to send from, and a value minted for it would be a capability nobody holds.
- **A page that still holds a usable value is not given another.** Minting on every load would churn
  the store and hand a second tab's value to the first, so a request already presenting a live grant
  keeps it.

What this does not fix, said plainly rather than left to be discovered: the app's own script shares
this origin, so it can spend that app's agent turns — that cannot be prevented while the page
belongs to the app. What it cannot do is read the value, use it for another app, reach any route
other than that one, change config, or start or stop anything. No grant opens those.

What is **not** in this slice, each named because each is a different piece of work:

- **No `POST` to an app.** A body cannot be forwarded by a layer that never reads one, and forwarding the request without it would report success for content the app never saw.
- **A stream that ends while the browser lingers holds its thread** until the browser closes its socket. The bound is `MAX_CONNECTIONS`; the alternative — a timeout on an idle WebSocket — would break the one connection type this slice exists for.

The forward is a `Forwarder` trait, for the same reason `Spawner` and `Prober` are: what the router has to get right is *which app a request is sent to*, and a test of that decision should not depend on the kernel handing a real app the port the supervisor allocated.

### `crates/web` — the control panel

**Landed**: the panel itself — the endpoint state and its form, the app list including the entries
that are not apps and why, app creation, the five verbs per app, the skills the loader found with the
entries it refused, and an `Open` link to `/app/<slug>/`. One HTML page, one stylesheet, one script:
no CSS framework, no icon set, no bundler, because a panel that needs a build step is a panel nobody
reads in the repository.

The app page's floating dialog is this crate's bytes: `dialog.js` draws the ball and `dialog.css`
isolates it from whatever stylesheet the app ships (`all: initial` on every selector, so a reset
written for a different page cannot hide it). The split with `crates/gateway` is deliberate — the
gateway decides *where* the dialog goes and what policy it needs, which is rewriting, while the ball
is a page a person clicks in, which is this crate's.

**The message box is in** (`decided in implementation`, 2026-10-31), on the capability described
above: the ball opens a panel with an input, a send button and the answer. Three things it gets
right:

- **The answer is text, never markup.** It is model output, and a refusal's message is no more
  trustworthy than an answer is; the box sets `textContent`, and the slug it posts to is read as a
  directory name for the same reason.
- **Every failure names its next step.** An expired permission says to reload, because reloading is
  what mints another; an unreachable server says deepnest may not be running; a refusal reports what
  the route said. A box that simply went quiet would leave the user guessing which of the three
  happened.
- **Still no token.** The credential is the cookie the page was served with. The script never reads
  `document.cookie` and never touches the panel's `<meta>`, so a page whose own agent rewrote it
  cannot get a token out of the box either.

**The escalation prompt is in that dialog** (`decided in implementation`, 2026-10-31). One hop, one
call, approved by a person — and the person is looking at this page. Five decisions carry it:

- **The request rides in the call that needs the room.** `request_mode` and `justification` are two
  argument keys on the tool call, read and removed before the tool sees them. A tool of its own would
  ask for room without naming what for, and a separate round trip would spend a model call to say
  what the call already says. The loop checks the jump table **before** anybody is asked, so a
  request that does not widen never becomes a question — a host shown "widen workspace-write to
  read-only?" is being asked to approve a narrowing, and a host asked enough of those stops reading.
- **The turn waits, the page polls.** `GET /api/apps/<slug>/escalation` is what the dialog asks while
  a message is in flight; `POST` to the same path is the answer. The wait is bounded at two minutes,
  and what comes back when it runs out is *nobody answered* rather than *no* — the model's next move
  differs, and so does what the user should do about it.
- **One call, and the session does not move.** The approval produces a `Grant` for that call's
  context alone; the session's own policy is borrowed and never written to, and the tool result the
  model reads says both halves — the mode the call ran at, and the mode the session is still in.
- **A second cookie, not a wider one.** The page already holds a cookie scoped to the chat route; the
  question route gets its own value and its own path, so the capability to spend an agent turn cannot
  answer a widening request. A value names an app *and* a route, and the router checks both.
- **A refusal carries the person's words, and an approval carries none.** `Answer::Denied` is
  documented as "their words, if they gave any — shown to the model rather than summarised", and an
  empty reason is what makes a refusal unreadable to the model: it has been told no and told nothing
  about why, when the whole point of keeping `Denied` apart from `Unanswered` is that its next move
  differs. So the question block has a field for the reason, and it is optional: a person who just
  wants to say no says exactly that. The key is added only when refusing and only when the field is
  non-empty — an approval sends `{approve: true}` alone, because `Verdict` refuses a key it does not
  know and an approval has nothing to explain. The field is cleared with the question, so a reason
  typed at one request cannot answer the next one.

### `crates/packaging` — the single file

zstd-compressed toolchain payload unpacked on first run **atomically** — into a temp directory, then renamed, so a half-unpacked toolchain can never be mistaken for a complete one. C toolchain is not bundled: check for Xcode CLT / VS C++ Build Tools before the first build and guide the install.

**What has landed so far: the toolchain directory contract** (`decided in implementation`, 2026-10-31). `crates/packaging` now answers where an unpacked payload lives (`~/.deepnest/toolchain/<version>/`), what makes one usable, and what an unpack that was killed leaves behind. Four decisions came with it:

- **The staging directory is a sibling of the one it becomes, inside `toolchain/`.** Unpacking into the system temp directory is the obvious choice and the wrong one: publishing the result would then be a rename **across a filesystem boundary** whenever home and temp live on different volumes, and a rename across volumes is not a rename — it is a copy followed by a delete, whose interruption leaves exactly the half-unpacked directory this module exists to prevent.
- **A directory is usable because a marker says so, not because it exists.** `adopt` writes the marker inside the staging directory *before* the rename, so the published name appears only once its contents are complete; a directory whose marker is missing is reported as interrupted and never used. `crates/apps` makes the same move with `deepnest.toml`: a directory is what its marker says it is.
- **The marker names the payload, not the version.** A version directory can outlive the build that filled it — two development builds both call themselves `0.1.0`, and a restore can put an older one back over an upgrade — so a directory carrying a different payload is reported and replaced, where a bare version comparison would have called it ready and left the user running last week's compiler.
- **An interrupted unpack is swept, not resumed.** Half of one payload is not something to continue from; the process that owns the port removes the leftovers at startup and names them, because the port is the single-instance admission — it is what proves no other deepnest can be filling a staging directory at that moment.

**What has landed since: the C toolchain check** (`decided in implementation`, 2026-10-31). The README already promised that deepnest *checks for them before the first build and walks you through installing what is missing*; that promise is now kept, in [`cc`]. Four decisions came with it:

- **The answer is three readings, not a boolean.** Ready, missing, and *the check itself could not be answered* are separate because the actions are: build, install the named thing, or go and look at a broken probe. Collapsing the third into "missing" tells a user to install something they may already have.
- **Two of the three refuse the build.** An unanswerable check fails closed, like every other reading in this crate: a build started on "cannot tell" spends a minute of the user's time to arrive at the same refusal from a compiler, and says less when it gets there. The route answers `412` and reports that no step ran.
- **macOS asks `xcode-select -p`, not a fixed path.** The tools sit in `/Library/Developer/CommandLineTools` by default, a full Xcode puts them elsewhere, and `-s` can put them anywhere at all — a path check would answer "missing" for a machine that compiles C perfectly well, and a false "missing" stops a build that would have worked. The same reasoning makes Windows ask `vswhere` rather than look for `cl.exe` on `PATH`: outside a developer prompt those tools are never on it.
- **The fix is a command to paste or a page to open.** "Install a C toolchain" is not guidance; `xcode-select --install` is. This is the half of the promise that is about the user rather than about correctness, and a test keeps every platform's fix actionable.

**The unpack has landed; the payload and the signature have not.** `crates/packaging/src/unpack.rs` reads a tar inside a zstd stream into a `Staging` and publishes it through `Staging::adopt`, changing nothing in the contract above. Three decisions came with it, and they are about what a *malformed* payload may do rather than about an attacker — the archive is built by one step of the release pipeline and read by another, so the fault this defends against is a mistake in the packer:

- **A path is refused, not repaired.** An entry that is absolute or climbs with `..` fails the whole unpack with a named reason rather than being skipped or reinterpreted, and the toolchain directory is left exactly as it was. The `tar` crate's own `unpack_in` is not relied on for this: an independent check that fails closed with a reason a packer can act on is worth more here than a silent skip.
- **A link is checked where it is created.** A symlink is how a payload writes through a name, so its target is resolved lexically against its own entry's depth and must stay inside the toolchain root. Because a link that would escape never exists in the tree, no later entry can reach outside by writing through one, and the per-entry check is enough.
- **A payload makes files, directories and links, and nothing else.** A device node, a fifo or a socket is refused; nothing in a toolchain is one, and creating one needs a privilege the unpack should not hold.

**The payload's writer has landed too** (`decided in implementation`, 2026-11-01). `crates/packaging/src/pack.rs` is the other end of `unpack`: a toolchain tree in, the tar-in-zstd it reads out. Nothing new was invented — the reader was written first and its shape was the specification — so what the four decisions are about is what a *writer* owes a reader. Four:

- **A stream out, never a `Vec`.** The payload is the largest thing deepnest carries, and a `pack` returning bytes would hold a second copy of the whole toolchain in memory at the moment the machine is already busy. The caller passes a writer.
- **The same tree packs to the same bytes.** Entries are sorted and the header's time and ownership fields are zeroed, because a packer that emitted `read_dir` order with each file's own mtime would turn an unchanged tree into a different archive on every build — and this archive is signed and shipped, so "the payload did not change" has to be something two builds can agree on.
- **What the reader refuses, the writer refuses first** — through the reader's own `checked_link`, not a second copy of the rule. The reader must refuse an escaping link because it cannot see who built the archive; the packer must refuse it because it can, at the one moment the fix is a line in a build script instead of a first run that ends in a sentence about a toolchain the user never asked about. The same holds for a fifo, a socket or a device node.
- **A tree deepnest already unpacked is refused, by the marker it carries.** `adopt` writes `payload`; an archive carrying one would contradict the id the next unpack is told to publish, and `adopt` would overwrite one of them without a word. Pointing the packer at `~/.deepnest/toolchain/<version>/` is the mistake that produces it, so the refusal names that file.

A hard link is packed as a file: it is a second name for the same bytes, and finding the pairs would mean an inode table this crate has no other use for. What unpacks is the same tree either way.

**The archive lives in the binary, and the first build unpacks it** (`decided in implementation`, 2026-11-01). `crates/deepnest/build.rs` embeds the archive the release pipeline produces and `crates/deepnest/src/payload.rs` unpacks it into `~/.deepnest/toolchain/<version>/` through the contract above; the build then resolves its programs there first. Four decisions:

- **A build with no archive embeds an empty one, and the binary says so.** `include_bytes!` needs a file at compile time, so the build script always writes one: the archive when `DEEPNEST_PAYLOAD` names it, zero bytes otherwise. CI and a development build carry nothing, and `payload::embedded()` returning `None` is that reading — the build then uses the machine's `PATH` exactly as before, rather than failing on a corrupt payload it never meant to ship. `DEEPNEST_PAYLOAD` naming an unreadable file **fails the build**, because a packager who meant to carry a toolchain must not find out from a user.
- **The unpack happens at the first build, not at startup.** Startup is already the moment the port is won and the panel is served; making every launch pay for an unpack the user may never reach would spend it for nothing. It also settles the ordering the packaging crate requires: a build is only started by a request, and a request can only arrive after the bind, so no second instance can contest the staging directory.
- **The payload's programs are under `bin/`, and that directory is prepended to `PATH`.** Prepending, not merging: a bundled toolchain has to win over one the machine happens to have, or a self-contained binary would build the user's app with whatever version was installed. A payload with no `bin/` leaves `PATH` alone instead of pointing it at a directory that does not exist.
- **The `Runner` is where the payload meets the build.** `payload::Toolchain` implements the same seam `crates/apps` already had — `StdRunner` gained a `path_prefix` and nothing else changed — so a build that cannot get its toolchain reports that as the step's own outcome, where the user is already reading, instead of as a missing program.

Still to come: **the signature over the archive**, plus signing every bundled Mach-O before packing.

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
   why it does not. **It hosts the agent now** (`decided in implementation`, 2026-10-31):
   `POST /api/apps/<slug>/chat` runs one turn through a real per-app agent, so the five crates
   underneath are reachable from a message rather than only from a test.
6. `crates/apps` — template, build, lifecycle, logs. **Landed**, in three slices: the directory
   contract (slug, manifest, discovery, log tail, `GET /api/apps`), the process lifecycle (port
   allocation, start/stop/restart), and the template and the build (`POST /api/apps/<slug>/create`
   and `/build`). All three are wired into the binary, so this is not a module that sits unread.
7. `crates/gateway` + `crates/web` — the single door, the panel, the dialog. **Landed**: the
   gateway's forward, the HTTP layer under it, the WebSocket upgrade, unbuffered streaming, the panel,
   the dialog injection with its CSP rewrite, and the escalation prompt inside that dialog — the
   request carried by a call, the page polling for it, and the answer widening exactly one call. The
   binary serves connections concurrently, which is what lets a turn wait while the page answers.
8. `crates/skills` — loader, catalog, installer. **Landed**: the loader (two roots, two
   layouts, shadowing, skip reporting, the prompt summary), the bundled catalog, the `.state.json`
   that says which skills may be refreshed, the install path for a self-publishing package, the
   release check with its own one-request seam, and the socket under that seam have landed — the
   socket is `crates/deepnest/src/updates.rs`, its route is `GET /api/skills/updates`, and the
   panel's script reads it there. **Acting on the answer has landed too**: `POST
   /api/skills/refresh` runs `installer::install` for one managed skill and then `record_update` with
   the version the check adopted, so every row the panel offers has a button (`crates/deepnest/src/refresh.rs`).
   What remained in this crate was the install an *agent* would run, and it has landed too:
   `install_skill` (`crates/tools/src/skill.rs`, wired in `crates/deepnest/src/install.rs`) reaches
   `installer::install` from inside a turn, and gets there the only way a write outside the app can —
   refused at the session's mode, and run only for the one call the person widened. Every part this
   crate was to have has now landed.
9. `crates/packaging` — payload, first-run unpack, signing. **In progress**: the toolchain
   directory contract has landed (where a payload unpacks, the marker that makes a directory whole,
   and the sweep that removes what an interrupted unpack left behind), so has the C toolchain
   check, wired into the `build` route as a `412` before any step runs, and so has the unpack
   itself (`crates/packaging/src/unpack.rs`): a tar inside a zstd stream, read entry by entry, with
   every path and every link target checked against the toolchain root before anything is written.
   It is not wired into the binary yet, and cannot be: there is no payload in the build for it to
   read. What remains is that payload and the signing of it.

## 7. Non-negotiables

- Three-platform backend abstraction from the first line (`host 20:56`).
- Sandbox is OS-level in three modes, with per-call escalation (`host 19:03`).
- App directory layout and the single-port path contract (§2, §3).
- Every module lands with tests.
- `deepnest` is a single binary; apps are directories (`host 18:55`).

## 8. Not in v1

Remote relay for reaching local apps from outside (`host 18:49`), scheduled tasks, skill marketplace, multiple model providers beyond one OpenAI-compatible endpoint, sub-agent fan-out beyond the delegated `subagent` tool.
