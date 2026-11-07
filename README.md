# deepnest

**A local-first, single-file app generator.**

One binary, one double-click, one page in your browser. You describe the app you want; an agent writes it on your own machine — React front end, Rust back end, SQLite storage — and the page reloads while it is being written.

> **Status: early, and it runs. `v0.1.1` is published for macOS arm64, Windows x64 and Linux x64.** [Download it](https://github.com/argszero/deepnest/releases/latest), or run it from source: `cargo run -p deepnest` serves `http://127.0.0.1:4270/`, opens that page in your desktop's browser, and writes its control token to `~/.deepnest/deepnest.token`. A model endpoint over `https` works, verified against this machine's own certificate store. **What the published build is not:** it is ad-hoc signed rather than Developer-ID signed, so macOS makes you open the first copy by right-click → **Open** ([why](#opening-a-macos-build)); the Windows executable is signed not at all, so SmartScreen warns once; and the agent's shell tool does not run on Windows yet ([why](#download)). The design, the sandbox, the tool set, the agent loop, the app generator, the control panel and the skill loader are all in the repository, and a source checkout runs on all three platforms today. The binary embeds a toolchain payload and unpacks it on first use, and `crates/bundle` lays out the `.app` and signs every Mach-O in it, which CI turns into a `hdiutil`-verified `.dmg`.

---

## The idea

**Every app is a directory.** Source, configuration, the SQLite database and build output live together under `~/.deepnest/`. There is no remote build server and no cloud account: everything is built on your machine, and your apps run there.

One thing does leave the machine, and it is worth saying plainly: the agent needs a model, and you supply it. What you type and the code the agent writes go to **the endpoint you configure yourself**. They go nowhere else.

**Every app has its own agent.** Inside the page runs a floating dialog. You tell it what the app should do; it generates and modifies the code; the browser view refreshes live. One app, one directory, one agent.

**deepnest itself is a single file.** Roughly 140 MB, self-contained. It carries what it needs to build your apps — the Rust toolchain, the JavaScript runtime — so that the first app can be built on a machine with nothing installed but the OS.

## "Single file" means deepnest, not your apps

This distinction matters and is easy to get wrong:

| | What it is |
|---|---|
| **deepnest** | one executable file, ~140 MB, double-click to run |
| **each app you generate** | a directory: source code, config, SQLite database, build artifacts |

The single file is the builder. The apps are ordinary project directories you can open, read, version and back up.

## Measured numbers

Measured on an Apple-silicon Mac while designing the packaging, not estimated:

| Measurement | Result |
|---|---|
| Payload size (zstd -19, single-threaded) | ~140 MB |
| First launch: unpack the bundled toolchain | 937 ms |
| Building a second app, reusing the shared build cache | 4.25 s |
| Building a second app with a cold cache | 13.2 s |
| Rebuild after a small edit | 0.5 s |
| Resident memory while idle | 5 MB |

Payload composition: a minimal Rust toolchain compresses from 486 MB to 112 MB, and the JavaScript runtime from 60 MB to 16 MB.

`xz` would have reached 59 MB, but took 55 s to compress and 2.9 s to unpack — rejected, because first launch has to stay around a second.

## System requirements

deepnest does not bundle a C toolchain, and will not: it is large, platform-specific, and better installed by the OS's own tooling. Apps built with the Rust/SQLite stack need to compile C.

- **macOS** — Xcode Command Line Tools (`xcode-select --install`)
- **Windows** — Visual Studio C++ Build Tools
- **Linux** — a C compiler and linker (`build-essential` on Debian/Ubuntu, `base-devel` on Fedora)

deepnest checks for them before the first build and walks you through installing what is missing, instead of failing halfway through compilation.

## Download

`v0.1.1` ships one file per platform, on the [releases page](https://github.com/argszero/deepnest/releases/latest):

| Platform | File | What it is |
|---|---|---|
| macOS arm64 | `Deepnest-0.1.1-arm64-mac.dmg` | ad-hoc signed, not notarized — read the next section before opening it |
| Windows x64 | `Deepnest-0.1.1-x64-windows.exe` | not signed at all — SmartScreen asks once; one file, nothing to install |
| Linux x64 | `Deepnest-0.1.1-x64-linux.tar.gz` | not signed at all; a tarball because that is what carries the executable bit — glibc, not musl |

A `.sha256` sits beside each download. Each carries the Rust toolchain and the JavaScript runtime, so the first app can be built on a machine with nothing installed beyond the OS and its C toolchain (see [System requirements](#system-requirements)).

The Linux download is a tarball holding one file, `deepnest`: extract it and run it. It is built against glibc, and the JavaScript runtime it carries is the glibc build, so a musl-only distribution such as Alpine is not covered yet.

**One thing the Windows build does not do yet.** The agent's `bash` tool refuses to run a command there. Windows confines a command with an ACL plan plus a restricted token, and the code that applies that plan is not in this build; a plan derived but never applied is no fence at all, so the call is refused rather than run unconfined. Everything else is the same as on macOS — the panel, the dialog, the file tools, and building and running apps, which deepnest spawns itself with the toolchain it carries.

Every download is built by CI and never from a laptop, and a `v*` tag is what builds, signs and attaches all of them. The Linux asset in `v0.1.1` was attached after its tag, because a workflow has to reach the default branch before it can be run at all: it was built from the commit the tag points at, with only that workflow added.

## Opening a macOS build

A build published here without a Developer ID is **ad-hoc signed**, not signed by an Apple identity. macOS will therefore refuse it on the first double-click, with a message that says the developer cannot be verified.

That is expected, and it is not a warning to ignore lightly — it is the one thing a signature buys and this project does not have yet. To open one anyway:

1. Right-click `Deepnest.app` (or the `.dmg`) and choose **Open**.
2. Choose **Open** again in the dialog that appears.

macOS remembers the decision for that copy. A build that has been signed with a Developer ID and notarized needs none of this and opens on a double-click; the release notes say which of the two a given download is.

## The name

`deepnest` here has nothing to do with [Deepnest](https://deepnest.io), the open-source nesting (layout optimisation) tool for laser, plasma and CNC cutting, or its ports. Different project, different problem — this one generates applications.

## Documentation

English is the default for everything in this repository. A Chinese translation of this page lives in [README.zh.md](README.zh.md).

The design v1 is built against — module boundaries, data layout, the port model, and where each decision came from — is in [docs/design.md](docs/design.md) ([中文](docs/design.zh.md)).

Writing a skill — the two directories, both layouts, the header, and what the loader reports instead of loading — is in [docs/skills.md](docs/skills.md) ([中文](docs/skills.zh.md)).

## License

MIT — see [LICENSE](LICENSE).
