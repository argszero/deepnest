//! The floating dialog, injected into every page an app answers — and the one policy that has to
//! be loosened for it.
//!
//! The dialog belongs to deepnest, not to the app. An agent writes the app's page and rewrites it
//! wholesale whenever the user asks for something, so anything the agent could have put in the
//! page is something the agent can delete by accident. The ball is the only way back to the control
//! panel from a page the user is looking at, so it is injected here, at the last moment before the
//! bytes go out, where nothing the app does to its own files can reach it.
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
    let mut directives: Vec<(String, Vec<String>)> = value
        .split(';')
        .filter_map(|part| {
            let mut words = part.split_whitespace();
            let name = words.next()?;
            Some((
                name.to_string(),
                words.map(|source| source.to_string()).collect(),
            ))
        })
        .collect();

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

    let mut out = String::with_capacity(value.len() + 32);
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
    fn the_markup_names_the_two_paths_the_binary_has_to_serve() {
        // Cheap guard against a tag and a router drifting apart, mirroring the panel's own.
        let markup = markup("notes");
        assert!(markup.contains(SCRIPT_PATH), "{markup}");
        assert!(markup.contains(STYLE_PATH), "{markup}");
    }
}
