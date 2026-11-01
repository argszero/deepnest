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
//! The facts that cannot be fetched are rendered into the markup: the port, the endpoint state, how
//! many apps are on disk, and the skills the loader found. A reader with script off gets a page that
//! is true and slightly stale rather than a blank one, and a script that fails to load leaves
//! something readable behind.
//!
//! ## The app page's dialog
//!
//! This crate also owns the bytes of the floating dialog that `crates/gateway` injects into every
//! page an app answers ([`DIALOG_SCRIPT`], [`DIALOG_STYLE`]). The split is deliberate: the gateway
//! decides *where* the dialog goes and what policy it needs — that is rewriting, and it is the
//! gateway's job — while the ball itself is a page a person clicks in, which is this crate's.
//!
//! The dialog carries no token. The app's own script shares this origin, so a token written into an
//! app's page would be a token the app could read and spend; the panel's own token is safe in the
//! panel because the panel is deepnest's page and the app's is not. What it calls instead is two
//! routes, each paired with a cookie the server minted when it served this page and scoped to that
//! one path — `POST …/chat`, and `GET`/`POST …/escalation`.
//!
//! ## The widening request
//!
//! Escalation is one hop, per call, approved by a person (`host 19:03`), and this is the page that
//! person is looking at. A turn that needs more room than the session's mode stops inside the call
//! that needs it; the dialog polls the escalation route, shows the asked-for mode and the agent's
//! justification, and puts two buttons under it — **Allow for this call**, and **No**. The approval
//! is the audit record, so what it approved of is shown before it can be given, and the answer names
//! it back.
//!
//! Two properties are the reason it is built this way rather than as a permission the agent asks for
//! once: the buttons are the *only* place an answer comes from, so the page never answers on its own;
//! and the approval buys one call, so the panel's mode and the running session's mode are the same
//! before and after it.

/// Where the panel's script is served.
pub const SCRIPT_PATH: &str = "/panel.js";

/// Where the app page's dialog script is served.
///
/// The pair below is what `deepnest_gateway::dialog` names in the markup it injects; this crate
/// holds the bytes, and `the_dialog_the_gateway_names_is_served_here` pins the two together.
pub const DIALOG_SCRIPT_PATH: &str = "/dialog.js";

/// Where the app page's dialog stylesheet is served.
pub const DIALOG_STYLE_PATH: &str = "/dialog.css";

/// The app page's dialog script.
pub const DIALOG_SCRIPT: &str = include_str!("dialog.js");

/// The app page's dialog stylesheet.
pub const DIALOG_STYLE: &str = include_str!("dialog.css");

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
/// the port, the state of the config file, and the skills this process read on the way here — and
/// those are what is here.
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
    /// One plain-text line about the skills directory. Escaped here, never trusted.
    pub skills: String,
    /// The skills that loaded, as `(name, description)` — text, escaped here.
    ///
    /// A list rendered into the page rather than one the script fetches, unlike the app list: this
    /// is the page a user looks at *after* editing a skill file, so the version that says nothing
    /// until a round trip finishes is the version that hides the edit.
    pub skill_list: Vec<(String, String)>,
    /// The entries under the skills root that are not usable skills, as `(path, reason)`.
    ///
    /// Separate from [`Panel::skill_list`] because the two say different things: an empty list
    /// beside a full one of these means "your skill has a typo", and one merged list would read
    /// as "no skills yet".
    pub skill_problems: Vec<(String, String)>,
    /// What the shipped catalog offers and what is not on disk here yet.
    ///
    /// Rendered into the markup for the reason [`Panel::skill_list`] is: the catalog is a file the
    /// user edits, and a listing that arrived a round trip later would hide the edit. The press for
    /// an entry deepnest can fetch is the script's job — this crate states what is true, and a
    /// button that did nothing without a script would be a claim the page cannot keep.
    pub skill_offered: Vec<Offered>,
    /// The `skills:` lines the catalog has and could not read, one sentence each.
    ///
    /// The same fact as [`Panel::skill_problems`] one level up: a hand-edited catalog with a typo
    /// would otherwise lose its entry without a word.
    pub catalog_problems: Vec<String>,
}

/// One entry the catalog names which is not installed here.
///
/// No `installable` flag beside [`Offered::why_not`]: the two would be one fact spelled twice, and
/// the spelling that matters is the one the page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    /// The name the catalog gives it, and what an install asks for.
    pub name: String,
    /// The catalog's one line about what it is for.
    pub description: String,
    /// Why deepnest will not fetch it, in the catalog's own terms — `None` when it will.
    pub why_not: Option<String>,
}

/// The panel's HTML.
pub fn page(panel: &Panel) -> String {
    let page = PAGE
        .replace("@@PORT@@", &panel.port.to_string())
        .replace("@@TOKEN@@", &escape(&panel.token))
        .replace("@@ENDPOINT@@", &escape(&endpoint_state(panel)))
        .replace("@@APPS@@", &escape(&panel.apps))
        .replace("@@SKILLS@@", &escape(&panel.skills));
    // Last, and deliberately. The skills list is markup built below out of labels that came from
    // files on disk; substituting it any earlier would hand a description containing `@@APPS@@` to
    // the replace above, which would then write an app count into a line of a skill.
    page.replace("@@SKILL_LIST@@", &skill_markup(panel))
}

