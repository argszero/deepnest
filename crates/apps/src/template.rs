//! What a new app is made of: the files that turn an empty directory into an app.
//!
//! An app is a directory (slice 1) that deepnest can start (slice 2). This is the part that
//! produces one: a Rust server, a React front end, and the manifest that marks the directory as an
//! app at all.
//!
//! Five decisions are worth their own sentence, because each one is a thing a generated app could
//! reasonably have done differently:
//!
//! - **The manifest is written last.** It is what makes a directory an app, so a creation that
//!   fails halfway leaves a directory that is *not* an app — the truthful reading, and the one that
//!   offers "create it again" as the fix. Writing it first would leave a listed app whose source is
//!   missing, which reads as a broken app and points the user at a file they never saw.
//! - **A non-empty directory is never overwritten.** An app directory holds the user's data, their
//!   database and their agent's conversation, and there is no undo. A create that clobbered those
//!   would be the single most destructive thing this program could do, so it refuses and says which
//!   directory it refused.
//! - **SQLite's pragmas are injected, not left to the app to decide.** The measurement behind this
//!   is in `docs/design.md`: with `timeout=0` a second writer fails immediately, with
//!   `busy_timeout=5000` it succeeds after 1817 ms. Every app gets WAL and the timeout because the
//!   one that forgot would fail in the field, under two writers, long after the template was copied.
//! - **Assets are relative and the server matches any path prefix.** The gateway serves an app under
//!   `/app/<slug>/` and injects a `<base href>`; an absolute `/api/items` would leave the prefix and
//!   reach deepnest itself, which is the failure mode this contract exists to prevent.
//! - **The built binary lands where the supervisor looks for it.** [`crate::process::EXPECTED_BINARY`]
//!   is `target/release/app`, and the `Cargo.toml` here names its binary `app` for exactly that
//!   reason. One of the two moving is fine; both moving silently is not, so each names the other.
//!
//! What this does **not** do: it does not build. Creating an app writes text and nothing else, so
//! it needs no toolchain and cannot fail halfway through a compiler run — building is
//! [`crate::build`], a separate step the user triggers separately.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::manifest::{Manifest, FILE_NAME as MANIFEST_FILE};
use crate::slug::Slug;

/// The name a generated app's server answers to, relative to the app directory.
///
/// It is the last segment of [`crate::process::EXPECTED_BINARY`], written down here because the
/// `Cargo.toml` below has to spell it: a binary named anything else builds to a path the supervisor
/// does not look at.
pub const BINARY_NAME: &str = "app";

/// The files an app is created with, relative to its directory, in the order they are written.
///
/// The manifest is deliberately absent: [`create`] writes it last and this list is what a caller can
/// report as "here is what you got".
pub const FILES: &[&str] = &[
    "Cargo.toml",
    ".gitignore",
    "src/main.rs",
    "frontend/package.json",
    "frontend/index.html",
    "frontend/src/main.jsx",
];

/// An app that has just been created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    /// The app's directory.
    pub dir: PathBuf,
    /// Every file written, absolute, manifest last.
    pub files: Vec<PathBuf>,
}

