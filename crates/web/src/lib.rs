//! The control panel: what deepnest serves at `/`.
//!
//! `crates/deepnest` is the process. This crate is the page a person clicks in, and the rule it is
//! built to replaces the one the placeholder panel stated about itself: **a page that looked
//! finished and did nothing would be worse than one that states the truth**. So the page carries
//! only what is true — the model endpoint that is configured or is not, the apps that are on disk
//! or why one could not be used — and every button it offers is a route the server has.
//!
//! ## Why the token is in the page
//!
//! The API is behind the control token, and a browser opening `http://127.0.0.1:4270/` does not
//! have one. So the page carries it in a `<meta>` element its own script reads. That is safe for a
//! reason worth writing down rather than assuming:
//!
//! - The page is served with no CORS headers, so a script on another origin can *send* a request
//!   here but cannot read the answer — and the token only ever travels in a response body.
//! - `Host` is validated by the router, so a name rebound to `127.0.0.1` is a `403`, not this page.
//! - Anything already on this machine that could fetch the page could read
//!   `~/.deepnest/deepnest.token` directly. That file's `0600` mode protects *other users*; the
//!   panel asks the same machine for the same secret, so it lowers no bar that was not already
//!   there.
//!
//! The alternative was to make the user paste the token into a form. That puts the same secret in
//! a form field, in the browser's autofill, and in whatever a user does with "save password" — and
//! it makes the first thing deepnest asks for a trip to a terminal.
//!
//! ## Why the script is a separate file
//!
//! A policy of `script-src 'self'` is worth more than the convenience of an inline script: it means
//! a string that reaches the page from the filesystem — an app's name, a slug — cannot become code.
//! The token reaches the script through the DOM rather than through a `<script>` block for the same
//! reason.
//!
//! ## What the page says without script
//!
//! The facts that cannot be fetched are rendered into the markup: the port, the endpoint state, and
//! how many apps are on disk. A reader with script off gets a page that is true and slightly stale
//! rather than a blank one, and a script that fails to load leaves something readable behind.
//!
//! ## What is not here yet
//!
//! The floating dialog and escalation approvals are `crates/gateway`'s and the agent's work: the
//! dialog has to be injected into every `/app/<slug>/` response so an agent rewriting the app's page
//! cannot delete it, and there is no agent in the binary to escalate to yet. The panel links to an
//! app's page and does not pretend to be a conversation with it.

/// Where the panel's script is served.
pub const SCRIPT_PATH: &str = "/panel.js";

/// What `GET /` is allowed to load.
///
/// `script-src 'self'` and nothing else: no inline script, no `eval`, no third-party origin. There
/// is no network access beyond this origin either (`connect-src 'self'`), because the panel talks
/// only to deepnest. `form-action 'none'` survives from the placeholder: the panel's forms are
/// handled by script, so a real form submission is a bug rather than a fallback.
pub const CSP: &str = "default-src 'none'; script-src 'self'; connect-src 'self'; \
                       style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; \
                       frame-ancestors 'none'";

/// The panel's script.
pub const SCRIPT: &str = include_str!("panel.js");

/// Everything the page renders that the server knows at request time.
///
/// Deliberately small. The app list is fetched by the script, so an app that appeared a second ago
/// is in the next refresh without the page being re-rendered; what cannot be fetched is the token,
/// the port, and the state of the config file, and those are the three things here.
#[derive(Debug, Clone)]
pub struct Panel {
    /// The port this server is on, for the page's own text.
    pub port: u16,
    /// The control token the script sends on every `/api/` call.
    pub token: String,
    /// The model name, when a config exists. `None` is first run.
    pub model: Option<String>,
    /// One plain-text line about the apps directory. Escaped here, never trusted.
    pub apps: String,
}

/// The panel's HTML.
pub fn page(panel: &Panel) -> String {
    PAGE.replace("@@PORT@@", &panel.port.to_string())
        .replace("@@TOKEN@@", &escape(&panel.token))
        .replace("@@ENDPOINT@@", &escape(&endpoint_state(panel)))
        .replace("@@APPS@@", &escape(&panel.apps))
}

