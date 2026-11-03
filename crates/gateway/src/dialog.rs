//! The floating dialog, and the page it stands on: deepnest's own, with the app framed inside it.
//!
//! The dialog belongs to deepnest, not to the app. An agent writes the app's page and rewrites it
//! wholesale whenever the user asks for something, so anything the agent could have put in the
//! page is something the agent can delete by accident. The ball is the only way back to the control
//! panel from a page the user is looking at, so it is added here, at the last moment before the
//! bytes go out, where nothing the app does to its own files can reach it.
//!
//! **A page is two documents, and that is the fix for a defect the injection could not reach**
//! (`host 15:33`). Injecting the dialog into the app's page put the conversation and the app in one
//! document, so every finished rebuild — which is most turns, because a turn that writes a file
//! marks the app changed — reloaded the whole tab and took the dialog with it: the panel closed, the
//! conversation went off screen, and the person had to open it again to read the answer they had
//! just been given. A reload cannot spare a subtree of the document it replaces. So the app now
//! lives in a frame and the dialog lives beside it: [`stage`] is the page the tab holds, the app is
//! what the frame holds, and a rebuild reloads the frame. What the person sees is the app coming
//! back — which is what they asked for — and a conversation that never moved.
//!
//! [`inject`] is still here for a client that sends no `Sec-Fetch-Dest` and so cannot be told from
//! a top-level navigation: that client gets the older shape, a dialog inside the app's page, and its
//! rebuild still costs the conversation. It is a fallback with a known cost, not a second design.
//!
//! # Why the policy has to move with it
//!
//! An app that sets `Content-Security-Policy` is doing the right thing, and a dialog injected into
//! its page needs a script and a stylesheet. If the policy says `script-src 'none'`, the browser
//! refuses to load `/dialog.js` and the ball never appears — silently, which is the worst shape of
//! this failure because the page looks fine. So the policy is rewritten, and the rules for
//! rewriting it are these:
//!
//! - **Only when the dialog actually went in.** A `text/html` answer whose policy is rewritten
//!   while its body carries no dialog is a policy loosened for nothing.
//! - **Never narrower than what the app allowed.** A directive that has to be invented — because
//!   the app wrote `default-src` and no `script-src` — is invented with `default-src`'s own
//!   sources plus `'self'`, not with `'self'` alone: `default-src 'unsafe-inline'` already allowed
//!   inline script, and answering it with `script-src 'self'` would take that away.
//! - **`'none'` cannot stand beside anything.** It is only legal as the sole source, and a policy
//!   that lists it with others is one browsers resolve by ignoring it. So it is replaced rather
//!   than joined, and the result says what it means.
//!
//! `'self'` is the coarsest spelling that works: the dialog's assets are on the same origin as the
//! app, and a path-scoped source expression is not portable. It widens nothing across the boundary
//! this design draws — every app already shares deepnest's origin, and what separates them is the
//! prefix and the token, not the origin.

/// Where the dialog's script is served.
pub const SCRIPT_PATH: &str = "/dialog.js";

/// Where the dialog's stylesheet is served.
pub const STYLE_PATH: &str = "/dialog.css";

/// The id of the frame the app is shown in.
///
/// Named in one place because three files have to agree on it: the stage writes it, the dialog
/// script finds it, and the stylesheet lays it out.
pub const FRAME_ID: &str = "deepnest-app-frame";

/// The class the stage's own `<body>` carries, so the stylesheet can tell the stage's document from
/// an app's: both load `dialog.css`, and a rule that resets a body's margin must reach exactly one
/// of them.
pub const STAGE_CLASS: &str = "deepnest-stage";

/// The policy the stage is served with.
///
/// The stage is deepnest's own page, so its policy is deepnest's own: narrow, and about the two
/// things it does — load the dialog and hold one frame of the same origin. An app's policy has no
/// business here, and copying it in would let an app's `frame-src 'none'` take away the frame its
/// own page is shown in.
pub const STAGE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                             frame-src 'self'; connect-src 'self'; img-src 'self' data:; \
                             base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The page the tab holds while an app is open: the dialog's own document, with the app framed.