/// Why an app could not be created.
#[derive(Debug)]
pub enum TemplateError {
    /// The directory already has something in it. Refused rather than merged: an app directory
    /// holds the user's data, and nothing here can tell their file from one of ours.
    Occupied { dir: PathBuf },
    /// A file could not be written.
    Io { path: PathBuf, source: io::Error },
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Occupied { dir } => write!(
                f,
                "{} already has files in it — refusing to overwrite an app directory",
                dir.display()
            ),
            Self::Io { path, source } => write!(f, "cannot write {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for TemplateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Occupied { .. } => None,
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// Create the app `slug` in `dir`.
///
/// `display_name` is what the panel and the page show; a blank one falls back to the slug, because
/// the manifest requires a name and the slug is the one string that is guaranteed to exist.
pub fn create(slug: &Slug, dir: &Path, display_name: &str) -> Result<Created, TemplateError> {
    if let Ok(mut entries) = fs::read_dir(dir) {
        if entries.next().is_some() {
            return Err(TemplateError::Occupied {
                dir: dir.to_path_buf(),
            });
        }
    } else if dir.exists() {
        // It exists and is not a readable directory: a file, or a directory this process may not
        // look into. Either way, writing an app there would be guessing.
        return Err(TemplateError::Io {
            path: dir.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "exists and is not a readable directory",
            ),
        });
    }

    let name = match display_name.trim() {
        "" => slug.as_str(),
        given => given,
    };

    let mut files = Vec::with_capacity(FILES.len() + 1);
    for (relative, text) in SOURCES {
        let path = dir.join(relative);
        write(&path, &substitute(text, slug.as_str(), name))?;
        files.push(path);
    }

    // Last, and on purpose: see the module docs. `save` creates the directory if it is still
    // absent, and renames into place, so a crash here leaves no half-written manifest.
    let manifest_path = dir.join(MANIFEST_FILE);
    Manifest::new(name)
        .save(dir)
        .map_err(|source| TemplateError::Io {
            path: manifest_path.clone(),
            source,
        })?;
    files.push(manifest_path);

    Ok(Created {
        dir: dir.to_path_buf(),
        files,
    })
}

fn write(path: &Path, text: &str) -> Result<(), TemplateError> {
    let io_error = |source| TemplateError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    fs::write(path, text).map_err(io_error)
}

/// The two per-app values the sources carry.
///
/// **The slug is the machine's name and the display name is the user's**, and they are not
/// interchangeable: a `Cargo.toml` package name has to be an identifier, so an app called
/// `Smoke & Test` would not compile if the display name were substituted there. A real build found
/// exactly that, which is why the two placeholders are separate rather than one clever one.
///
/// The display name is escaped because it lands in HTML and in JSX text, where the user's own
/// punctuation must not become markup.
fn substitute(text: &str, slug: &str, display_name: &str) -> String {
    text.replace("@@APP_SLUG@@", slug)
        .replace("@@APP_NAME@@", &escape_html(display_name))
}

/// Escape the five characters that would end the text they sit in.
///
/// The name is the user's own words and lands in HTML and in a Rust doc comment, so a name
/// containing `<` must not become markup in either.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Every file an app is created with, and its text.
const SOURCES: &[(&str, &str)] = &[
    ("Cargo.toml", CARGO_TOML),
    (".gitignore", GITIGNORE),
    ("src/main.rs", MAIN_RS),
    ("frontend/package.json", PACKAGE_JSON),
    ("frontend/index.html", INDEX_HTML),
    ("frontend/src/main.jsx", MAIN_JSX),
];

const CARGO_TOML: &str = r#"[package]
# The slug, not the display name: a package name has to be an identifier, and an app the user
# called "Smoke & Test" still has to compile.
name = "@@APP_SLUG@@"
version = "0.1.0"
edition = "2021"
publish = false

# The binary deepnest starts. The name matters: it builds to `target/release/app`, which is the
# path `deepnest apps` looks for before it reports "nothing has been built yet".
[[bin]]
name = "app"
path = "src/main.rs"

[dependencies]
# `bundled` compiles SQLite from source, so the app does not depend on a system library being
# present — the deepnest payload pins its own version of everything it links.
rusqlite = { version = "0.31", features = ["bundled"] }

[profile.release]
# A generated app is a local tool that gets rebuilt while its agent works, not a service fleet.
# Optimising it as hard as rustc can would spend the user's seconds on every change.
opt-level = 1

# An empty workspace root of its own. Cargo walks *up* from this package looking for a workspace,
# and an app is created under the user's home — `~/.deepnest/apps/<slug>` — where any `Cargo.toml`
# above it (a monorepo the user keeps their own projects in, a stray manifest in the home
# directory) would otherwise be adopted as this package's workspace and stop the build with
# "current package believes it's in a workspace when it's not". The empty table ends that search
# at the app, which is what makes an app buildable no matter what it was created inside.
[workspace]
"#;

const GITIGNORE: &str = r#"/target
/frontend/dist
/frontend/node_modules

# The database and its write-ahead log: this is the user's data, produced by running the app.
/app.db
/app.db-wal
/app.db-shm
"#;

const MAIN_RS: &str = r##"//! @@APP_SLUG@@ — the server half of this app.
//!
//! deepnest starts this binary with `PORT` in the environment and reads nothing else from it: the
//! port is allocated per start and never persisted, so it cannot be compiled in.
//!
//! Three things here are contracts with deepnest rather than choices this app makes:
//!
//! - **SQLite runs in WAL with `busy_timeout` and one writer.** Measured: with `timeout=0` a second
//!   writer fails immediately; with `busy_timeout=5000` it succeeds after 1817 ms. The timeout is
//!   the margin around a single writer, not permission to have two.
//! - **Every URL is relative.** deepnest's gateway serves this app under `/app/<slug>/` and injects
//!   a `<base href>`; an absolute `/api/…` would leave the prefix and reach deepnest itself.
//! - **The binary answers on the port it was given, and nothing else.** That is what "started"
//!   means to the supervisor: it probes the port, and a process that bound the wrong one looks
//!   like a process that never came up.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

fn main() {
    let port: u16 = match std::env::var("PORT").ok().and_then(|text| text.parse().ok()) {
        Some(port) => port,
        None => {
            eprintln!("app: PORT is not set — deepnest allocates it per start");
            std::process::exit(2);
        }
    };

    let db = match open_database() {
        Ok(db) => db,
        Err(error) => {
            eprintln!("app: cannot open the database: {error}");
            std::process::exit(1);
        }
    };

    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("app: cannot bind 127.0.0.1:{port}: {error}");
            std::process::exit(1);
        }
    };
    // Printed, not logged: deepnest appends this process's output to the app's log, and the first
    // line of a log that begins with the port it took is worth more than one that does not.
    println!("app: listening on http://127.0.0.1:{port}/");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(error) = serve(stream, &db) {
                    eprintln!("app: {error}");
                }
            }
            Err(error) => eprintln!("app: {error}"),
        }
    }
}