/// The skills section: what loaded, then what the catalog offers and this home does not have, then
/// what did not load and what the catalog itself could not read.
///
/// Built here rather than by the caller for the reason the token and the app line are escaped
/// here: the crate that owns the page is the crate that knows which of its positions are markup
/// and which are text.
fn skill_markup(panel: &Panel) -> String {
    let mut out = String::new();
    if !panel.skill_list.is_empty() {
        out.push_str("    <ul class=\"skills\">\n");
        for (name, description) in &panel.skill_list {
            out.push_str(&format!(
                "      <li><code>{}</code> <span class=\"note\">{}</span></li>\n",
                escape(name),
                escape(description)
            ));
        }
        out.push_str("    </ul>\n");
    }
    if !panel.skill_offered.is_empty() {
        out.push_str(
            "    <p class=\"note\">The catalog that ships with deepnest offers these too:</p>\n",
        );
        out.push_str("    <ul class=\"skills\">\n");
        for offered in &panel.skill_offered {
            out.push_str("      <li");
            if offered.why_not.is_none() {
                // The hook the page's script binds a press to. An entry deepnest cannot fetch gets
                // none: the reason under it is the whole of what there is to do about it.
                out.push_str(&format!(" data-offer=\"{}\"", escape(&offered.name)));
            }
            out.push_str(&format!(
                "><code>{}</code> <span class=\"note\">{}</span>",
                escape(&offered.name),
                escape(&offered.description)
            ));
            if let Some(why) = &offered.why_not {
                out.push_str(&format!(" <span class=\"note\">{}</span>", escape(why)));
            }
            out.push_str("</li>\n");
        }
        out.push_str("    </ul>\n");
    }
    if !panel.catalog_problems.is_empty() {
        out.push_str("    <p class=\"note\">The catalog has lines deepnest could not read:</p>\n");
        out.push_str("    <ul class=\"skills\">\n");
        for problem in &panel.catalog_problems {
            out.push_str(&format!(
                "      <li><span class=\"bad\">{}</span></li>\n",
                escape(problem)
            ));
        }
        out.push_str("    </ul>\n");
    }
    if !panel.skill_problems.is_empty() {
        let count = panel.skill_problems.len();
        let (entry, verb, thing) = if count == 1 {
            ("entry", "is", "a skill")
        } else {
            ("entries", "are", "skills")
        };
        out.push_str(&format!(
            "    <p class=\"note\">{count} {entry} under the skills root {verb} not {thing}:</p>\n"
        ));
        out.push_str("    <ul class=\"skills\">\n");
        for (path, reason) in &panel.skill_problems {
            out.push_str(&format!(
                "      <li><code>{}</code> <span class=\"note\">{}</span></li>\n",
                escape(path),
                escape(reason)
            ));
        }
        out.push_str("    </ul>\n");
    }
    out
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

/// The page. `@@PORT@@`, `@@TOKEN@@`, `@@ENDPOINT@@`, `@@APPS@@`, `@@SKILLS@@` and
/// `@@SKILL_LIST@@` are the substitutions — the skills list last, so a description that happens to
/// contain a placeholder stays a description.
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
  ul { margin: .3rem 0 .9rem; padding-left: 1.1rem; }
  li { margin: .3rem 0; }
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

  <section id="skills-section">
    <h2>Skills</h2>
    <p id="skills-summary" class="state">Skills: @@SKILLS@@</p>
    <p class="note">A skill is instructions the agent may choose to follow, installed at
    <code>~/.deepnest/skills/&lt;name&gt;/SKILL.md</code> or as one Markdown file beside it. A skill
    an <em>app</em> carries at <code>&lt;app&gt;/.agent/skills/</code> is not in this list: this page
    belongs to no app, and one list of every app's skills would say each of them applies everywhere.</p>
@@SKILL_LIST@@
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
            skills: "1 skill in /home/u/.deepnest/skills".to_string(),
            skill_list: vec![("notes".to_string(), "Use when writing a note".to_string())],
            skill_problems: Vec::new(),
            skill_offered: Vec::new(),
            catalog_problems: Vec::new(),
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
            skills: "</p><script>alert(3)</script>".to_string(),
            // A skill's name is a directory or a header line, and its description is whatever the
            // user typed: both are input, and both land in markup.
            skill_list: vec![(
                "<b>name</b>".to_string(),
                "</li><script>alert(4)</script>".to_string(),
            )],
            skill_problems: vec![("<i>path</i>".to_string(), "<img onerror=y>".to_string())],
            // The catalog is a file the user edits: its names, descriptions and reasons are input
            // like any other, and one of them reaching the page as markup would be the same bug.
            skill_offered: vec![Offered {
                name: "<em>offered</em>".to_string(),
                description: "<script>alert(5)</script>".to_string(),
                why_not: Some("<u>why</u>".to_string()),
            }],
            catalog_problems: vec!["<s>problem</s>".to_string()],
        });
        assert!(!page.contains("<script>alert"), "{page}");
        assert!(!page.contains("<img onerror"), "{page}");
        assert!(!page.contains("<b>name</b>"), "{page}");
        assert!(!page.contains("<i>path</i>"), "{page}");
        assert!(!page.contains("<em>offered</em>"), "{page}");
        assert!(!page.contains("<u>why</u>"), "{page}");
        assert!(!page.contains("<s>problem</s>"), "{page}");
        assert!(!page.contains("<script>alert(5)"), "{page}");
        assert!(page.contains("&quot;&gt;&lt;script&gt;"), "{page}");
    }

    #[test]
    fn the_page_lists_the_skills_the_loader_found() {
        // Rendered, not fetched: this is the page a user looks at right after editing a skill.
        let page = page(&panel());
        assert!(
            page.contains("1 skill in /home/u/.deepnest/skills"),
            "{page}"
        );
        assert!(page.contains("<code>notes</code>"), "{page}");
        assert!(page.contains("Use when writing a note"), "{page}");
    }

    #[test]
    fn the_page_says_which_skills_it_does_not_list() {
        // The panel belongs to no app, so it reads the user's skills only. A page that stayed
        // silent about that would make "my app's skill does not show up" the next bug report.
        let page = page(&panel());
        assert!(page.contains("is not in this list"), "{page}");
        assert!(page.contains(".agent/skills/"), "{page}");
    }

    #[test]
    fn a_skill_that_could_not_be_used_is_reported_beside_the_ones_that_could() {
        let page = page(&Panel {
            skill_problems: vec![(
                "/home/u/.deepnest/skills/broken/SKILL.md".to_string(),
                "the header has no description".to_string(),
            )],
            ..panel()
        });
        assert!(
            page.contains("1 entry under the skills root is not a skill"),
            "{page}"
        );
        assert!(page.contains("the header has no description"), "{page}");
        // And the one that did load is still there: a fault in one skill is not a reason to hide
        // the others.
        assert!(page.contains("<code>notes</code>"), "{page}");
    }

    #[test]
    fn a_description_that_looks_like_a_placeholder_is_left_alone() {
        // The skills list is substituted last for this reason: `@@APPS@@` in a description is a
        // description, not a request to repeat the app count inside a skill's line.
        let page = page(&Panel {
            skill_list: vec![("odd".to_string(), "@@APPS@@".to_string())],
            ..panel()
        });
        assert!(
            page.contains(r#"<span class="note">@@APPS@@</span>"#),
            "{page}"
        );
    }

    #[test]
    fn an_entry_the_catalog_offers_is_listed_with_a_place_for_the_press() {
        // The catalog ships to tell a user that a capability exists before they have it. An entry
        // that is not on disk is therefore the one case the page has to show, not hide.
        let page = page(&Panel {
            skill_offered: vec![Offered {
                name: "browser-harness".to_string(),
                description: "Drive the browser".to_string(),
                why_not: None,
            }],
            ..panel()
        });
        assert!(page.contains("offers these too"), "{page}");
        assert!(page.contains(r#"data-offer="browser-harness""#), "{page}");
        assert!(page.contains("Drive the browser"), "{page}");
    }

    #[test]
    fn an_entry_deepnest_will_not_fetch_says_why_and_offers_no_press() {
        // A press here could only run something the catalog did not describe, and a row with no
        // press and no reason would read as a button that is broken rather than as one absent.
        let page = page(&Panel {
            skill_offered: vec![Offered {
                name: "baoyu-design".to_string(),
                description: "Designs".to_string(),
                why_not: Some("it names no `repo`, so deepnest cannot fetch it".to_string()),
            }],
            ..panel()
        });
        assert!(!page.contains("data-offer"), "{page}");
        assert!(page.contains("it names no `repo`"), "{page}");
    }

    #[test]
    fn a_catalog_line_that_could_not_be_read_is_reported() {
        let page = page(&Panel {
            catalog_problems: vec!["catalog entry `notes` has no description".to_string()],
            ..panel()
        });
        assert!(page.contains("could not read"), "{page}");
        assert!(page.contains("has no description"), "{page}");
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
    fn the_dialog_sends_a_message_and_holds_no_credential_of_its_own() {
        // The box exists, and the one route it calls is the chat route whose path the server scopes
        // the cookie to. What it must never contain is the token: the app's script shares this
        // origin, so a token here is a token the app can read and spend everywhere.
        assert!(DIALOG_SCRIPT.contains("/api/apps/"), "the box never posts");
        assert!(
            DIALOG_SCRIPT.contains("/chat"),
            "the box posts to some other route"
        );
        assert!(
            DIALOG_SCRIPT.contains("credentials"),
            "the cookie rides on that attribute"
        );
        assert!(
            DIALOG_SCRIPT.contains("same-origin"),
            "any other credentials mode stops the cookie being sent"
        );
        assert!(DIALOG_SCRIPT.contains(r#""Content-Type": "application/json""#));
        assert!(
            !DIALOG_SCRIPT.contains("deepnest-token"),
            "the dialog must not read the panel's token meta"
        );
        assert!(
            !DIALOG_SCRIPT.contains("authorization"),
            "the dialog must not carry the control token"
        );
    }

    #[test]
    fn the_dialog_takes_a_pasted_picture_and_sends_it_with_the_message() {
        // The core page could carry words only, and "look at this" is what a screenshot is for.
        // What a screenshot is *for* is the part that is easy to get wrong: `this bit is wrong, do
        // it like that one` is two pictures and one sentence, and a picture parked beside the box
        // has nothing tying it to the sentence it is about. Three things have to hold at once — the
        // paste is caught, the picture lands where the caret was, and its bytes travel in the same
        // body as the message — because any one of them missing is a paste that silently does
        // nothing.
        assert!(
            DIALOG_SCRIPT.contains("clipboardData"),
            "nothing reads the clipboard"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"addEventListener("paste""#),
            "the paste event is never bound"
        );
        assert!(
            DIALOG_SCRIPT.contains("readAsDataURL"),
            "a pasted file is never read"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"indexOf("image/")"#),
            "everything pasted would be taken for a picture"
        );
        // The place, not a list. `setRangeText` is what keeps the marker inside the words: it
        // inserts at the caret and it keeps the box's undo history, which an assignment to `value`
        // would throw away along with the ability to cut the marker back out.
        assert!(
            DIALOG_SCRIPT.contains("setRangeText"),
            "the marker is not inserted into the text"
        );
        assert!(
            DIALOG_SCRIPT.contains("selectionStart"),
            "the insertion point is not the caret"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"return "[image " + id + "]""#),
            "the marker has no number to be referred to by"
        );
        // Read back out of the words: a marker that was deleted is a picture the person decided
        // against, and the order of the markers is the order the request must carry.
        assert!(
            DIALOG_SCRIPT.contains("picturesReferredTo("),
            "the pictures are not read out of the message"
        );
        assert!(
            DIALOG_SCRIPT.contains("ask(text, picturesReferredTo(text)"),
            "the request does not carry the pictures the words mention"
        );
        // The same body the route already reads, with one more key, and only when it is needed.
        assert!(DIALOG_SCRIPT.contains("body.images = images"));
        // Nothing left behind once the message is away, because a picture nobody chose to resend
        // is tokens spent on every later turn of the app's life.
        assert!(
            DIALOG_SCRIPT.contains("forgetAttachments();"),
            "the pictures are still in the box after the message was sent"
        );
        // The strip is gone: a picture beside the box is the shape this change exists to remove.
        assert!(
            !DIALOG_SCRIPT.contains("deepnest-dialog-pictures"),
            "the picture strip is still in the script"
        );
        assert!(
            !DIALOG_STYLE.contains("#deepnest-dialog-pictures"),
            "the picture strip is still in the stylesheet"
        );
        // The token rule from the test above holds here too: a picture does not need a credential.
        assert!(!DIALOG_SCRIPT.contains("deepnest-token"));
        // Colours and layout come from the stylesheet, never from an inline style.
        assert!(DIALOG_STYLE.contains("#deepnest-dialog-picture-note"));
    }

    #[test]
    fn a_picture_travels_with_the_words_that_point_at_it() {
        // The reason the marker is in the text and not in a list: the request's `message` is what
        // the model reads in order, so the reference and the thing referenced arrive in one piece.
        // A message whose picture has been taken out is a message with no marker for it, and the
        // two readings — what the words say and what the request carries — cannot disagree.
        assert!(
            DIALOG_SCRIPT.contains("var pattern = /\\[image (\\d+)\\]/g"),
            "the marker is not parsed out of the words"
        );
        assert!(
            DIALOG_SCRIPT.contains("found.push(attached[index].url)"),
            "a marker does not resolve to the picture it numbers"
        );
        // Deleted marker, deleted picture: `attached` is consulted only through the markers, so an
        // entry nothing refers to is never sent.
        assert!(
            !DIALOG_SCRIPT.contains("body.images = attached"),
            "the request sends every picture that was ever pasted"
        );
        // A marker typed twice is one picture, because one question does not become two by being
        // pointed at twice.
        assert!(
            DIALOG_SCRIPT.contains("if (!taken[id])"),
            "a repeated marker would send its picture twice"
        );
    }

    #[test]
    fn the_dialog_takes_a_file_the_same_way_it_takes_a_picture() {
        // A file is the picture case with one difference that decides the whole design: it has a
        // name. `[file report.csv]` is the marker, so what the person's sentence points at and what
        // the model matches the contents to are the same string. The three ways a file arrives —
        // pasted, dropped, chosen — all have to reach that one insertion point, because a feature
        // that works from the clipboard and does nothing from the file picker reads as broken.
        assert!(
            DIALOG_SCRIPT.contains(r#"return "[file " + name + "]""#),
            "a file's marker does not carry its name"
        );
        assert!(
            DIALOG_SCRIPT.contains("filesReferredTo("),
            "the files are not read back out of the message"
        );
        assert!(
            DIALOG_SCRIPT.contains("ask(text, picturesReferredTo(text), filesReferredTo(text))"),
            "the request does not carry the files the words name"
        );
        assert!(
            DIALOG_SCRIPT.contains("body.files = files.map("),
            "the files key is added whether or not there are files"
        );
        // Markers have to be tellable apart, and by the same reading that resolves them: the words
        // in front of the bracket.
        assert!(
            DIALOG_SCRIPT.contains("var pattern = /\\[file ([^\\]\\n]+)\\]/g"),
            "the file marker is not parsed, or is parsed the same shape as a picture's"
        );
        // Dropping: without `preventDefault` the browser navigates to the file instead.
        assert!(
            DIALOG_SCRIPT.contains(r#"addEventListener("dragover""#),
            "a dropped file would open in the browser"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"addEventListener("drop""#),
            "nothing takes a dropped file"
        );
        // Choosing: a hidden input a button drives, so the dialog keeps its shape and a keyboard
        // still reaches it.
        assert!(
            DIALOG_SCRIPT.contains(r#""deepnest-dialog-picker""#)
                && DIALOG_SCRIPT.contains(r#""deepnest-dialog-attach""#),
            "there is no way to choose a file that is not on the clipboard"
        );
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog-picker[hidden]"),
            "`all: initial` resets `[hidden]`, so the picker would be drawn"
        );
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog-attach"),
            "the button is styled nowhere"
        );
        // The paste path no longer insists on `image/*`: a spreadsheet copied out of a file manager
        // arrives the same way, and dropping it was a paste that silently did nothing.
        assert!(
            DIALOG_SCRIPT.contains(r#"if (item.kind === "file") {"#),
            "only pictures survive a paste"
        );
        // The bytes go in the body, not a path: nothing here names a file on disk for the server to
        // open, and the base64 prefix the data URL carries is dropped before it travels.
        assert!(
            DIALOG_SCRIPT.contains("readAsDataURL"),
            "a file's bytes are never read"
        );
        assert!(
            DIALOG_SCRIPT.contains("reader.result.slice(comma + 1)"),
            "the data: prefix travels to the server instead of being dropped here"
        );
        assert!(
            !DIALOG_SCRIPT.contains("file_path"),
            "the dialog names a path for the server to read"
        );
        // The name is cleaned where the person can see the marker that results, because the server
        // refuses a name a marker cannot hold.
        assert!(
            DIALOG_SCRIPT.contains("function fileNameOf("),
            "a name is used without the characters a marker cannot hold being taken out"
        );
    }

    #[test]
    fn the_dialog_draws_a_message_that_was_only_a_picture() {
        // The read-back half of the paste: a message sent as a screenshot alone has no text in the
        // record, so a transcript that drew `turn.user` drew an empty row and the answer hung
        // under nothing. The picture is what the turn was about, and the page has to say so.
        assert!(
            DIALOG_SCRIPT.contains("turn.pictures"),
            "the transcript never reads the picture count"
        );
        assert!(
            DIALOG_SCRIPT.contains("picturesIn("),
            "there is no phrase for a number of pictures"
        );
        // The words are used when they are there — the file has no text only sometimes. A message
        // that was a picture, or a file, and nothing else still gets a line saying what it was.
        assert!(
            DIALOG_SCRIPT.contains("turn.user || (pictures + files > 0"),
            "an empty question is drawn as an empty line"
        );
        // And a question that had both says what came with it.
        assert!(DIALOG_SCRIPT.contains("\"with \" + withWhat(pictures, files)"));
        assert!(DIALOG_SCRIPT.contains("deepnest-pictures"));
        // `all: initial` means an unstyled line inherits nothing, so it has to be in the sheet.
        assert!(DIALOG_STYLE.contains("#deepnest-dialog-transcript .deepnest-pictures"));
    }

    #[test]
    fn the_transcript_counts_files_instead_of_pouring_them_into_the_chat() {
        // The record holds what the model was shown: the words, then the files' contents. The page
        // is showing a person what they said. A csv inside a chat line is a screen of data where a
        // sentence used to be, so the count is what is drawn — the same trade `pictures` makes.
        assert!(
            DIALOG_SCRIPT.contains("turn.files || 0"),
            "the transcript never reads the file count"
        );
        assert!(
            DIALOG_SCRIPT.contains("function filesIn("),
            "there is no phrase for a number of files"
        );
        assert!(
            DIALOG_SCRIPT.contains("function withWhat(pictures, files)"),
            "a message with both kinds says only one of them"
        );
        assert!(
            DIALOG_SCRIPT.contains("\"with \" + withWhat(pictures, files)"),
            "a question that named files does not say so"
        );
    }

    #[test]
    fn the_transcript_shows_each_call_and_not_only_how_many_there_were() {
        // The defect: the transcript said "ran 12 tools" and nothing else, so which file was read,
        // what the shell was told to run, and whether a call was denied or attempted were all
        // unknowable from the page — while the file the route reads had every one of them.
        assert!(
            DIALOG_SCRIPT.contains("renderCalls"),
            "the calls the agent made are not drawn"
        );
        assert!(
            DIALOG_SCRIPT.contains("deepnest-call-name"),
            "a row does not say which tool ran"
        );
        assert!(
            DIALOG_SCRIPT.contains("deepnest-call-what"),
            "a row does not say what the call was about"
        );
        // Three endings and a fourth word for an ending the file does not name. `ok` is never the
        // fallback: a call with no answer is a call with no answer.
        for word in [r#""failed""#, r#""refused""#, r#""done""#, r#""running""#] {
            assert!(
                DIALOG_SCRIPT.contains(&format!("return {word}")) || DIALOG_SCRIPT.contains(word),
                "a row cannot say {word}"
            );
        }
        assert!(
            DIALOG_SCRIPT.contains("turn.calls"),
            "the rows are not read from the turn"
        );
        // The output is behind a disclosure, and a poll must not shut it: the transcript is
        // rebuilt every time the file grows, which while a turn runs is every round.
        assert!(
            DIALOG_SCRIPT.contains(r#"createElement("details")"#),
            "the output is not collapsed"
        );
        assert!(
            DIALOG_SCRIPT.contains("openedCalls"),
            "a disclosure shuts itself on the next poll"
        );
        // Every one of those strings came from a model or from a file, so none of them is markup.
        // The word appears in the file's own comment about the rule; what must not appear is a
        // line that assigns to it.
        assert!(
            !DIALOG_SCRIPT.contains("innerHTML =")
                && !DIALOG_SCRIPT.contains("insertAdjacentHTML("),
            "the transcript builds markup instead of nodes"
        );
        for selector in [
            "#deepnest-dialog-transcript .deepnest-call-name",
            "#deepnest-dialog-transcript .deepnest-call-state",
            "#deepnest-dialog-transcript .deepnest-call-refused .deepnest-call-state",
            "#deepnest-dialog-transcript .deepnest-call-out pre",
        ] {
            assert!(DIALOG_STYLE.contains(selector), "{selector} is unstyled");
        }
    }

    #[test]
    fn the_answer_is_drawn_as_markdown_without_turning_it_into_markup() {
        // The model answers in Markdown; the transcript showed the source. Rendering it is the
        // fix, and rendering it by building HTML from a model's words would be a hole in the page.
        for part in [
            "renderWords",
            "withInline",
            "createElement(\"li\")",
            "createElement(\"pre\")",
        ] {
            assert!(DIALOG_SCRIPT.contains(part), "{part} is missing");
        }
        // Links are the one thing a rendered answer can make the page *do*, so only the two
        // schemes that are documents are links, and none of them may reach back into this page.
        assert!(
            DIALOG_SCRIPT.contains("https?:\\/\\/"),
            "the link pattern does not restrict the scheme"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"link.rel = "noopener noreferrer""#),
            "a link in an answer can reach into this page"
        );
        let words = DIALOG_SCRIPT
            .find("function renderWords(text, what)")
            .expect("the renderer exists");
        let body = &DIALOG_SCRIPT[words..];
        let body = &body[..body.find("\n  }").expect("it ends")];
        assert!(
            !body.contains("innerHTML") && !body.contains("insertAdjacentHTML"),
            "the renderer assembles markup"
        );
    }

    #[test]
    fn every_copy_control_says_which_exchange_it_is_in() {
        // Two scopes of copy control (a block, and the whole answer) times six exchanges was
        // twelve controls with two names between them. Measured in Chrome's own accessibility tree
        // against six exchanges: the announced names were "Copy this code block" and "Copy this
        // answer", six of each, with nothing saying which exchange a control belonged to — so a
        // person with a screen reader could copy the wrong answer twice before noticing.
        assert!(
            DIALOG_SCRIPT.contains("function copyNames(turn, number)"),
            "the controls have no per-exchange name to share"
        );
        // Named by what was asked, which is the one thing about a turn a person recognizes; the
        // position is the fallback for a message that carried only a picture and has no words.
        assert!(
            DIALOG_SCRIPT.contains(r#"where: 'the answer to "' + question + '"'"#)
                && DIALOG_SCRIPT.contains(r#"answer: 'Copy the answer to "' + question + '"'"#),
            "the answer is not named by the question it answers"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"answer: "Copy the answer in " + place"#),
            "a message with no words has no name to fall back on"
        );
        // A block says which block, in which answer, and only bothers with an ordinal when there
        // is more than one.
        assert!(
            DIALOG_SCRIPT.contains("function blockLabel(language, what, ordinal, total)"),
            "a block is named without saying which block it is"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"" of " + total + " in " + what"#),
            "two blocks in one answer are named the same"
        );
        assert!(
            DIALOG_SCRIPT.contains("var names = copyNames(turn, number);"),
            "the names are built but never used"
        );
        // The old fixed words are gone from the code. They are quoted in the comment above, which
        // is where this repository records what a fix replaced — so the assertion is about the
        // control's argument, not about the phrase being absent from the file.
        assert!(
            !DIALOG_SCRIPT.contains(r#"copyControl("deepnest-answer-copy", "Copy this answer""#)
                && !DIALOG_SCRIPT.contains(r#""Copy this code block","#),
            "a control still carries the name that said nothing"
        );
    }

    #[test]
    fn the_conversation_can_be_reached_by_a_keyboard() {
        // The transcript is the one box in the dialog that scrolls on its own — `max-height:
        // 18rem` and `overflow-y: auto` — and a box a keyboard cannot focus is a box a keyboard
        // cannot scroll. Measured in Chrome with real key presses, before this: the tab order ran
        // from the ball to "Conversations", then through the twelve copy buttons *inside* the
        // transcript, then on to the jump button and the message box. The transcript itself never
        // took focus, so with a keyboard the answers above the fold could not be read at all.
        //
        // The reading is about reach rather than about scrolling: focused by script, the box did
        // move on PageDown in the same browser. What was missing was the way in.
        assert!(
            DIALOG_SCRIPT.contains(r#"transcript.setAttribute("tabindex", "0")"#),
            "the transcript is not in the tab order, so a keyboard cannot reach the conversation"
        );
        // `role="log"` and its label were already there; the tab stop is what makes them reachable,
        // and a test that dropped either half would leave a box announced to nobody.
        assert!(
            DIALOG_SCRIPT.contains(r#"transcript.setAttribute("role", "log")"#),
            "the conversation is not announced as a log"
        );
        assert!(
            DIALOG_SCRIPT
                .contains(r#"transcript.setAttribute("aria-label", "This app's conversation")"#),
            "the conversation has no name to announce"
        );
        // A box that takes focus has to show it, and inside its own edge: drawn outside, the ring
        // lands on the jump button and the message box beneath.
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog-transcript:focus-visible"),
            "the transcript takes focus without saying so"
        );
        assert!(
            DIALOG_STYLE.contains("outline-offset: -2px"),
            "the focus ring is drawn outside the box it belongs to"
        );
    }

    #[test]
    fn the_transcript_yields_to_a_reader_who_scrolled_up_and_offers_the_way_back() {
        // A transcript that scrolls itself while it is being written to cannot be read while the
        // agent works, which is the one moment reading it is worth something.
        assert!(
            DIALOG_SCRIPT.contains("function atTheLatest()"),
            "there is no single answer to where the reader is"
        );
        assert!(
            DIALOG_SCRIPT.contains("offerTheLatest"),
            "nothing decides whether the way back is offered"
        );
        assert!(
            DIALOG_SCRIPT.contains("deepnest-dialog-jump"),
            "the way back has no control"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"addEventListener("scroll""#),
            "the control does not follow the reader"
        );
        // Offered, not taken: the only thing the button does is move the reader when pressed.
        let press = DIALOG_SCRIPT
            .find("jump.addEventListener(\"click\"")
            .expect("the control is wired");
        let handler = &DIALOG_SCRIPT[press..];
        let handler = &handler[..handler.find("\n  });").expect("the handler ends")];
        assert!(
            handler.contains("transcript.scrollTop = transcript.scrollHeight"),
            "the press does not go to the newest line: {handler}"
        );
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog-jump"),
            "the control is unstyled"
        );
    }

    #[test]
    fn a_code_block_in_an_answer_can_be_taken_out_of_it() {
        // An answer here is usually code, and code is written to be used somewhere else. Before
        // this, the only way out of a fenced block was to select it by hand — through a `pre` that
        // wrapped, so the selection picked up the dialog's own line breaks.
        assert!(
            DIALOG_SCRIPT.contains("function fencedBlock(language, source, what, ordinal, total)"),
            "a fenced block is drawn without the language its fence named"
        );
        assert!(
            DIALOG_SCRIPT.contains("line.slice(3).trim()"),
            "the language on the fence's own line is dropped"
        );
        // Two scopes, because they are two different jobs: one block of code, or everything the
        // answer said.
        for control in ["deepnest-code-copy", "deepnest-answer-copy"] {
            assert!(
                DIALOG_SCRIPT.contains(control),
                "there is no {control} control"
            );
            assert!(DIALOG_STYLE.contains(control), "{control} is unstyled");
        }
        // `navigator.clipboard` is absent or rejects outside a secure context, and `127.0.0.1`
        // is one while a LAN address a future build is served from is not — so the older route is
        // wired rather than assumed unnecessary.
        assert!(
            DIALOG_SCRIPT.contains("navigator.clipboard.writeText"),
            "the clipboard is written through one route only"
        );
        assert!(
            DIALOG_SCRIPT.contains("document.execCommand(\"copy\")"),
            "a page that is not a secure context has no way to copy"
        );
        // The control reports what the press did. A copy button that looks the same whether it
        // wrote the clipboard or not is a button nobody can trust.
        assert!(
            DIALOG_SCRIPT.contains("data-copied"),
            "the control does not show what the press did"
        );
    }

    #[test]
    fn the_copy_of_an_answer_is_assembled_from_nodes_like_the_answer_itself() {
        // The words being copied came from a model. Building a copy control out of a template
        // string would put them back into markup, which is the hole the renderer exists to avoid.
        let start = DIALOG_SCRIPT
            .find("// ---- Taking the answer out of the page")
            .expect("the copy section exists");
        let section = &DIALOG_SCRIPT[start..];
        let end = section
            .find("\n  function renderTurn(")
            .expect("the section ends where the turn begins");
        let section = &section[..end];
        assert!(
            !section.contains("innerHTML") && !section.contains("insertAdjacentHTML"),
            "the copy controls assemble markup"
        );
        // What is copied is the model's own source, not the drawn nodes: the person pastes it
        // somewhere that reads Markdown, and re-serialising the DOM would be a second renderer.
        // The block is now drawn one element per line, so the text no longer arrives in one
        // assignment — but the control still receives `source` itself, which is the property that
        // matters: what a person copies must not be the boxes the block was laid out with.
        assert!(
            section.contains("code.textContent = source")
                || (section.contains("copyControl(") && section.contains("source,")),
            "the block is not written as text, or the control does not receive the source"
        );
    }

    #[test]
    fn the_answer_is_laid_out_as_blocks_and_not_one_run_on_paragraph() {
        // The renderer built the right nodes and the stylesheet still drew them wrong: the reset at
        // the top of `dialog.css` sets `display` on every descendant and its initial value is
        // `inline`. Measured in Chrome against the running binary, `P`, `H4`, `UL`, `OL` and `LI`
        // all computed to `inline` — the answer's paragraphs ran together, its headings were the
        // size of the text around them, and its lists lost their bullets while keeping their words.
        // So every element the renderer creates says what it is, and this pins the ones that were
        // missing rather than all of them one at a time.
        for selector in [
            "#deepnest-dialog .deepnest-said p",
            "#deepnest-dialog .deepnest-said ul",
            "#deepnest-dialog .deepnest-said ol",
            "#deepnest-dialog .deepnest-said h3",
            "#deepnest-dialog .deepnest-said li",
        ] {
            assert!(DIALOG_STYLE.contains(selector), "{selector} has no layout");
        }
        assert!(
            DIALOG_STYLE.contains("display: list-item"),
            "a list item is not a list item, so it draws no marker"
        );
        // And the `li` carries its own numbering: the reset gives it `disc` whatever the `ol`
        // around it says, so a numbered list drew bullets until this rule existed. The selector and
        // the declaration are asserted separately, and that is not laziness: an assertion written
        // across the line break between them passed on macOS and Linux and failed on Windows, where
        // the file is checked out with CRLF and the `{` is followed by `\r\n` rather than `\n`.
        assert!(
            DIALOG_STYLE.contains("ol > li {"),
            "no rule reaches the items of a numbered list"
        );
        assert!(
            DIALOG_STYLE.contains("list-style-type: decimal"),
            "a numbered list draws bullets"
        );
    }

    #[test]
    fn the_answer_under_the_box_is_drawn_as_blocks_while_it_is_written() {
        // The line under the box held the answer's own source. Measured in Chrome against a stream
        // that wrote an answer with a heading, a bold run, a fenced block and a numbered list: the
        // element held `##`, `**` and both fences as literal characters, and inside it there were
        // zero headings, zero bold runs, zero code blocks and zero lists — while the transcript
        // above drew the same text as blocks. So a person watched Markdown being typed and read it
        // as Markdown only once the turn had landed in the transcript.
        //
        // Two moments, one renderer: the pieces as they arrive and the finished answer. The route
        // that carries an answer and the route that carries a page's own lines are separate, because
        // "Rebuilding…" is not Markdown and drawing it as such would be a worse sentence.
        assert!(
            DIALOG_SCRIPT.contains("function sayAnswer("),
            "there is no path that draws an answer rather than a line"
        );
        assert!(
            DIALOG_SCRIPT.contains("sayAnswer(words);"),
            "the finished answer still goes through the plain line"
        );
        assert!(
            !DIALOG_SCRIPT.contains("say(text, false)"),
            "the answer is still being written as text"
        );
        // The answer line is drawn by the renderer the transcript uses, with no copy controls: the
        // transcript above already offers one per block, and a second set here would double the tab
        // stops this panel spends on copying. `null` is what says so.
        assert!(
            DIALOG_SCRIPT.contains("renderWords(written, null)"),
            "the answer is not drawn by the renderer"
        );
        assert!(
            DIALOG_SCRIPT.contains("said.className = \"deepnest-said\""),
            "the answer's blocks sit outside the stylesheet's markdown scope"
        );
        // Redrawing per piece would rebuild every block hundreds of times for one answer; dropping
        // the last would leave a partial answer on screen for good. Coalescing with a trailing draw
        // is the only shape that does neither.
        assert!(
            DIALOG_SCRIPT.contains("setTimeout(drawAnswer, 0)"),
            "nothing coalesces the redraws"
        );
        assert!(
            DIALOG_SCRIPT.contains("clearTimeout(redrawal)"),
            "a held-back draw can paint a partial answer over the finished one"
        );
        // A paragraph may not hold blocks, so the line itself stops being one.
        assert!(
            DIALOG_SCRIPT.contains(r#"var answer = document.createElement("div");"#),
            "the answer line is still a paragraph, which cannot hold a heading or a list"
        );
        // The renderer's own body must not copy the model's answer into a control here: the block
        // path that draws one is the path that names one.
        let block = DIALOG_SCRIPT
            .find("function fencedBlock(language, source, what, ordinal, total)")
            .expect("the fenced block exists");
        let body = &DIALOG_SCRIPT[block..];
        let body = &body[..body.find("\n  }").expect("it ends")];
        assert!(
            body.contains("if (what === null)"),
            "a block shown rather than copied still draws a copy control"
        );
        // And the stylesheet has to reach the answer's blocks wherever they are drawn, not only in
        // the transcript.
        for selector in [
            "#deepnest-dialog .deepnest-said p",
            "#deepnest-dialog .deepnest-said pre",
            "#deepnest-dialog .deepnest-said code",
        ] {
            assert!(DIALOG_STYLE.contains(selector), "{selector} has no layout");
        }
    }

    #[test]
    fn a_wrapped_line_of_code_reads_as_a_continuation_and_not_a_new_line() {
        // A fenced block wraps on purpose — the column is 19rem wide and a real line of Rust does
        // not fit — but every row used to begin at the block's own left edge. Measured in Chrome
        // against a five-line block whose third line was 122 characters: the browser drew seven rows
        // and all seven began at the same x, so one statement read as three, the last of them
        // starting mid-expression at `…sum::<i64>();`.
        //
        // One element per line, and the stylesheet pulls the first row back by what it pushes the
        // wrapped rows in, is what tells them apart.
        assert!(
            DIALOG_SCRIPT.contains("line.className = \"deepnest-code-line\""),
            "a line of a block has no element of its own to be laid out by"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#".split("\n")"#),
            "the block is still written as one text node"
        );
        // A blank line is a line: an empty block generates no line box, so it would vanish and the
        // code under it would move up into its place.
        assert!(
            DIALOG_SCRIPT.contains(r#"one === "" ? "\u200b" : one"#),
            "the blank lines of a block are dropped"
        );
        // The two halves of the hanging indent, and the three declarations `all: initial` costs.
        for needed in [
            "#deepnest-dialog .deepnest-said pre .deepnest-code-line",
            "#deepnest-dialog .deepnest-code pre .deepnest-code-line",
            "padding-left: 2ch",
            "text-indent: -2ch",
            "white-space: pre-wrap",
            "display: block",
        ] {
            assert!(DIALOG_STYLE.contains(needed), "{needed} is missing");
        }
        // The block is drawn the same way in the transcript and under the box, so the rule has to
        // reach both places — a continuation that only reads right in one of them is half a fix.
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog .deepnest-code pre"),
            "the under-the-box block is not covered by the line rule"
        );
    }

    #[test]
    fn the_answer_being_written_keeps_its_newest_line_in_view() {
        // The answer line is its own scroll window (`max-height: 9rem`, `overflow: auto`), and it did
        // not follow what it was writing. Measured in Chrome against a twenty-piece stream: the box
        // sat on its first line for the whole answer, and the newest line ended 459 px below a 144 px
        // window — 603 px of answer written while the reader could see the first quarter of it and
        // nothing that came after.
        //
        // Three positions, and the third is the one that must not be lost: a box that pulls its own
        // scroll to the end is a box a person cannot read while the agent works.
        assert!(
            DIALOG_SCRIPT.contains("function onTheNewest()"),
            "the box cannot tell whether the reader is on its newest line"
        );
        assert!(
            DIALOG_SCRIPT.contains("answer.scrollTop + answer.clientHeight >= answer.scrollHeight - 8"),
            "the newest line is measured some other way, or demanding an exact pixel a scrolled box \
             rarely lands on"
        );
        // While the answer is being written, the newest line — and the reading is taken per piece, so
        // a reader who leaves mid-answer is left alone from that piece on.
        let more = DIALOG_SCRIPT
            .find("function typeMore(piece)")
            .expect("the writer exists");
        let body = &DIALOG_SCRIPT[more..];
        let body = &body[..body.find("\n  }").expect("it ends")];
        assert!(
            body.contains("placement = onTheNewest() ? NEWEST : HOLD;"),
            "the answer is written without following its own newest line, or without yielding"
        );
        // A finished answer opens at its first line, which is where every version before this one
        // left it: the box is read top-down once there is nothing more coming.
        let whole = DIALOG_SCRIPT
            .find("function sayAnswer(text)")
            .expect("the finished path exists");
        let body = &DIALOG_SCRIPT[whole..];
        let body = &body[..body.find("\n  }").expect("it ends")];
        assert!(
            body.contains("placement = FIRST;"),
            "a finished answer is left scrolled wherever the stream ended"
        );
        // And the redraw puts the reader where the mode says, all three ways.
        let draw = DIALOG_SCRIPT
            .find("function drawAnswer()")
            .expect("the draw exists");
        let body = &DIALOG_SCRIPT[draw..];
        let body = &body[..body.find("\n  }").expect("it ends")];
        for move_line in [
            "answer.scrollTop = answer.scrollHeight;",
            "answer.scrollTop = 0;",
            "answer.scrollTop = held;",
        ] {
            assert!(
                body.contains(move_line),
                "{move_line} is missing from the redraw"
            );
        }
        // Replacing the children throws the old position away, so the held one is read first.
        assert!(
            body.find("var held = answer.scrollTop;").unwrap()
                < body.find("answer.textContent = \"\";").unwrap(),
            "the position to hold is read after the blocks that made it are gone"
        );
    }

    #[test]
    fn a_list_drawn_under_an_item_stays_under_it() {
        // The renderer took every consecutive bullet line into one list, whatever its indent and
        // whatever its marker. Measured in Chrome against the running binary, on an answer whose
        // steps carried their own sub-points: six `li` in one `ol`, every one of them `decimal`, and
        // no list inside a list anywhere in the answer. So "1. read the file, 2. the header row, 3.
        // the first data row, 4. fix the import" is what a person read back, and the numbering was
        // this page's invention rather than the model's.
        assert!(
            DIALOG_SCRIPT.contains("var BULLET = /^([ \\t]*)([-*+]|\\d+[.)])\\s+(.*)$/;"),
            "a list line is read without the indent that says what it sits under"
        );
        let words = DIALOG_SCRIPT
            .find("function renderWords(text, what)")
            .expect("the renderer exists");
        let body = &DIALOG_SCRIPT[words..];
        let body = &body[..body.find("\n  }").expect("it ends")];
        // The shape is a stack rather than a second renderer: a line either joins the list it is as
        // far in as, opens a list inside the item above it, or closes the lists it is out of.
        assert!(
            body.contains("open.pop()"),
            "a line further out never closes the list it left"
        );
        assert!(
            body.contains("open[open.length - 1].at > at"),
            "the indent is read but nothing does anything about it"
        );
        assert!(
            body.contains("parent.list.lastChild.appendChild(list)"),
            "a sub-list is drawn beside its item rather than inside it"
        );
        // The other marker at the same depth is a second list, not a second item.
        assert!(
            body.contains("innermost.ordered !== ordered"),
            "a bullet under a number is drawn as a numbered step"
        );
        // Both directions: a bulleted list inside a numbered item, and a numbered list inside a
        // bulleted one.
        for selector in [
            "#deepnest-dialog .deepnest-said li > ul",
            "#deepnest-dialog .deepnest-said li > ol",
        ] {
            assert!(DIALOG_STYLE.contains(selector), "{selector} has no layout");
        }
    }

    #[test]
    fn a_table_in_an_answer_is_drawn_as_a_table() {
        // A comparison table is the shape an agent answers with more often than any other, and no
        // branch in the renderer caught one: every row of it fell through to the paragraph branch,
        // which joins its lines with a space. Measured in Chrome against the running binary, on a
        // reply holding a three-column table of four rows — one paragraph, 320 px wide and 88 px
        // tall, whose text ran `| Model | Price per 1M input | Verdict | |---|--:|:--:| | kimi-k2.6 |
        // 4.00 | …`: twenty pipe characters in a single run of prose, the delimiter row included.
        // Nothing on the page was a `table`, so no row lined up under another and the verdict for
        // one model sat beside the price of the next. After: one `table` 320 px wide, twelve
        // `table-cell`s, four rows at four `y` offsets and three columns at three `x` offsets.
        assert!(
            DIALOG_SCRIPT.contains("function tableRule(line)"),
            "the line under a header is never read as a delimiter row, so no table is recognised"
        );
        assert!(
            DIALOG_SCRIPT.contains("function tableCells(line)"),
            "a row is never split into the cells it is made of"
        );
        let words = DIALOG_SCRIPT
            .find("function renderWords(text, what)")
            .expect("the renderer exists");
        let body = &DIALOG_SCRIPT[words..];
        let body = &body[..body.find("\n  }").expect("it ends")];
        assert!(
            body.contains("var align = tableHere(lines, i);"),
            "there is a cell splitter but no branch that draws a table"
        );
        // The delimiter row is scaffolding rather than content: skipping it along with the header
        // is what keeps `|---|--:|:--:|` out of the answer.
        assert!(
            body.contains("i += 2;"),
            "the delimiter row is drawn as a row of the table"
        );
        // A model writes "the numbers are:" and the table on the very next line, and a paragraph
        // that kept reading would swallow the header row before the loop above ever saw it.
        assert!(
            body.contains("!tableHere(lines, i)"),
            "a table written straight under a sentence is read as part of that sentence"
        );
        assert!(
            DIALOG_SCRIPT.contains("\"deepnest-table-\" + align"),
            "a column's alignment is read and then dropped"
        );
        for selector in [
            "#deepnest-dialog .deepnest-said table",
            "#deepnest-dialog .deepnest-said tr",
            "#deepnest-dialog .deepnest-said th",
        ] {
            assert!(DIALOG_STYLE.contains(selector), "{selector} has no layout");
        }
        // The reset at the top of the stylesheet reaches every one of these and puts it back to
        // `inline`, which draws a table as one run of words with its cells side by side. The
        // declarations are asserted one at a time because an assertion written across the line
        // break between a selector and its body failed on Windows, where the file is checked out
        // with CRLF.
        for display in [
            "display: table;",
            "display: table-header-group;",
            "display: table-row;",
            "display: table-cell;",
        ] {
            assert!(
                DIALOG_STYLE.contains(display),
                "{display} is missing, so the parts are drawn inline and the rows run together"
            );
        }
        assert!(
            DIALOG_STYLE.contains("border-collapse: collapse"),
            "the reset leaves `separate`, so every cell is drawn with its own four borders"
        );
    }

    #[test]
    fn the_message_box_sends_on_enter_and_keeps_shift_enter_for_a_new_line() {
        // The box a person types into constantly, and the key every agent front end sends with.
        // Measured in Chrome with a real key press, on the box holding "fix the failing tests":
        // `Enter` reached the textarea as a `keydown`, the browser turned it into a line break
        // (`beforeinput:insertLineBreak`), the form was submitted **0** times and the stub saw **0**
        // chat requests. So the sentence stayed in the box and a second line appeared under it.
        // After: the same press submits **1** time, posts exactly one request carrying that
        // message, and no line break is inserted.
        assert!(
            DIALOG_SCRIPT.contains("input.addEventListener(\"keydown\""),
            "the message box has no key handler, so Enter cannot send"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"if (event.key !== "Enter" || event.shiftKey) {"#),
            "Enter is not the sending key, or Shift+Enter is taken away from the new line"
        );
        // An input method's Enter belongs to the input method. Without this guard a person writing
        // Chinese sends half a word the moment they accept a candidate — the host does write
        // Chinese, so the guard is the difference between the key working and the key losing text.
        assert!(
            DIALOG_SCRIPT.contains("if (event.isComposing || event.keyCode === 229) {"),
            "an Enter that is choosing an input method's candidate would send the message"
        );
        // `requestSubmit` rather than a click on the button: the button is disabled for the length
        // of a turn, and clicking it would turn the key off for exactly the minutes it is used.
        assert!(
            DIALOG_SCRIPT.contains("form.requestSubmit()"),
            "Enter does not go through the form's own send path"
        );
        // Measured after the fix: with a turn running, `send.disabled` is true and the key is left
        // to the browser, so it is a line break rather than a send that the server would refuse.
        assert!(
            DIALOG_SCRIPT.contains("if (send.disabled) {"),
            "Enter during a turn would post a second request instead of leaving the key alone"
        );
        // The handler has to reach the send path, and the send path is the submit handler below it:
        // a key handler defined after it would still work, but one that named the route itself
        // would be a second place that knows what sending means. This pins the arrangement the
        // comment above describes.
        let keyed = DIALOG_SCRIPT
            .find("input.addEventListener(\"keydown\"")
            .expect("the handler exists");
        let submitted = DIALOG_SCRIPT
            .find("form.addEventListener(\"submit\"")
            .expect("the send path exists");
        assert!(
            keyed < submitted,
            "the Enter handler sits after the submit handler it is supposed to route into"
        );
    }

    #[test]
    fn up_in_an_empty_box_brings_back_the_message_just_sent() {
        // The box is emptied the moment a message goes, and before this the sentence could not be
        // reached again from it. Measured in Chrome with a real key press, after a message had been
        // sent and the stub had it: `Up` on the empty box left it empty (`value: ""`). After: the
        // same key returns `{"value":"now the second one","caret":18}` — the message, with the caret
        // at its end — and a second `Up` reaches the one before it
        // (`{"value":"fix the failing tests","caret":21}`).
        assert!(
            DIALOG_SCRIPT.contains("var sentByHand = [];"),
            "the dialog keeps no record of what was sent, so there is nothing to bring back"
        );
        assert!(
            DIALOG_SCRIPT.contains("function remember(text)"),
            "a sent message is never written down"
        );
        // Written where the message leaves, not where the answer arrives: a turn that fails still
        // leaves the person wanting the sentence back to fix and resend.
        let remembered = DIALOG_SCRIPT
            .find("remember(text)")
            .expect("the message is remembered");
        let answered = DIALOG_SCRIPT
            .find("if (result.status === 200 && result.body")
            .expect("the answer path exists");
        assert!(
            remembered < answered,
            "a message is only remembered once its answer arrived, so a failed turn loses it"
        );
        assert!(
            DIALOG_SCRIPT
                .contains(r#"if (event.key !== "ArrowUp" && event.key !== "ArrowDown") {"#),
            "the box does not listen for the two keys that walk its own history"
        );
        // A half-typed paragraph must survive a stray arrow key. Measured against a box holding
        // "half a thought": the browser itself kept the text, and this guard is what keeps a walk
        // from replacing it.
        assert!(
            DIALOG_SCRIPT.contains(r#"if (input.value !== "" && !recalled) {"#),
            "the history walk takes the arrow keys away from a message being written"
        );
        // Without the flag the walk stops after one step: the box is no longer empty, so the guard
        // above would refuse every `Up` after the first. Measured: two presses returned the same
        // message and never reached the one before it.
        assert!(
            DIALOG_SCRIPT.contains("var recalled = false;"),
            "a recalled message is indistinguishable from one the person typed, so the walk stops"
        );
        assert!(
            DIALOG_SCRIPT.contains("recalled = text !== \"\";"),
            "putting a message back does not record that it may be walked past"
        );
        // The person's own edit ends the walk; a recalled message must not reset its own position,
        // and setting `value` from script does not fire this event.
        assert!(
            DIALOG_SCRIPT.contains("input.addEventListener(\"input\""),
            "an edited box keeps a stale walk position, so the next Up goes somewhere unexpected"
        );
        // The caret belongs at the end. Position 0 is what setting `value` leaves behind, and it
        // puts the next word typed in front of the sentence that was just recalled.
        assert!(
            DIALOG_SCRIPT.contains("input.setSelectionRange(end, end)"),
            "a recalled message leaves the caret at its start"
        );
    }

    #[test]
    fn an_exchange_can_be_found_in_a_conversation_the_window_does_not_reach() {
        // The transcript draws the newest twenty turns, so in a long conversation the rest is not
        // in the page at all. Measured in Chrome against forty exchanges: the panel opened with
        // twenty drawn, the first exchange on screen was number 21, and the browser's own find for
        // a phrase in exchange 7 answered `false` — a page cannot be searched for words it does not
        // contain. Two hundred exchanges took nine presses of "Show the N earlier messages" to
        // reach. After: typing the phrase into the field answered
        // `{"drawn":1,"note":"Showing 1 of 40 exchanges","wantedInPage":true}`, and a phrase nothing
        // says answered "No exchange matches that."
        assert!(
            DIALOG_SCRIPT.contains(r#"find.id = "deepnest-dialog-find";"#),
            "there is no way to look for an exchange in a conversation the window does not reach"
        );
        // A search is not a page: with a phrase in the field the window is not applied, or the
        // answer would be "not in the twenty on screen", which is a different answer from "not in
        // the conversation" and the wrong one to give.
        assert!(
            DIALOG_SCRIPT.contains("recent = numbered.filter(function (entry) {"),
            "a search only looks through the turns the window is already drawing"
        );
        // The window itself stays, for the reading that is not a search.
        assert!(
            DIALOG_SCRIPT.contains("recent = numbered.slice(-shownTurns);"),
            "the transcript no longer draws a window when nothing is being searched for"
        );
        // The memo decides whether the transcript is redrawn at all, so a phrase that does not
        // change what is drawn still has to change the answer under it.
        assert!(
            DIALOG_SCRIPT.contains(r#""|" + finding;"#),
            "typing more of a phrase that matches the same turns does not redraw the count"
        );
        // Measured: on a conversation of twenty — the window is the whole of it — the field was
        // not on screen at all (`findShown: false`), and after a search was cleared it went away
        // again. A search box over a conversation that fits is furniture.
        assert!(
            DIALOG_SCRIPT.contains("find.hidden = older === 0 && !finding;"),
            "the find field is offered even when the window withholds nothing"
        );
        // A searched-for exchange keeps the number it has in the conversation. Without that the
        // transcript numbers a match "exchange 1" because it is first on screen, and the drawer
        // calls the same turn by its real number.
        assert!(
            DIALOG_SCRIPT.contains(
                "renderTurn(entry.turn, live && entry.number === numbered.length, entry.number)"
            ),
            "a found exchange is numbered by its place on screen rather than in the conversation"
        );
        // The panel closes on Escape through a handler on the document. Measured in Chrome with a
        // real key press while the field held a phrase: after the field took the key, the panel was
        // still open (`panelOpen: true`) and the window was back (twenty drawn). Without the stop,
        // the one key that means "clear the search" is the key that takes the whole panel away.
        let stopped = DIALOG_SCRIPT
            .find("event.stopPropagation();")
            .expect("the field stops the key it handles");
        let closes = DIALOG_SCRIPT
            .find(r#"if (event.key === "Escape" && !panel.hidden) {"#)
            .expect("the panel closes on Escape");
        assert!(
            stopped < closes,
            "the find field's Escape reaches the panel's own handler, so clearing a search closes \
             the panel"
        );
    }

    #[test]
    fn the_box_empties_when_the_message_goes_rather_than_when_the_answer_comes() {
        // The box used to keep the message for the whole length of a turn, because the clearing sat
        // in the branch that runs when the answer arrives. Two things followed, both measured in
        // Chrome against a stub holding the chat POST open for eight seconds: the box read as "not
        // sent" while the turn ran (`value` still held the message at T+1s), and a sentence typed
        // into it during the turn was deleted by the answer — `value` went from "build me a page
        // that lists the invoices and add a total row" to `""` the moment the reply landed. After:
        // the box is empty at T+0.3s, and the same typed sentence is still there after the answer.
        let cleared = DIALOG_SCRIPT
            .find("input.value = \"\";")
            .expect("the box is emptied somewhere");
        let asked = DIALOG_SCRIPT
            .find("ask(text, picturesReferredTo(text), filesReferredTo(text))")
            .expect("the request is made somewhere");
        assert!(
            cleared < asked,
            "the box is emptied only once the answer arrived, so the panel reads as not sent for \
             the whole turn and anything typed into it meanwhile is deleted"
        );
        // The one thing that clearing early costs: a turn that could not be sent would take the
        // sentence with it, so every exit that is not an answer has to give it back.
        assert!(
            DIALOG_SCRIPT.contains("function giveBack(text)"),
            "a message whose turn was refused is lost from the box, and the reload one of those \
             refusals asks for would lose it for good"
        );
        // Measured, both refusals: `{"value":"build me a page that lists the invoices","caret":39}`
        // after a `500` carrying `{"error":"the model refused this one"}` and after a `401`.
        assert_eq!(
            DIALOG_SCRIPT.matches("giveBack(text);").count(),
            3,
            "the refusals that give the message back are not the three that leave without an \
             answer — and the one that must not is the stream that died mid-answer, because that \
             turn did run and a box holding the message again asks it a second time"
        );
        // The box is emptied when the message leaves, so a person may have typed the next sentence
        // by the time a turn fails. That sentence is theirs.
        assert!(
            DIALOG_SCRIPT.contains("if (input.value.trim() !== \"\") {"),
            "the given-back message replaces a sentence the person typed while the turn failed"
        );
        // Not `putBack`: that one records the text as a recalled message, so the next `Up` would
        // walk over a sentence the person is about to fix and resend.
        let given = DIALOG_SCRIPT
            .find("function giveBack(text)")
            .expect("the message is given back");
        let walked = DIALOG_SCRIPT
            .find("recalled = text !== \"\";")
            .expect("a recalled message is recorded");
        assert!(
            walked < given,
            "the message that comes back from a failed turn is marked recalled, so `Up` replaces \
             it instead of protecting it"
        );
        // The inner name shadowed the outer one for the whole callback, which is why no failure
        // exit below it could have reached the message that was sent.
        assert!(
            !DIALOG_SCRIPT.contains("var text = result.body.text;"),
            "the answer's own text shadows the message the turn was asked"
        );
        // The caret belongs after what came back, or the next word typed lands in front of it.
        assert!(
            DIALOG_SCRIPT.contains("function caretAtTheEnd()"),
            "a message given back leaves the caret at its start"
        );
    }

    #[test]
    fn the_drawer_lists_the_conversations_and_says_which_one_the_next_message_would_join() {
        // The gap this closes: an app had one conversation and no way to start a clean second one
        // without deleting the first. The page's part is the list and the choice, and the choice
        // has to be visible — a drawer that draws rows without marking the open one leaves the
        // person guessing where their next sentence lands.
        assert!(
            DIALOG_SCRIPT.contains("deepnest-dialog-sessions-toggle"),
            "there is no way into the conversations"
        );
        assert!(
            DIALOG_SCRIPT.contains("deepnest-dialog-sessions-list"),
            "the drawer has no list"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"setAttribute("aria-current""#),
            "the open conversation is drawn but not announced"
        );
        assert!(
            DIALOG_SCRIPT.contains("deepnest-dialog-sessions-new"),
            "there is no way to start a conversation"
        );
        // One route, two verbs, and the body is what differs — the answer to both is the list, so
        // the two choices cannot end up rendering two different things.
        assert!(
            DIALOG_SCRIPT.contains("function sessionsRoute()"),
            "the drawer does not name its route"
        );
        for asked in [r#"{ open: id }"#, r#"{ new: true }"#] {
            assert!(
                DIALOG_SCRIPT.contains(asked),
                "the page never asks for {asked}"
            );
        }
        // The transcript belongs to the conversation it was read from: switching empties it and
        // asks again rather than appending a second conversation under the first.
        let adopt = DIALOG_SCRIPT
            .find("function adopt(body)")
            .expect("the switch is not written");
        let body = &DIALOG_SCRIPT[adopt..];
        let body = &body[..body
            .find(
                "
  }",
            )
            .expect("adopt ends")];
        assert!(
            body.contains("transcript.textContent = \"\""),
            "the transcript of the session being left stays on screen: {body}"
        );
        assert!(
            body.contains("readHistory()"),
            "the new conversation is never read: {body}"
        );
        // And built as nodes: a title is the person's own first sentence. The assignment forms and
        // not the bare word, because this file's headers name `innerHTML` while forbidding it.
        assert!(
            !DIALOG_SCRIPT.contains(".innerHTML") && !DIALOG_SCRIPT.contains("innerHTML ="),
            "the drawer puts somebody's words into markup"
        );
    }

    #[test]
    fn every_part_of_the_drawer_is_styled() {
        // The dialog's stylesheet opens with `all: initial`, so an element it forgets is not
        // "unstyled" but invisible: a `button` loses its border, its padding and its size.
        for selector in [
            "#deepnest-dialog-head",
            "#deepnest-dialog-sessions-toggle",
            "#deepnest-dialog-sessions",
            "#deepnest-dialog-sessions[hidden]",
            "#deepnest-dialog-sessions-list",
            "#deepnest-dialog-sessions .deepnest-session",
            "#deepnest-dialog-sessions .deepnest-session-current",
            "#deepnest-dialog-sessions .deepnest-session-title",
            "#deepnest-dialog-sessions .deepnest-session-meta",
            "#deepnest-dialog-sessions-new",
        ] {
            assert!(
                DIALOG_STYLE.contains(selector),
                "{selector} is unstyled, which under `all: initial` means invisible"
            );
        }
        // The hidden attribute has to be spelled out for the same reason.
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog-sessions[hidden]"),
            "the closed drawer stays on screen"
        );
    }

    #[test]
    fn the_dialog_reads_the_conversation_back_instead_of_remembering_it() {
        // The defect this route fixes: a turn that rewrites the app reloads the page, and the
        // answer the person was reading was a variable that died with it. So the transcript is
        // asked for, from the route the server scopes a read-only cookie to.
        assert!(
            DIALOG_SCRIPT.contains(r#"/history"#),
            "the box never asks what the app's agent has already said"
        );
        assert!(
            DIALOG_SCRIPT.contains("fetch(historyRoute()"),
            "the transcript has to be fetched from its own route"
        );
        // A read: no method, no body. A `POST` here would mean the box could change something
        // merely by opening, and it is the one route on this page that must not.
        let read = &DIALOG_SCRIPT[DIALOG_SCRIPT
            .find("fetch(historyRoute()")
            .expect("just asserted")..];
        let read = &read[..read.find("})").expect("the fetch call ends")];
        assert!(
            !read.contains("method"),
            "the transcript request names a method, so it is not the read it claims: {read}"
        );
        assert!(!read.contains("body"), "a read has no body: {read}");
        assert!(
            read.contains(r#"credentials: "same-origin""#),
            "without the cookie the route answers 401: {read}"
        );
    }

    #[test]
    fn the_dialog_says_what_a_running_turn_is_doing_and_stops_when_it_ends() {
        // The measured defect: one turn ran 112 seconds and 24 rounds and the page said a single
        // word for all of it, so a turn that had rewritten the app read as a turn that never ran.
        // The reader is the transcript route, re-read on a timer for as long as the request is out
        // — nothing new is stored and the server is not told it is being watched.
        assert!(
            DIALOG_SCRIPT.contains("PROGRESS_MS"),
            "nothing re-reads the turn while it runs"
        );
        assert!(
            DIALOG_SCRIPT.contains("setInterval(readHistory"),
            "the timer does not read the transcript, so it cannot say how far the turn has got"
        );
        // The two numbers that separate "working" from "stuck": the rounds spent and the calls made.
        assert!(
            DIALOG_SCRIPT.contains("last.rounds"),
            "the round count is not shown"
        );
        assert!(
            DIALOG_SCRIPT.contains("last.tools"),
            "the tool count is not shown"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#""Working — round ""#)
                || DIALOG_SCRIPT.contains("\"Working — round \""),
            "the line does not name the round it is on"
        );
        // Every exit from a turn goes through `stopPolling` — the answer, the refusal, the expired
        // grant, the dead server — so that is where the watcher has to stop. A watcher that
        // outlived its turn would poll a file nothing is writing.
        let stop = &DIALOG_SCRIPT[DIALOG_SCRIPT
            .find("function stopPolling()")
            .expect("the box stops its timers somewhere")..];
        let stop = &stop[..stop.find("\n  }").expect("the function ends")];
        assert!(
            stop.contains("stopWatching()"),
            "stopPolling does not stop the progress watcher: {stop}"
        );
        // A line that changes every second must not be announced every second: the progress line
        // turns the live region off, and the answer that finally matters turns it back on.
        assert!(
            DIALOG_SCRIPT.contains(r#"setAttribute("aria-live", "off")"#),
            "a line that rewrites itself every second is announced every second"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"setAttribute("aria-live", "polite")"#),
            "the answer never gets its announcement back"
        );
        // `all: initial` leaves both of them invisible without a rule of their own.
        for selector in ["#deepnest-dialog-answer.live", ".deepnest-running"] {
            assert!(DIALOG_STYLE.contains(selector), "{selector} is unstyled");
        }
    }

    #[test]
    fn the_dialog_offers_a_stop_only_while_a_turn_is_running() {
        // The measured defect: a turn ran 112 seconds and 24 rounds, and the only thing the page
        // could do about it was wait. The stop reaches the loop on its own cookie, through the
        // route the server mints beside the four it already hands the page.
        assert!(
            DIALOG_SCRIPT.contains("deepnest-dialog-stop"),
            "the box has no stop control"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"type = "button""#),
            "a stop inside the form that submitted it would start a second turn"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#""/cancel""#),
            "the control does not post to the route that stops a turn"
        );
        // It starts hidden and is put away again by `stopWatching`, which every exit from a turn
        // goes through — so a page never shows a stop for a turn that has already ended.
        assert!(
            DIALOG_SCRIPT.contains("stop.hidden = true;"),
            "the control is never hidden"
        );
        let watching = &DIALOG_SCRIPT[DIALOG_SCRIPT
            .find("function stopWatching()")
            .expect("the box stops its watcher somewhere")..];
        let watching = &watching[..watching.find("\n  }").expect("the function ends")];
        assert!(
            watching.contains("stop.hidden = true"),
            "a finished turn leaves its stop button on screen: {watching}"
        );
        let started = &DIALOG_SCRIPT[DIALOG_SCRIPT
            .find("function watchTheTurn()")
            .expect("the box watches a turn somewhere")..];
        let started = &started[..started.find("\n  }").expect("the function ends")];
        assert!(
            started.contains("stop.hidden = false"),
            "a running turn offers no way out of it: {started}"
        );
        // `all: initial` resets `display`, so the attribute alone would leave it visible.
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog-stop[hidden]"),
            "the control is unstyled, so hiding it does nothing"
        );
    }

    #[test]
    fn the_dialog_reads_the_answer_as_it_is_written() {
        // The measured shape of the defect: a turn spends most of its minutes in tools and then
        // writes its answer, and the page held nothing until the whole body arrived — so the end of
        // a long turn and a turn that had hung read the same. The route has answered as an event
        // stream since it was written; what was missing was the reader.
        assert!(
            DIALOG_SCRIPT.contains(r#"Accept: STREAM"#),
            "the box never asks for the answer as it is written"
        );
        assert!(
            DIALOG_SCRIPT.contains(r#"var STREAM = "text/event-stream""#),
            "the stream's own type is not named, so nothing can ask for or recognise it"
        );
        // Read as it arrives, not collected and then parsed: the reader and the decoder are the two
        // pieces that make a piece of an answer visible while the rest is still being written.
        for piece in ["getReader()", "TextDecoder", "decoder.decode("] {
            assert!(
                DIALOG_SCRIPT.contains(piece),
                "the stream is not read as it arrives — {piece} is missing"
            );
        }
        // The three kinds the route writes, and the fact that only `text` is the answer: `done` and
        // `error` carry the reply the JSON path would have given whole, and both end the turn.
        for kind in [r#""text""#, r#""done""#, r#""error""#] {
            assert!(
                DIALOG_SCRIPT.contains(kind),
                "the reader does not know the {kind} event"
            );
        }
        // Which reading applies is the *reply's* content type, never the request's: a refusal is
        // answered with a JSON body even when the page asked for events, and reading that as a
        // stream would turn the sentence saying what to do into a parse error.
        assert!(
            DIALOG_SCRIPT.contains(r#"response.headers.get("content-type")"#),
            "the box decides how to read the answer by something other than what came back"
        );
        assert!(
            DIALOG_SCRIPT.contains("response.json()"),
            "the refusal path was removed, so a 401 or a 412 has nothing to read it"
        );
        // The progress line and the answer share one element, so the guard is what keeps a counter
        // from overwriting the model's sentence once it has started writing it.
        let progress = &DIALOG_SCRIPT[DIALOG_SCRIPT
            .find("function showProgress(")
            .expect("the box says what a turn is doing somewhere")..];
        let progress = &progress[..progress.find("\n  }").expect("the function ends")];
        assert!(
            progress.contains("if (typing)"),
            "the progress line overwrites an answer mid-sentence: {progress}"
        );
        // Every way out of a turn has to put the line back, or the next turn's progress never
        // appears: the settle handler, the failed request, and nothing else.
        let submit = &DIALOG_SCRIPT[DIALOG_SCRIPT
            .find(r#"form.addEventListener("submit""#)
            .expect("the box submits somewhere")..];
        let submit = &submit[..submit.find("\n  });").expect("the listener ends")];
        assert!(
            submit.matches("typing = false;").count() >= 2,
            "a turn that ended leaves the box believing an answer is still being written"
        );
        // A line that grows word by word is not announced word by word.
        assert!(
            submit.contains(r#"readEvents(response, typeMore)"#),
            "the deltas are read but never shown"
        );
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog-answer.typing"),
            "an answer still being written is unstyled, so nothing says more is coming"
        );
    }

    #[test]
    fn the_dialog_says_what_went_wrong_instead_of_failing_silently() {
        // Three failures a user can hit, each with a different next step, and the box has to name
        // the step: the grant expired, the server is gone, and the answer was a refusal.
        assert!(DIALOG_SCRIPT.contains("reload the page"));
        assert!(
            DIALOG_SCRIPT.contains("result.status === 401"),
            "the expired capability has to be recognised by its own status: without that test the \
             box falls through to the server's raw refusal and never names the reload that mints \
             another grant"
        );
        assert!(DIALOG_SCRIPT.contains("did not reach deepnest"));
        assert!(DIALOG_SCRIPT.contains("result.body.error"));
    }

    #[test]
    fn the_dialog_builds_its_box_without_inner_html() {
        // The agent's answer is model output, and a slug is a directory name: both are text.
        // The assignment forms, not the bare word: this file's own header names `innerHTML` while
        // explaining why it is not used, and an assertion on the word would fail on that sentence
        // rather than on a mistake.
        assert!(
            !DIALOG_SCRIPT.contains(".innerHTML"),
            "text from a model or a filesystem must not become markup"
        );
        assert!(
            !DIALOG_SCRIPT.contains("innerHTML ="),
            "text from a model or a filesystem must not become markup"
        );
        assert!(DIALOG_SCRIPT.contains("textContent"));
        // And the stylesheet has to style the box it builds: `all: initial` leaves a textarea
        // with no border and no size, so a box with no rule would be invisible.
        for selector in [
            "#deepnest-dialog-input",
            "#deepnest-dialog-send",
            "#deepnest-dialog-answer",
        ] {
            assert!(DIALOG_STYLE.contains(selector), "{selector} is unstyled");
        }
    }

    #[test]
    fn the_dialog_shows_the_widening_request_and_answers_it_only_when_clicked() {
        // The route, what it asks, the reason it gives, and the answer — all four, or the person is
        // clicking a button whose object they cannot see.
        assert!(
            DIALOG_SCRIPT.contains("/escalation"),
            "the dialog never asks for a question"
        );
        assert!(
            DIALOG_SCRIPT.contains("pending"),
            "it does not read the question"
        );
        assert!(
            DIALOG_SCRIPT.contains("justification"),
            "the reason the agent gave is not shown"
        );
        assert!(DIALOG_SCRIPT.contains("approve"), "it never answers");
        // Buttons, not a timer: nothing in this file may answer without a person.
        assert!(
            DIALOG_SCRIPT.contains("disable") || DIALOG_SCRIPT.contains("disabled"),
            "a second click during the answer is not guarded against"
        );
        // Polling while the turn is in flight, and stopped when it is not: a poll that outlives the
        // turn is a page asking for a question nobody is asking.
        assert!(DIALOG_SCRIPT.contains("setInterval"), "nothing polls");
        assert!(
            DIALOG_SCRIPT.contains("clearInterval"),
            "a poll outlives the turn it was started for"
        );
        for selector in [
            "#deepnest-dialog-asked",
            "#deepnest-dialog-asked-what",
            "#deepnest-dialog-asked-why",
            "#deepnest-dialog-allow",
            "#deepnest-dialog-refuse",
        ] {
            assert!(DIALOG_STYLE.contains(selector), "{selector} is unstyled");
        }
        // `all: initial` leaves a button with no border and no size, so a hidden block that becomes
        // visible needs its own rule for `[hidden]` as well as for itself.
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog-asked[hidden]"),
            "the question cannot be hidden again"
        );
    }

    #[test]
    fn a_refusal_carries_the_words_the_person_typed_and_an_approval_does_not() {
        // `Answer::Denied { reason }` reaches the model as "They said: …", and an empty reason
        // leaves it reading a bare refusal — the one answer it cannot act on, since `Denied` and
        // `Unanswered` are kept apart precisely because the next move differs. So there has to be
        // a field to type into, and the route has to receive it.
        assert!(
            DIALOG_SCRIPT.contains("deepnest-dialog-reason"),
            "there is nowhere to say why"
        );
        assert!(
            DIALOG_STYLE.contains("#deepnest-dialog-reason"),
            "the field is unstyled, and `all: initial` left it with no border and no size"
        );
        // The body is built once, from a verdict that may or may not carry the words. Pinned as a
        // shape rather than as a word, so the read is "the reason reaches the request" and not
        // "the string 'reason' appears somewhere in this file".
        assert!(
            DIALOG_SCRIPT.contains("JSON.stringify(verdict)"),
            "the answer is not built from the verdict"
        );
        assert!(
            DIALOG_SCRIPT.contains("verdict.reason"),
            "the words never reach the request"
        );
        // And only on a refusal, only when there are some: the route's verdict refuses a key it
        // does not know, so an approval must not carry a `reason` at all.
        assert!(
            DIALOG_SCRIPT.contains("if (!approved)") && DIALOG_SCRIPT.contains("if (whyText)"),
            "a reason is sent with an approval, or an empty one is sent as if it were words"
        );
        // A reason left in the box would answer the next question with the previous refusal.
        assert!(
            DIALOG_SCRIPT.contains(r#"whyNot.value = """#),
            "the field keeps the last refusal"
        );
    }

    #[test]
    fn the_dialog_watches_the_route_that_reports_a_rebuild() {
        // The dialog's own copy of the route, beside the panel's: both are strings on this side and
        // verbs on the server's, so the only thing that can keep them equal is a line like this.
        // `build` is the route a turn that changed the app leaves behind, and the dialog is what
        // turns its answer into a reload.
        assert!(
            DIALOG_SCRIPT.contains("/build"),
            "the dialog never asks whether the rebuild finished, so a turn that changed the app \
             leaves the page showing the binary from before it"
        );
        assert!(DIALOG_SCRIPT.contains("location.reload()"));
    }

    #[test]
    fn the_panel_asks_for_the_update_check_outside_the_markup() {
        // The check is a request per skill to somebody else's server, so the page must not be
        // waiting on it: the script asks after the markup is already readable, and the answer is
        // appended rather than rendered. Both halves are asserted, because either one alone would
        // pass while the panel still blocked on a network it does not need.
        assert!(
            SCRIPT.contains("/api/skills/updates"),
            "the panel never asks whether a newer skill release exists"
        );
        assert!(
            !PAGE.contains("/api/skills/updates"),
            "the update check is rendered into the markup, which makes the page wait for a network"
        );
        assert!(
            PAGE.contains("id=\"skills-section\""),
            "the script appends into a section the markup does not have"
        );
    }

    #[test]
    fn the_panel_asks_for_a_refresh_with_a_post_that_names_one_skill() {
        // A refresh runs a program on the user's machine, so the script must ask for it with a verb a
        // browser never prefetches and a body naming the skill: a link would make the request by
        // being pointed at, and one button that refreshed everything would act on a set nobody saw.
        assert!(
            SCRIPT.contains("/api/skills/refresh"),
            "the panel has no way to ask for a refresh, so the route is unreachable from the product"
        );
        assert!(
            SCRIPT.contains("JSON.stringify({ name: name })"),
            "the refresh is asked for without naming the skill it is about"
        );
        // The server answers `202` and the publisher takes as long as the package does, so the script
        // has to read the same route back for the outcome.
        assert!(
            SCRIPT.matches("/api/skills/refresh").count() >= 2,
            "the script asks for a refresh and never reads its outcome back"
        );
    }

    #[test]
    fn the_script_calls_the_routes_the_server_has() {
        // Cheap guard against a script and a router drifting apart: every path the script fetches
        // is one this repository serves.
        for path in [
            "/api/health",
            "/api/apps",
            "/api/config",
            "/api/skills/updates",
            "/api/skills/refresh",
        ] {
            assert!(
                SCRIPT.contains(path),
                "the script never calls {path}, so either it is dead or this test is stale"
            );
        }
        // And the one route the markup names is the one the server answers.
        assert_eq!(SCRIPT_PATH, "/panel.js");
    }

    #[test]
    fn a_long_conversation_can_be_read_back_to_its_first_exchange() {
        // The server sends every turn the app has, and the transcript draws a window of them. The
        // window is what a long session needs; a window nothing can move is the first exchange —
        // the one that says what the app was asked to be — drawn nowhere and said to be nowhere.
        // `numbered` rather than `turns`: the slice is taken over the conversation with each turn
        // carrying its own number, which is what lets a searched-for exchange keep it. The window
        // itself is the same window, and this test is about the window.
        assert!(
            DIALOG_SCRIPT.contains("numbered.slice(-shownTurns)"),
            "the transcript draws a fixed slice, so nothing can reach past it"
        );
        assert!(
            DIALOG_SCRIPT.contains("shownTurns += TURNS_SHOWN;"),
            "the window has no way to grow, so the oldest turns stay unreachable"
        );
        assert!(
            DIALOG_SCRIPT.contains("deepnest-dialog-earlier"),
            "the turns the window has not reached are offered by no control"
        );
        assert!(
            DIALOG_SCRIPT.contains("earlier message"),
            "the control does not say how much of the conversation is behind it"
        );
        assert!(
            !DIALOG_SCRIPT.contains("turns.slice(-TURNS_SHOWN)"),
            "one call site still draws the fixed window the control cannot move"
        );

        // Reaching them adds turns above the ones on screen, and the reader is looking at one of
        // those: the box grows under them, so the place is held against what was added.
        assert!(
            DIALOG_SCRIPT.contains("holdingPlace = true;"),
            "revealing earlier turns redraws without marking where they were added"
        );
        assert!(
            DIALOG_SCRIPT.contains("wasTop + (transcript.scrollHeight - wasTall)"),
            "a prepend restores the offset from the top, which scrolls the reader up"
        );

        // And a conversation left for another one starts at the window, not at whatever the last
        // one had been opened out to.
        assert!(
            DIALOG_SCRIPT.matches("shownTurns = TURNS_SHOWN;").count() >= 2,
            "switching conversations keeps the previous one's window"
        );

        // The control is a button inside a box whose every element starts at `all: initial`, which
        // resets `display` along with everything else: without a `display` of its own it is an
        // inline button that cannot fill the line it sits on.
        let rule = DIALOG_STYLE
            .split("#deepnest-dialog-earlier {")
            .nth(1)
            .expect("the earlier-turns control has no rule of its own");
        let rule = &rule[..rule.find('}').expect("the rule never closes")];
        assert!(
            rule.contains("display: block"),
            "the control declares no display, so `all: initial` leaves it inline"
        );
        assert!(
            DIALOG_STYLE.contains(".deepnest-turn:first-of-type"),
            "the first turn drawn still matches `:first-child`, which the control broke"
        );
    }
}