/// The endpoint line, as text.
///
/// Text and not markup: the page's own script renders the same fact as markup a moment later, and
/// two spellings of one sentence is one more thing to keep in agreement.
fn endpoint_state(panel: &Panel) -> String {
    match &panel.model {
        Some(model) => format!("Model endpoint: configured ({model})."),
        None => "Model endpoint: not configured — deepnest needs one before it can write anything."
            .to_string(),
    }
}

/// Escape a value for an HTML text position or a double-quoted attribute.
///
/// The token is hex and an app's name is a directory name, so this is about the rule rather than
/// about today's input: a value that reaches the page is escaped at the point it is written, not at
/// the point it is hoped to be safe.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
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

/// The page. `@@PORT@@`, `@@TOKEN@@`, `@@ENDPOINT@@` and `@@APPS@@` are the only substitutions.
const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="deepnest-token" content="@@TOKEN@@">
<meta name="deepnest-port" content="@@PORT@@">
<title>deepnest</title>
<style>
  :root { color-scheme: light dark; --line: rgba(127,127,127,.32); --dim: .66; }
  body { font: 16px/1.55 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; margin: 0; }
  main { max-width: 44rem; margin: 8vh auto 4rem; padding: 0 1.5rem; }
  h1 { font-size: 1.6rem; margin: 0 0 .2rem; letter-spacing: -.02em; }
  h2 { font-size: 1.05rem; margin: 2rem 0 .6rem; letter-spacing: -.01em; }
  .sub { margin: 0 0 1.6rem; opacity: var(--dim); font-size: .9rem; }
  p { margin: 0 0 .85rem; }
  code, .mono { font: .875em ui-monospace, SFMono-Regular, Menlo, monospace; background: rgba(127,127,127,.14); padding: .1em .35em; border-radius: 4px; }
  section { border-top: 1px solid var(--line); padding-top: .4rem; margin-top: 1.6rem; }
  label { display: block; margin: .7rem 0 .2rem; font-size: .875rem; opacity: .85; }
  input { width: 100%; box-sizing: border-box; padding: .45rem .55rem; font: inherit; font-size: .9rem; color: inherit; background: transparent; border: 1px solid var(--line); border-radius: 6px; }
  button { font: inherit; font-size: .875rem; padding: .35rem .7rem; color: inherit; background: transparent; border: 1px solid var(--line); border-radius: 6px; cursor: pointer; }
  button:hover:not(:disabled) { border-color: currentColor; }
  button:disabled { opacity: .4; cursor: default; }
  button.primary { border-color: currentColor; font-weight: 600; }
  .row { display: flex; gap: .5rem; align-items: baseline; flex-wrap: wrap; }
  .app { border: 1px solid var(--line); border-radius: 8px; padding: .7rem .85rem; margin: 0 0 .6rem; }
  .app h3 { font-size: .95rem; margin: 0; }
  .app .slug { opacity: var(--dim); font-size: .8rem; }
  .app .actions { margin-top: .55rem; display: flex; gap: .4rem; flex-wrap: wrap; align-items: baseline; }
  .note { font-size: .85rem; opacity: var(--dim); }
  .state { font-size: .9rem; margin: .3rem 0 .9rem; }
  .bad { color: #c0392b; }
  .good { color: #2d7a46; }
  @media (prefers-color-scheme: dark) { .bad { color: #ff8b7d; } .good { color: #7ed6a1; } }
  noscript p { border-left: 3px solid var(--line); padding-left: .8rem; }
</style>
</head>
<body>
<main>
  <h1>deepnest</h1>
  <p class="sub">local-first app generator — running on <code>127.0.0.1:@@PORT@@</code></p>

  <p id="endpoint" class="state">@@ENDPOINT@@</p>

  <noscript>
    <p>The panel needs JavaScript to list apps and change anything. Without it the API still
    answers: <code>GET /api/health</code>, and <code>GET /api/apps</code> with the token from
    <code>~/.deepnest/deepnest.token</code> as a bearer credential.</p>
  </noscript>

  <section id="first-run" hidden>
    <h2>Connect a model</h2>
    <p class="note">deepnest ships no model. Your key goes to the endpoint you name and nowhere
    else; it is written to <code>~/.deepnest/config.toml</code> with mode <code>0600</code>.</p>
    <form id="endpoint-form">
      <label for="base-url">Base URL</label>
      <input id="base-url" name="base_url" placeholder="https://api.deepseek.com" autocomplete="off" required>
      <label for="api-key">API key</label>
      <input id="api-key" name="api_key" type="password" autocomplete="off" required>
      <label for="model">Model</label>
      <input id="model" name="model" placeholder="deepseek-flash" autocomplete="off" required>
      <p class="row" style="margin-top:.9rem"><button class="primary" type="submit">Save</button></p>
    </form>
  </section>

  <section id="apps-section">
    <h2>Apps</h2>
    <p id="apps-summary" class="state">Apps: @@APPS@@</p>
    <div id="apps"><p class="note">Reading the app list…</p></div>
    <details id="create-box">
      <summary class="note">New app</summary>
      <form id="create-form">
        <label for="new-slug">Name</label>
        <input id="new-slug" name="slug" placeholder="notes" autocomplete="off" required>
        <label for="new-name">Display name</label>
        <input id="new-name" name="name" placeholder="Notes" autocomplete="off">
        <p class="row" style="margin-top:.9rem"><button class="primary" type="submit">Create</button></p>
      </form>
    </details>
  </section>

  <p id="status" class="note" role="status" aria-live="polite"></p>
</main>
<script src="/panel.js"></script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> Panel {
        Panel {
            port: 4270,
            token: "0123456789abcdef".repeat(4),
            model: Some("deepseek-flash".to_string()),
            apps: "2 apps in /home/u/.deepnest/apps".to_string(),
        }
    }

    #[test]
    fn the_page_carries_the_token_the_port_and_what_the_server_already_knows() {
        let page = page(&panel());
        assert!(page.contains(r#"<meta name="deepnest-token" content="0123456789abcdef"#));
        assert!(page.contains(r#"<meta name="deepnest-port" content="4270">"#));
        assert!(page.contains("127.0.0.1:4270"));
        // Rendered, not fetched: a reader without script still learns these.
        assert!(page.contains("configured (deepseek-flash)"), "{page}");
        assert!(page.contains("2 apps in /home/u/.deepnest/apps"), "{page}");
    }

    #[test]
    fn first_run_says_the_endpoint_is_not_configured() {
        let page = page(&Panel {
            model: None,
            ..panel()
        });
        assert!(page.contains("not configured"), "{page}");
    }

    #[test]
    fn the_script_is_a_file_rather_than_an_inline_block() {
        // What `script-src 'self'` buys: a string from the filesystem — an app's name — cannot
        // become code, because no inline script would ever run.
        let page = page(&panel());
        assert!(
            page.contains(r#"<script src="/panel.js"></script>"#),
            "{page}"
        );
        assert!(
            !page.contains("<script>"),
            "an inline script would need 'unsafe-inline' in the policy"
        );
        assert!(CSP.contains("script-src 'self'"), "{CSP}");
        assert!(!CSP.contains("script-src 'unsafe-inline'"), "{CSP}");
    }

    #[test]
    fn a_value_that_reaches_the_page_is_escaped() {
        let page = page(&Panel {
            port: 4270,
            token: "\"><script>alert(1)</script>".to_string(),
            model: Some("<img onerror=x>".to_string()),
            apps: "</p><script>alert(2)</script>".to_string(),
        });
        assert!(!page.contains("<script>alert"), "{page}");
        assert!(!page.contains("<img onerror"), "{page}");
        assert!(page.contains("&quot;&gt;&lt;script&gt;"), "{page}");
    }

    #[test]
    fn the_page_does_not_claim_the_dialog_exists() {
        // The floating dialog and escalation approvals are gateway and agent work. A panel that
        // offered them would be offering a button that does nothing.
        let page = page(&panel()).to_lowercase();
        assert!(!page.contains("escalat"), "{page}");
        assert!(!page.contains("dialog"), "{page}");
    }

    #[test]
    fn the_script_calls_the_routes_the_server_has() {
        // Cheap guard against a script and a router drifting apart: every path the script fetches
        // is one this repository serves.
        for path in ["/api/health", "/api/apps", "/api/config"] {
            assert!(
                SCRIPT.contains(path),
                "the script never calls {path}, so either it is dead or this test is stale"
            );
        }
        // And the one route the markup names is the one the server answers.
        assert_eq!(SCRIPT_PATH, "/panel.js");
    }
}