fn open_database() -> rusqlite::Result<rusqlite::Connection> {
    let connection = rusqlite::Connection::open(database_path())?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, text TEXT NOT NULL);",
    )?;
    Ok(connection)
}

/// This app's database, next to its source — never in the directory it happened to be started from.
///
/// The binary lives at `<app>/target/release/app`, so the app directory is three parents up. That
/// is derived from the executable rather than from the working directory because a binary someone
/// runs by hand must find the same data the one deepnest started does.
fn app_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("app"));
    exe.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn database_path() -> PathBuf {
    app_dir().join("app.db")
}

fn serve(mut stream: TcpStream, db: &rusqlite::Connection) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Headers are read to their blank line and then dropped: this app answers by path only.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/");

    let response = if !(method == "GET" || method == "HEAD") {
        (405, "text/plain; charset=utf-8", "this app answers GET and HEAD\n".to_string())
    } else if path.ends_with("/api/items") {
        match items(db) {
            Ok(body) => (200, "application/json", body),
            Err(error) => (500, "text/plain; charset=utf-8", format!("{error}\n")),
        }
    } else {
        match asset(path) {
            Some((content_type, body)) => (200, content_type, body),
            None => (404, "text/plain; charset=utf-8", "not found\n".to_string()),
        }
    };

    let (status, content_type, body) = response;
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if method != "HEAD" {
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()
}

/// The page and its assets, read from disk per request.
///
/// Read per request rather than embedded so that a rebuild of the front end shows up on the next
/// refresh without restarting the app — the design's "this version is done" refresh relies on it.
///
/// Two directories, in this order, because the bundler is not a static-site generator: `bun build`
/// writes `frontend/dist/main.js` and leaves `frontend/index.html` where it is. Serving only `dist`
/// answers every asset and 404s the page itself, which is exactly what the first version of this
/// did.
fn asset(path: &str) -> Option<(&'static str, String)> {
    let rest = path.trim_start_matches('/');
    let rest = if rest.is_empty() { "index.html" } else { rest };
    // A path that climbs, or names a dotfile, is refused before either directory is tried: the two
    // roots below are the only places a request may read from.
    if rest
        .split('/')
        .any(|part| part.is_empty() || part == ".." || part.starts_with('.'))
    {
        return None;
    }
    let app = app_dir();
    for root in ["frontend/dist", "frontend"] {
        let file = app.join(root).join(rest);
        if let Ok(body) = std::fs::read_to_string(&file) {
            let content_type = match file.extension().and_then(|text| text.to_str()) {
                Some("html") => "text/html; charset=utf-8",
                Some("js") => "text/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                Some("json") => "application/json",
                Some("svg") => "image/svg+xml",
                _ => "application/octet-stream",
            };
            return Some((content_type, body));
        }
    }
    None
}