///
/// `path` is the app's URL as the browser asked for it — the prefix and the query included — so the
/// frame loads the same address the tab did. The frame's request carries `Sec-Fetch-Dest: iframe`
/// and is answered with the app; this page's carried `document` and is answered with this. That
/// one header is the whole distinction, which is what makes a deep link into an app still work: a
/// person who opens `/app/notes/settings` gets a stage whose frame is `/app/notes/settings`.
pub fn stage(slug: &str, path: &str) -> String {
    let slug = escape(slug);
    // A path reaches this from a request line, so it is escaped for the attribute it is written
    // into. `&` first, or the escapes of the others would be escaped twice.
    let src = escape(path);
    format!(
        "<!doctype html>\
         <html lang=\"en\">\
         <head>\
         <meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"{META_NAME}\" content=\"{slug}\">\
         <title>{slug}</title>\
         <link rel=\"stylesheet\" href=\"{STYLE_PATH}\">\
         </head>\
         <body class=\"{STAGE_CLASS}\">\
         <iframe id=\"{FRAME_ID}\" src=\"{src}\" title=\"{slug}\"></iframe>\
         <script src=\"{SCRIPT_PATH}\" defer></script>\
         </body>\
         </html>"
    )
}

/// A policy that lets the page it is served with be framed.
///
/// An app is shown inside the stage's frame, and a `frame-ancestors` directive is the one way an
/// app can refuse that: the app's own page would load into an empty frame, with the policy sitting
/// in a header nobody reads. So the directive is dropped and every other directive is left exactly
/// as the app wrote it — this removes a refusal, it does not grant anything.
pub fn framable(value: &str) -> String {
    let directives = parse(value);
    if directives
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("frame-ancestors"))
    {
        return write(
            directives
                .into_iter()
                .filter(|(name, _)| !name.eq_ignore_ascii_case("frame-ancestors"))
                .collect(),
        );
    }
    value.to_string()
}

/// The `<meta>` the injected markup carries.
///
/// It is how the page states that a dialog is already in it, and how the script learns which app it
/// is standing on. The name is prefixed for the same reason the panel's token `<meta>` is: an app
/// may have a `<meta name="app">` of its own and must not be mistaken for this one.
pub const META_NAME: &str = "deepnest-app";

/// The markup injected into a page.
///
/// A `<meta>`, a stylesheet and a deferred script, and no element for them to fill: the ball is
/// built by the script, so a page that never runs it is a page with no dialog rather than a page
/// with an empty button. The paths are absolute because the page carries a `<base href>` naming the
/// app's prefix, and a relative one would resolve inside the app — where nothing is served.
pub fn markup(slug: &str) -> String {
    format!(
        "<meta name=\"{META_NAME}\" content=\"{}\">\
         <link rel=\"stylesheet\" href=\"{STYLE_PATH}\">\
         <script src=\"{SCRIPT_PATH}\" defer></script>",
        escape(slug)
    )
}

/// The page with the dialog added, or `None` when it needs nothing.
///
/// `None` means the page already carries one: a page is served through exactly one hop today, but
/// the marker is what makes a second hop harmless rather than a second ball.
///
/// The last `</body>` is where it goes, so a page that has one keeps its dialog after everything it
/// rendered itself. A page with no `</body>` — a fragment, or a document that never closed — gets it
/// at the end: the design asks for this in every page an app answers, and a page the browser will
/// happily wrap in a body is still a page.
pub fn inject(slug: &str, page: &str) -> Option<String> {
    // Lowercased once, for the two case-insensitive searches below. `to_ascii_lowercase` maps only
    // ASCII bytes, so every index it finds is an index into the original page as well.
    let lowered = page.to_ascii_lowercase();
    if lowered.contains(&format!("name=\"{META_NAME}\"")) {
        return None;
    }
    let element = markup(slug);
    let mut out = String::with_capacity(page.len() + element.len());
    match lowered.rfind("</body>") {
        Some(at) => {
            out.push_str(&page[..at]);
            out.push_str(&element);
            out.push_str(&page[at..]);
        }
        None => {
            out.push_str(page);
            out.push_str(&element);
        }
    }
    Some(out)
}

