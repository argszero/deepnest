# deepnest

**A local-first, single-file app generator.**

One binary, one double-click, one page in your browser. You describe the app you want; an agent writes it on your own machine — React front end, Rust back end, SQLite storage — and the page reloads while it is being written.

> **Status: early, and it starts.** Nothing is released yet. The design, the sandbox, the tool set, the agent loop and the binary are in the repository: `cargo run -p deepnest` serves `http://127.0.0.1:4270/` and writes its control token to `~/.deepnest/deepnest.token`. The app generator, the control panel, skills and packaging are still ahead.

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
| Payload size (zstd -19, 12 threads) | ~140 MB |
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

deepnest checks for them before the first build and walks you through installing what is missing, instead of failing halfway through compilation.

## The name

`deepnest` here has nothing to do with [Deepnest](https://deepnest.io), the open-source nesting (layout optimisation) tool for laser, plasma and CNC cutting, or its ports. Different project, different problem — this one generates applications.

## Documentation

English is the default for everything in this repository. A Chinese translation of this page lives in [README.zh.md](README.zh.md).

The design v1 is built against — module boundaries, data layout, the port model, and where each decision came from — is in [docs/design.md](docs/design.md) ([中文](docs/design.zh.md)).

## License

MIT — see [LICENSE](LICENSE).