fn items(db: &rusqlite::Connection) -> rusqlite::Result<String> {
    let mut statement = db.prepare("SELECT id, text FROM items ORDER BY id")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = String::from("[");
    for (index, row) in rows.enumerate() {
        let (id, text) = row?;
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"id\":");
        out.push_str(&id.to_string());
        out.push_str(",\"text\":\"");
        out.push_str(&escape_json(&text));
        out.push_str("\"}");
    }
    out.push(']');
    Ok(out)
}

/// Escape the two characters that would end a JSON string, plus the control characters JSON forbids
/// raw. Enough for text a user typed, which is all this table holds.
fn escape_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if (other as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", other as u32)),
            other => out.push(other),
        }
    }
    out
}
"##;

const PACKAGE_JSON: &str = r#"{
  "private": true,
  "scripts": {
    "build": "bun build src/main.jsx --outdir dist --minify"
  },
  "dependencies": {
    "react": "^18.3.1",
    "react-dom": "^18.3.1"
  }
}
"#;

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>@@APP_NAME@@</title>
<!-- The stylesheet is inline and the script is a relative path: one artifact to build, and it
     keeps working when deepnest serves this page under /app/<slug>/ with an injected <base>.

     This page is the first thing the user sees of the app, and it is the reference the app's agent
     reads before changing it, so it is a starting point that already looks deliberate rather than
     a wireframe: the token block below is the whole palette, rhythm and elevation. A new colour or
     a new gap that is not one of those variables is a second visual system in a page that can
     carry one. -->
<style>
  :root {
    color-scheme: light dark;
    --bg: #fbfbfd;
    --surface: #ffffff;
    --line: #e4e4ee;
    --text: #16161d;
    --muted: #62626f;
    --accent: #5b5bd6;
    --accent-soft: #ededfb;
    --danger: #b3261e;
    --danger-soft: #fbebe9;
    --radius: 12px;
    --step: 8px;
    --shadow: 0 1px 2px rgba(16, 16, 24, .06), 0 10px 30px rgba(16, 16, 24, .07);
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #0d0d11;
      --surface: #16161d;
      --line: #2a2a36;
      --text: #f2f2f6;
      --muted: #a2a2b2;
      --accent: #a6a6ff;
      --accent-soft: #21213a;
      --danger: #ff9c94;
      --danger-soft: #2e1b1a;
      --shadow: 0 1px 2px rgba(0, 0, 0, .5), 0 10px 30px rgba(0, 0, 0, .35);
    }
  }
  * { box-sizing: border-box; }
  html { -webkit-text-size-adjust: 100%; }
  body {
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font: 15px/1.6 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
    -webkit-font-smoothing: antialiased;
  }
  main {
    max-width: 38rem;
    margin: 0 auto;
    padding: calc(var(--step) * 8) calc(var(--step) * 3) calc(var(--step) * 6);
  }
  .head { display: flex; align-items: baseline; gap: var(--step); flex-wrap: wrap; }
  h1 { font-size: 1.6rem; line-height: 1.2; letter-spacing: -.021em; margin: 0; }
  .badge {
    font-size: .75rem;
    font-weight: 600;
    letter-spacing: .02em;
    color: var(--accent);
    background: var(--accent-soft);
    border-radius: 999px;
    padding: 1px 9px;
  }
  .lede { margin: calc(var(--step) / 2) 0 calc(var(--step) * 3); color: var(--muted); }
  .panel {
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--radius);
    box-shadow: var(--shadow);
    overflow: hidden;
  }
  .list { list-style: none; margin: 0; padding: 0; }
  .row { padding: calc(var(--step) * 1.5) calc(var(--step) * 2); border-bottom: 1px solid var(--line); }
  .row:last-child { border-bottom: 0; }
  /* The empty state is not an absence of design: for a new app it is the only screen there is,
     which is why it gets a shape and a sentence rather than a blank panel. */
  .empty { margin: 0; padding: calc(var(--step) * 5) calc(var(--step) * 2); color: var(--muted); text-align: center; }
  .problem {
    margin: 0 0 calc(var(--step) * 2);
    padding: calc(var(--step) * 1.25) calc(var(--step) * 2);
    border-radius: var(--radius);
    border: 1px solid var(--danger);
    background: var(--danger-soft);
    color: var(--danger);
  }
  :focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  @media (prefers-reduced-motion: no-preference) {
    .row { transition: background-color .12s ease; }
    .row:hover { background: var(--accent-soft); }
  }