/// A `Content-Security-Policy` that lets the dialog load, and lets nothing else go.
///
/// The directives this widens are the two the dialog needs — a script and a stylesheet — and the
/// two that override them when an app is being specific (`script-src-elem`, `style-src-elem`).
/// A `-elem` directive that is not there is not invented: it only exists to override its parent,
/// and a parent this function has already widened needs no override.
pub fn policy(value: &str) -> String {
    if value.trim().is_empty() {
        return value.to_string();
    }
    let mut directives = parse(value);

    // What an invented directive has to allow as well as `'self'`: everything `default-src` already
    // allowed, because a directive that is absent is governed by it.
    let fallback = directives
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("default-src"))
        .map(|(_, sources)| sources.clone());

    for wanted in ["script-src", "style-src"] {
        match find(&mut directives, wanted) {
            Some(sources) => allow_self(sources),
            None => {
                if let Some(sources) = &fallback {
                    let mut widened = sources.clone();
                    allow_self(&mut widened);
                    directives.push((wanted.to_string(), widened));
                }
            }
        }
    }
    for specific in ["script-src-elem", "style-src-elem"] {
        if let Some(sources) = find(&mut directives, specific) {
            allow_self(sources);
        }
    }

    write(directives)
}

/// A policy as its directives, in the order it stated them.
fn parse(value: &str) -> Vec<(String, Vec<String>)> {
    value
        .split(';')
        .filter_map(|part| {
            let mut words = part.split_whitespace();
            let name = words.next()?;
            Some((
                name.to_string(),
                words.map(|source| source.to_string()).collect(),
            ))
        })
        .collect()
}

/// The directives as a policy again.
fn write(directives: Vec<(String, Vec<String>)>) -> String {
    let mut out = String::with_capacity(160);
    for (index, (name, sources)) in directives.iter().enumerate() {
        if index > 0 {
            // One space after the separator, which is how a person writes a policy and how the
            // one the app sent most likely read.
            out.push_str("; ");
        }
        out.push_str(name);
        for source in sources {
            out.push(' ');
            out.push_str(source);
        }
    }
    out
}

/// The sources of one directive, matched by name in any spelling.
fn find<'a>(
    directives: &'a mut [(String, Vec<String>)],
    name: &str,
) -> Option<&'a mut Vec<String>> {
    directives
        .iter_mut()
        .find(|(directive, _)| directive.eq_ignore_ascii_case(name))
        .map(|(_, sources)| sources)
}

/// Add `'self'` to a source list, and drop a `'none'` that cannot survive beside it.
fn allow_self(sources: &mut Vec<String>) {
    sources.retain(|source| !source.eq_ignore_ascii_case("'none'"));
    if !sources
        .iter()
        .any(|source| source.eq_ignore_ascii_case("'self'"))
    {
        sources.push("'self'".to_string());
    }
}

