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
            DIALOG_SCRIPT.contains("ask(text, picturesReferredTo(text))"),
            "the request does not carry the pictures the words mention"
        );
        // The same body the route already reads, with one more key, and only when it is needed.
        assert!(DIALOG_SCRIPT.contains("body.images = images"));
        // Nothing left behind once the message is away, because a picture nobody chose to resend
        // is tokens spent on every later turn of the app's life.
        assert!(
            DIALOG_SCRIPT.contains("forgetPictures();"),
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
        // The words are used when they are there — the file has no text only sometimes.
        assert!(
            DIALOG_SCRIPT.contains("turn.user || (pictures > 0"),
            "an empty question is drawn as an empty line"
        );
        // And a question that had both says what came with it.
        assert!(DIALOG_SCRIPT.contains("\"with \" + picturesIn(pictures)"));
        assert!(DIALOG_SCRIPT.contains("deepnest-pictures"));
        // `all: initial` means an unstyled line inherits nothing, so it has to be in the sheet.
        assert!(DIALOG_STYLE.contains("#deepnest-dialog-transcript .deepnest-pictures"));
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
}