</style>
</head>
<body>
<div id="root"></div>
<script type="module" src="./main.js"></script>
</body>
</html>
"#;

const MAIN_JSX: &str = r#"// This app's front end.
//
// Every URL is relative. deepnest serves an app under `/app/<slug>/` and injects a `<base href>`,
// so `./api/items` is this app's API while `/api/items` would be deepnest's own control port.
// That difference is the whole reason the gateway contract exists.

import { useCallback, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";

const API = "./api/items";

async function readItems() {
  const response = await fetch(API, { headers: { accept: "application/json" } });
  if (!response.ok) {
    throw new Error(`the app answered ${response.status}`);
  }
  return response.json();
}

function App() {
  const [items, setItems] = useState([]);
  const [problem, setProblem] = useState(null);

  const refresh = useCallback(() => {
    readItems().then(setItems).catch((error) => setProblem(error.message));
  }, []);

  useEffect(refresh, [refresh]);

  return (
    <main>
      <header className="head">
        <h1>@@APP_NAME@@</h1>
        <span className="badge">
          {items.length} {items.length === 1 ? "item" : "items"}
        </span>
      </header>
      <p className="lede">Running on this machine. Ask its agent for a change.</p>
      {problem !== null ? (
        <p className="problem" role="alert">
          Cannot read the list: {problem}
        </p>
      ) : null}
      <section className="panel" aria-label="Items">
        {items.length === 0 ? (
          <p className="empty">Nothing here yet. Ask the agent to add the first one.</p>
        ) : (
          <ul className="list">
            {items.map((item) => (
              <li className="row" key={item.id}>
                {item.text}
              </li>
            ))}
          </ul>
        )}
      </section>
    </main>
  );
}

createRoot(document.getElementById("root")).render(<App />);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-template-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn dir(&self) -> PathBuf {
            self.base.join("apps").join("notes")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn slug() -> Slug {
        Slug::parse("notes").unwrap()
    }

    #[test]
    fn a_created_app_is_an_app_the_listing_accepts() {
        let scratch = Scratch::new("create");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();

        let manifest = crate::manifest::load(&created.dir).unwrap();
        assert_eq!(manifest.display_name(), "Notes");
        assert_eq!(manifest.app.template, crate::manifest::CURRENT_TEMPLATE);
        for relative in FILES {
            assert!(
                created.dir.join(relative).is_file(),
                "{relative} was not written"
            );
        }
    }

    #[test]
    fn the_manifest_is_written_last_so_a_half_created_app_is_not_an_app() {
        // Written first, a creation that failed halfway would list as an app whose source is
        // missing; written last, the same failure lists as "not an app", which is what it is.
        let scratch = Scratch::new("manifest-last");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();
        assert_eq!(
            created.files.last().and_then(|path| path.file_name()),
            Some(std::ffi::OsStr::new(MANIFEST_FILE)),
            "{:?}",
            created.files
        );
    }

    #[test]
    fn a_directory_with_files_in_it_is_refused_rather_than_overwritten() {
        let scratch = Scratch::new("occupied");
        let dir = scratch.dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("app.db"), "the user's data").unwrap();

        let error = create(&slug(), &dir, "Notes").unwrap_err();
        assert!(matches!(error, TemplateError::Occupied { .. }), "{error:?}");
        // The point of the refusal: the file that was there is still there.
        assert_eq!(
            fs::read_to_string(dir.join("app.db")).unwrap(),
            "the user's data"
        );
    }

    #[test]
    fn an_empty_directory_that_already_exists_is_used() {
        let scratch = Scratch::new("empty-dir");
        let dir = scratch.dir();
        fs::create_dir_all(&dir).unwrap();
        assert!(create(&slug(), &dir, "Notes").is_ok());
    }

    #[test]
    fn a_blank_name_falls_back_to_the_slug() {
        // The manifest requires a name, and the slug is the one string that is guaranteed to exist.
        let scratch = Scratch::new("blank-name");
        let created = create(&slug(), &scratch.dir(), "   ").unwrap();
        let manifest = crate::manifest::load(&created.dir).unwrap();
        assert_eq!(manifest.display_name(), "notes");
    }

    #[test]
    fn a_name_with_markup_is_escaped_where_it_lands() {
        let scratch = Scratch::new("escape");
        let created = create(&slug(), &scratch.dir(), "<script>x</script>").unwrap();

        let page = fs::read_to_string(created.dir.join("frontend/index.html")).unwrap();
        assert!(page.contains("&lt;script&gt;"), "{page}");
        assert!(!page.contains("<title><script>"), "{page}");

        // The manifest keeps the name as the user wrote it: it is a display name, not markup.
        let manifest = crate::manifest::load(&created.dir).unwrap();
        assert_eq!(manifest.display_name(), "<script>x</script>");
    }

    #[test]
    fn the_display_name_never_reaches_a_place_that_needs_an_identifier() {
        // Found by building a generated app for real: `Cargo.toml` had
        // `name = "Smoke &amp; Test"`, and cargo refused to parse it. The slug is the machine's
        // name; the display name is the user's, and only the second may be free text.
        let scratch = Scratch::new("identifier");
        let created = create(&slug(), &scratch.dir(), "Smoke & Test").unwrap();

        let cargo = fs::read_to_string(created.dir.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"notes\""), "{cargo}");
        // The comment above the field mentions the name on purpose; the field itself must not.
        assert!(!cargo.contains("name = \"Smoke"), "{cargo}");

        let source = fs::read_to_string(created.dir.join("src/main.rs")).unwrap();
        assert!(source.starts_with("//! notes"), "{source}");
    }

    #[test]
    fn the_generated_crate_is_its_own_workspace_root() {
        // Found by running the first-use path for real: an app created under a home that sat
        // inside a Cargo workspace made cargo walk up to that workspace's manifest, and the build
        // stopped with "current package believes it's in a workspace when it's not" before a line
        // of the app was compiled. An app is created under the *user's* home, which is exactly
        // where an unrelated `Cargo.toml` above it is normal, so the app has to end the search.
        let scratch = Scratch::new("workspace_root");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();

        let cargo = fs::read_to_string(created.dir.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("\n[workspace]\n"), "{cargo}");
        // Empty, and a root: a member entry here would pull the user's own crates into the app.
        assert!(!cargo.contains("members"), "{cargo}");
        assert!(!cargo.contains("workspace = "), "{cargo}");
    }

    #[test]
    fn no_placeholder_survives_into_a_generated_file() {
        // A file written with one placeholder unfilled is a file with `@@APP_SLUG@@` in it, which
        // is the kind of thing that only shows up when someone reads the app.
        let scratch = Scratch::new("placeholders");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();
        for relative in FILES {
            let text = fs::read_to_string(created.dir.join(relative)).unwrap();
            assert!(
                !text.contains("@@"),
                "{relative} kept a placeholder: {text}"
            );
        }
    }

    #[test]
    fn the_generated_database_opens_in_wal_with_a_busy_timeout() {
        // The measured failure this prevents: two writers, `timeout=0`, the second one dies at
        // once. Every app gets the pragmas because the app that forgot fails months later.
        let scratch = Scratch::new("pragmas");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();
        let source = fs::read_to_string(created.dir.join("src/main.rs")).unwrap();

        assert!(source.contains("PRAGMA journal_mode = WAL"), "{source}");
        assert!(source.contains("PRAGMA busy_timeout = 5000"), "{source}");
    }

    #[test]
    fn the_generated_server_looks_for_the_page_where_the_bundler_leaves_it() {
        // `bun build` emits `dist/main.js` and leaves `index.html` in `frontend/`. A server that
        // only served `dist` answered every asset and 404'd the page — found by running one.
        let scratch = Scratch::new("roots");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();
        let source = fs::read_to_string(created.dir.join("src/main.rs")).unwrap();
        assert!(source.contains("\"frontend/dist\""), "{source}");
        assert!(source.contains("\"frontend\""), "{source}");
    }

    #[test]
    fn the_generated_frontend_uses_relative_urls_only() {
        // The gateway contract, checked where it is cheap: an absolute API path leaves the
        // `/app/<slug>/` prefix and would reach deepnest's own port.
        let scratch = Scratch::new("relative");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();

        let page = fs::read_to_string(created.dir.join("frontend/index.html")).unwrap();
        assert!(page.contains("src=\"./main.js\""), "{page}");

        let script = fs::read_to_string(created.dir.join("frontend/src/main.jsx")).unwrap();
        assert!(script.contains("\"./api/items\""), "{script}");
        assert!(
            !script.contains("\"/api/"),
            "an absolute API path would leave the app's prefix: {script}"
        );
    }

    #[test]
    fn the_generated_cargo_project_builds_the_binary_the_supervisor_looks_for() {
        // `process::EXPECTED_BINARY` is `target/release/app`; the `[[bin]]` name is what puts it
        // there. Spelling both here is what stops them drifting apart silently.
        let scratch = Scratch::new("binary-name");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();
        let cargo = fs::read_to_string(created.dir.join("Cargo.toml")).unwrap();

        assert!(cargo.contains("name = \"app\""), "{cargo}");
        assert_eq!(BINARY_NAME, "app");
        // Cargo appends the platform's own suffix, so the name the template declares is the file's
        // stem — `app` on two platforms, `app.exe` on the third. Comparing against the whole file
        // name passed everywhere the suffix is absent and failed where it is not.
        let expected = Path::new(crate::process::EXPECTED_BINARY);
        assert_eq!(
            expected.file_stem().and_then(|stem| stem.to_str()),
            Some(BINARY_NAME),
            "the supervisor looks for {}",
            crate::process::EXPECTED_BINARY
        );
        assert_eq!(
            expected.parent().and_then(|dir| dir.to_str()),
            Some("target/release"),
            "the supervisor looks for {}",
            crate::process::EXPECTED_BINARY
        );
    }

    #[test]
    fn the_generated_gitignore_keeps_the_build_output_and_the_users_database_out() {
        let scratch = Scratch::new("gitignore");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();
        let ignore = fs::read_to_string(created.dir.join(".gitignore")).unwrap();
        for pattern in [
            "/target",
            "/frontend/dist",
            "/frontend/node_modules",
            "/app.db",
        ] {
            assert!(ignore.contains(pattern), "{pattern} missing from {ignore}");
        }
    }

    #[test]
    fn the_generated_page_declares_its_whole_look_as_one_token_block() {
        // Found by running deepnest for real and looking at the page: it worked and looked like a
        // wireframe. The fix has to be a look the app can keep, not a single flattering screen, so
        // what is checked is the seam the agent restyles through — one place for the palette and
        // the rhythm, and a dark set beside it.
        let scratch = Scratch::new("tokens");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();
        let page = fs::read_to_string(created.dir.join("frontend/index.html")).unwrap();

        for token in [
            "--bg:",
            "--surface:",
            "--text:",
            "--accent:",
            "--step:",
            "--radius:",
        ] {
            assert!(page.contains(token), "{token} missing from the token block");
        }
        // A page that reads the OS's preference is a page that is right in both.
        assert!(page.contains("prefers-color-scheme: dark"), "{page}");
        // `color-scheme` is what makes the scrollbar and form controls follow it.
        assert!(page.contains("color-scheme: light dark"), "{page}");
    }

    #[test]
    fn the_generated_page_styles_the_states_a_new_app_starts_in() {
        // An app's first screen is an empty list on a day-one database, so the empty state is the
        // design, not an edge case. Both states are checked in both halves — the stylesheet defines
        // them and the component reaches for them — because a class the component never uses is a
        // stylesheet that is dead on arrival.
        let scratch = Scratch::new("states");
        let created = create(&slug(), &scratch.dir(), "Notes").unwrap();
        let page = fs::read_to_string(created.dir.join("frontend/index.html")).unwrap();
        let script = fs::read_to_string(created.dir.join("frontend/src/main.jsx")).unwrap();

        assert!(page.contains(".empty"), "{page}");
        assert!(page.contains(".problem"), "{page}");
        assert!(script.contains("className=\"empty\""), "{script}");
        assert!(script.contains("className=\"problem\""), "{script}");
        // A failure is announced, not merely painted: colour alone is not a state a screen reader
        // reaches.
        assert!(script.contains("role=\"alert\""), "{script}");
    }
}