/// Escape a value for a double-quoted attribute.
///
/// A slug is validated before it reaches the forward, so today this escapes nothing. It is here
/// because the rule is about the point of writing rather than about today's input: a value that
/// reaches a page is escaped where it is written.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialog_goes_in_at_the_end_of_the_body() {
        let page = "<!doctype html><html><body><p>hi</p></body></html>";
        let injected = inject("notes", page).unwrap();
        assert!(
            injected.contains(
                "<p>hi</p><meta name=\"deepnest-app\" content=\"notes\">\
                 <link rel=\"stylesheet\" href=\"/dialog.css\">\
                 <script src=\"/dialog.js\" defer></script></body>"
            ),
            "{injected}"
        );
    }

    #[test]
    fn a_page_with_no_body_still_gets_one_at_the_end() {
        // A fragment is still a page the browser will wrap, and the design asks for this in every
        // page an app answers.
        assert_eq!(
            inject("notes", "<p>hi</p>").unwrap(),
            "<p>hi</p><meta name=\"deepnest-app\" content=\"notes\">\
             <link rel=\"stylesheet\" href=\"/dialog.css\">\
             <script src=\"/dialog.js\" defer></script>"
        );
    }

    #[test]
    fn the_last_body_close_is_the_one_used() {
        // A page whose own script mentions `</body>` is a page whose dialog belongs at the real
        // end, not inside a string literal halfway down the document.
        let page = "<html><body></body><script>var s = \"</body>\";</script></body></html>";
        let injected = inject("notes", page).unwrap();
        // Immediately before the *final* `</body>`: an insertion at the first one would leave the
        // dialog inside the document body, ahead of the app's own script.
        assert!(
            injected.ends_with("defer></script></body></html>"),
            "{injected}"
        );
        // Exactly one dialog, and it is the last one before the document ends.
        assert_eq!(injected.matches("deepnest-app").count(), 1, "{injected}");
    }

    #[test]
    fn a_page_that_already_carries_the_dialog_is_left_alone() {
        // Idempotence, so a second hop cannot produce a second ball.
        let page = "<html><body><meta name=\"deepnest-app\" content=\"notes\"></body></html>";
        assert_eq!(inject("notes", page), None);
    }

    #[test]
    fn an_app_that_happens_to_use_the_same_meta_spelling_is_still_given_one() {
        // The failure the `deepnest-` prefix exists to prevent is the opposite one — taking an
        // app's own `<meta name="app">` for ours — and this pins that the prefix is what counts.
        let page = "<html><body><meta name=\"app\" content=\"notes\"></body></html>";
        assert!(inject("notes", page).is_some());
    }

    #[test]
    fn a_slug_reaches_the_page_escaped() {
        let injected = inject("\"><script>alert(1)</script>", "<body></body>").unwrap();
        assert!(!injected.contains("<script>alert"), "{injected}");
        assert!(injected.contains("&quot;&gt;&lt;script&gt;"), "{injected}");
    }

    #[test]
    fn a_policy_that_already_allowed_scripts_keeps_what_it_allowed_and_gains_self() {
        // What a real app writes. `'unsafe-eval'` is left where it is: this function adds, it does
        // not judge.
        assert_eq!(
            policy("default-src 'self'; script-src 'unsafe-eval'"),
            "default-src 'self'; script-src 'unsafe-eval' 'self'; style-src 'self'"
        );
    }

    #[test]
    fn none_is_replaced_rather_than_joined() {
        // `'none'` beside another source is resolved by ignoring it, so joining it would leave a
        // policy that says one thing and means another.
        // And `style-src` is not invented beside it: with no `default-src` the app restricted
        // styles not at all, so adding a directive would newly restrict them.
        assert_eq!(policy("script-src 'none'"), "script-src 'self'");
        assert_eq!(
            policy("script-src 'none'; style-src 'none'"),
            "script-src 'self'; style-src 'self'"
        );
    }

    #[test]
    fn an_invented_directive_carries_what_default_src_allowed() {
        // The rule this test exists for: `default-src 'unsafe-inline'` allowed inline script, so
        // inventing `script-src 'self'` would be narrowing the app's policy while widening it.
        assert_eq!(
            policy("default-src 'unsafe-inline'"),
            "default-src 'unsafe-inline'; script-src 'unsafe-inline' 'self'; \
             style-src 'unsafe-inline' 'self'"
        );
    }

    #[test]
    fn a_policy_with_nothing_to_fall_back_on_is_left_alone() {
        // No `default-src` and no `script-src` means scripts are unrestricted: there is nothing to
        // loosen, and adding a directive would be narrowing.
        for value in ["img-src 'self'", "upgrade-insecure-requests"] {
            assert_eq!(policy(value), value, "{value}");
        }
    }

    #[test]
    fn an_elem_directive_is_widened_because_it_overrides_its_parent() {
        // `script-src-elem 'none'` wins over the `script-src` this function just widened, so a
        // policy that uses it would refuse the dialog anyway.
        assert_eq!(
            policy("script-src 'self'; script-src-elem 'none'"),
            "script-src 'self'; script-src-elem 'self'"
        );
    }

    #[test]
    fn directive_names_are_matched_in_any_spelling() {
        assert_eq!(
            policy("DEFAULT-SRC 'none'; Script-Src 'none'"),
            "DEFAULT-SRC 'none'; Script-Src 'self'; style-src 'self'"
        );
    }

    #[test]
    fn an_already_widened_policy_is_not_widened_twice() {
        let once = policy("default-src 'none'");
        assert_eq!(policy(&once), once);
    }

    #[test]
    fn an_empty_policy_stays_empty() {
        assert_eq!(policy(""), "");
        assert_eq!(policy("   "), "   ");
    }

    #[test]
    fn the_stage_is_a_page_of_deepnest_s_own_with_the_app_in_a_frame() {
        // The defect this shape exists for: a finished rebuild used to reload the tab, and a reload
        // cannot spare a subtree of the document it replaces — so the conversation went with it.
        let page = stage("notes", "/app/notes/");
        assert!(
            page.contains(r#"<iframe id="deepnest-app-frame""#),
            "{page}"
        );
        assert!(
            page.contains(r#"<script src="/dialog.js" defer></script>"#),
            "{page}"
        );
        assert!(
            page.contains(r#"<link rel="stylesheet" href="/dialog.css">"#),
            "{page}"
        );
        // The slug reaches the script the way it always has, through the meta the dialog reads.
        assert!(
            page.contains(r#"<meta name="deepnest-app" content="notes">"#),
            "{page}"
        );
        // And the app's own markup is not in this page at all: it is what the frame asks for.
        assert!(page.contains(r#"class="deepnest-stage""#), "{page}");
    }

    #[test]
    fn the_frame_loads_the_address_the_tab_asked_for() {
        // A deep link, a query and an app's own route all survive because none of them are read
        // here: the frame is told the address the browser used.
        let page = stage("notes", "/app/notes/settings?tab=advanced");
        assert!(
            page.contains(r#"src="/app/notes/settings?tab=advanced""#),
            "{page}"
        );
    }

    #[test]
    fn a_slug_or_a_path_reaches_the_stage_escaped() {
        // Both are written into attributes, and the path comes from a request line.
        let page = stage("\"><script>alert(1)</script>", "/app/x/\"onload=\"alert(1)");
        assert!(!page.contains("<script>alert"), "{page}");
        assert!(!page.contains(r#""onload=""#), "{page}");
        assert!(page.contains("&quot;"), "{page}");
    }

    #[test]
    fn the_stage_is_served_with_its_own_policy_and_it_holds_one_frame() {
        // An app's policy has no business governing the page the app is shown on: `frame-src 'none'`
        // there would take away the frame the app is in. So the stage carries deepnest's own, and
        // what it has to allow is exactly two things.
        assert!(STAGE_CSP.contains("script-src 'self'"), "{STAGE_CSP}");
        assert!(STAGE_CSP.contains("style-src 'self'"), "{STAGE_CSP}");
        assert!(STAGE_CSP.contains("frame-src 'self'"), "{STAGE_CSP}");
        assert!(STAGE_CSP.contains("connect-src 'self'"), "{STAGE_CSP}");
        assert!(STAGE_CSP.starts_with("default-src 'none'"), "{STAGE_CSP}");
    }

    #[test]
    fn an_app_that_refused_to_be_framed_loses_that_one_directive() {
        // `frame-ancestors` is the only way an app can refuse the stage, and refusing would load it
        // into an empty frame with the refusal in a header nobody reads. Everything else it said
        // stays exactly as it said it.
        assert_eq!(
            framable("default-src 'self'; frame-ancestors 'none'"),
            "default-src 'self'"
        );
        assert_eq!(
            framable("Frame-Ancestors 'none'; img-src 'self'"),
            "img-src 'self'"
        );
        // A policy that said nothing about framing is not rewritten at all: the same string comes
        // back, so a reader comparing the two sees no change to explain.
        for value in ["default-src 'self'", "", "  "] {
            assert_eq!(framable(value), value, "{value}");
        }
    }

    #[test]
    fn the_markup_names_the_two_paths_the_binary_has_to_serve() {
        // Cheap guard against a tag and a router drifting apart, mirroring the panel's own.
        let markup = markup("notes");
        assert!(markup.contains(SCRIPT_PATH), "{markup}");
        assert!(markup.contains(STYLE_PATH), "{markup}");
    }
}
