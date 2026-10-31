//! The rewrites that let an app believe it is still served from `/`.
//!
//! Every function here is pure and takes the prefix it works under, so what the forward does to a
//! page or a header can be read and tested without a socket. That is the point: the interesting
//! failures of a proxy are all rewriting failures, and a rewriting rule you cannot test in
//! isolation is a rewriting rule nobody checks.
//!
//! One rule holds across all of them: **leave alone what is already right.** An app that was
//! written against the `/app/<slug>/` contract and already emits prefixed URLs is a supported app;
//! prefixing its output a second time would break exactly the app that read the documentation.

/// The path prefix an app is served under: `/app/<slug>`.
pub fn prefix(slug: &str) -> String {
    format!("/app/{slug}")
}

/// The path the app itself should see.
///
/// `/app/notes/api/items` → `/api/items`, and `/app/notes` or `/app/notes/` → `/`. The app never
/// learns its prefix, which is what makes an ordinary app work unchanged: it wrote `/api/items`
/// and that is what arrives.
pub fn upstream_path(prefix: &str, path: &str) -> String {
    match path.strip_prefix(prefix) {
        Some("") | Some("/") => "/".to_string(),
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        // Anything else is a path that merely starts with the same characters (`/app/notes2`),
        // which is a different app's prefix or no prefix at all. The caller has already matched
        // the slug segment, so this is not reachable through the router — it is here so that the
        // function has no case that silently guesses.
        _ => "/".to_string(),
    }
}

/// A `Location` header, moved under the prefix.
///
/// Three shapes arrive, and only one of them is ours to change:
///
/// - `/login` — an absolute path on this origin. It leaves the prefix unless it is rewritten, and
///   the user lands on deepnest's own 404. Prefixed.
/// - `https://example.com/x` and `//example.com/x` — another origin. Left alone: rewriting it
///   would send the browser to a path on this machine that nobody asked for.
/// - `login` — relative, so it resolves under the prefix already. Left alone.
pub fn location(prefix: &str, value: &str) -> String {
    if value.starts_with("//") || value.contains("://") {
        return value.to_string();
    }
    if !value.starts_with('/') {
        return value.to_string();
    }
    if value == prefix || value.starts_with(&format!("{prefix}/")) {
        return value.to_string();
    }
    format!("{prefix}{value}")
}

/// A `Set-Cookie` header, confined to the app's prefix.
///
/// A cookie with `Path=/` is sent to every route on the port, deepnest's own API included: an app
/// could shadow the panel's cookies for free by name collision. A cookie with no `Path` at all gets
/// the default `/` from the browser, so it is confined by adding one.
pub fn set_cookie(prefix: &str, value: &str) -> String {
    let mut parts = value.split(';');
    let first = parts.next().unwrap_or_default().trim_end();
    let mut attributes: Vec<String> = parts.map(|part| part.to_string()).collect();

    let under = format!("{prefix}/");
    let mut found = false;
    for attribute in attributes.iter_mut() {
        let trimmed = attribute.trim_start();
        if trimmed.len() < 5 || !trimmed[..5].eq_ignore_ascii_case("path=") {
            continue;
        }
        found = true;
        let path = &trimmed[5..];
        if path == prefix || path.starts_with(&under) {
            continue;
        }
        // `/` becomes the prefix itself, and any other path is appended to it, so `/api` and `/`
        // stay distinguishable from each other.
        let moved = if path.starts_with('/') {
            format!("{prefix}{path}")
        } else {
            format!("{under}{path}")
        };
        *attribute = format!(" Path={moved}");
    }

    if !found {
        attributes.push(format!(" Path={under}"));
    }

    let mut out = String::with_capacity(value.len() + prefix.len() + 8);
    out.push_str(first);
    for attribute in attributes {
        out.push(';');
        if !attribute.starts_with(' ') {
            out.push(' ');
        }
        out.push_str(&attribute);
    }
    out
}

/// The page with a `<base href>` naming its prefix.
///
/// Without it, a page served at `/app/notes/` that says `<script src="./main.js">` asks for
/// `/app/notes/main.js` — correct — but one served at `/app/notes` (no trailing slash) asks for
/// `/app/main.js`, and one that says `<img src="logo.png">` resolves against whatever the address
/// bar happens to hold. The base element makes the answer independent of the request's spelling.
///
/// `None` means "leave the body exactly as it was", and it is returned in two cases: the body is
/// not HTML, and the page already declares a base. A page that declares one has made a decision
/// about its own URLs, and this forward is not the place to overrule it.
pub fn inject_base(prefix: &str, body: &str) -> Option<String> {
    let lowered = body.to_ascii_lowercase();
    if lowered.contains("<base ") || lowered.contains("<base>") {
        return None;
    }
    let element = format!("<base href=\"{prefix}/\">");

    // Inside `<head>` is where a base belongs and where it also applies to the body below it.
    if let Some(head) = lowered.find("<head") {
        if let Some(open_end) = lowered[head..].find('>') {
            let at = head + open_end + 1;
            let mut out = String::with_capacity(body.len() + element.len());
            out.push_str(&body[..at]);
            out.push_str(&element);
            out.push_str(&body[at..]);
            return Some(out);
        }
    }
    // No head: after `<html …>` if there is one, so the base is still ahead of every URL, and
    // otherwise at the very start.
    if let Some(html) = lowered.find("<html") {
        if let Some(open_end) = lowered[html..].find('>') {
            let at = html + open_end + 1;
            let mut out = String::with_capacity(body.len() + element.len());
            out.push_str(&body[..at]);
            out.push_str(&element);
            out.push_str(&body[at..]);
            return Some(out);
        }
    }
    Some(format!("{element}{body}"))
}

/// Headers that belong to one hop and must not be forwarded.
///
/// `Connection` names the ones in play for this hop, and the rest are the fixed list from RFC 9110
/// §7.6.1. Forwarding them is how a proxy turns a `Connection: close` into a client that waits for
/// a keep-alive connection that will never come. `Content-Length` is in the list for a different
/// reason: the body is re-read and re-measured here, so a forwarded length is a second opinion
/// that can only ever disagree.
pub fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> String {
        prefix("notes")
    }

    #[test]
    fn the_prefix_strips_and_what_is_left_is_what_the_app_wrote() {
        assert_eq!(upstream_path(&at(), "/app/notes/"), "/");
        assert_eq!(upstream_path(&at(), "/app/notes"), "/");
        assert_eq!(upstream_path(&at(), "/app/notes/api/items"), "/api/items");
        assert_eq!(upstream_path(&at(), "/app/notes/main.js"), "/main.js");
        // A deeper path keeps its shape: the app owns everything past its own prefix.
        assert_eq!(upstream_path(&at(), "/app/notes/a/b/c"), "/a/b/c");
    }

    #[test]
    fn a_path_that_only_starts_like_the_prefix_is_not_stripped_into_nonsense() {
        // `/app/notes2` is not this app. The router never routes it here, and the function still
        // has an answer rather than a guess.
        assert_eq!(upstream_path(&at(), "/app/notes2/x"), "/");
    }

    #[test]
    fn an_absolute_location_moves_under_the_prefix() {
        assert_eq!(location(&at(), "/login"), "/app/notes/login");
        assert_eq!(location(&at(), "/"), "/app/notes/");
        assert_eq!(
            location(&at(), "/api/items?x=1"),
            "/app/notes/api/items?x=1"
        );
    }

    #[test]
    fn an_already_prefixed_or_foreign_location_is_left_alone() {
        for value in [
            "/app/notes/login",
            "/app/notes/",
            "/app/notes",
            "https://example.com/x",
            "//example.com/x",
            "login",
            "",
        ] {
            assert_eq!(location(&at(), value), value, "{value}");
        }
    }

    #[test]
    fn a_cookie_meant_for_the_whole_port_is_confined_to_the_app() {
        // The case this exists for: `Path=/` would be sent to deepnest's own API too.
        assert_eq!(
            set_cookie(&at(), "session=abc; Path=/; HttpOnly"),
            "session=abc; Path=/app/notes/; HttpOnly"
        );
        assert_eq!(
            set_cookie(&at(), "session=abc; Path=/api"),
            "session=abc; Path=/app/notes/api"
        );
    }

    #[test]
    fn a_cookie_with_no_path_gets_one() {
        // Without a Path the browser defaults to `/`, which is the same leak by another route.
        assert_eq!(set_cookie(&at(), "id=1"), "id=1; Path=/app/notes/");
        assert_eq!(
            set_cookie(&at(), "id=1; HttpOnly; SameSite=Lax"),
            "id=1; HttpOnly; SameSite=Lax; Path=/app/notes/"
        );
    }

    #[test]
    fn a_cookie_already_confined_is_not_confined_twice() {
        for value in [
            "session=abc; Path=/app/notes/",
            "session=abc; Path=/app/notes/api",
            "session=abc; path=/app/notes/",
        ] {
            assert_eq!(set_cookie(&at(), value), value, "{value}");
        }
    }

    #[test]
    fn a_cookie_value_containing_a_semicolon_is_not_mistaken_for_an_attribute() {
        // Only the first segment is the pair; everything after it is attributes. A quoted value
        // with a `;` keeps working because the split is on the raw separator, as browsers do it.
        let rewritten = set_cookie(&at(), "prefs=a:b; Path=/");
        assert_eq!(rewritten, "prefs=a:b; Path=/app/notes/");
    }

    #[test]
    fn the_page_gets_a_base_naming_its_prefix() {
        let page = "<!doctype html><html><head><title>Notes</title></head><body></body></html>";
        let rewritten = inject_base(&at(), page).unwrap();
        assert!(
            rewritten.contains("<head><base href=\"/app/notes/\"><title>"),
            "{rewritten}"
        );
        // Nothing else moved.
        assert!(rewritten.contains("<!doctype html><html>"), "{rewritten}");
        assert!(rewritten.ends_with("</body></html>"), "{rewritten}");
    }

    #[test]
    fn a_head_with_attributes_still_takes_the_base() {
        let page = "<html><head lang=\"en\"><script src=\"main.js\"></script></head></html>";
        let rewritten = inject_base(&at(), page).unwrap();
        assert!(
            rewritten.contains("<head lang=\"en\"><base href=\"/app/notes/\"><script"),
            "{rewritten}"
        );
    }

    #[test]
    fn a_page_with_no_head_gets_the_base_before_anything_that_names_a_url() {
        let page = "<html><body><img src=\"logo.png\"></body></html>";
        let rewritten = inject_base(&at(), page).unwrap();
        assert!(
            rewritten.contains("<html><base href=\"/app/notes/\"><body>"),
            "{rewritten}"
        );
    }

    #[test]
    fn a_fragment_with_no_html_at_all_is_still_placed_first() {
        assert_eq!(
            inject_base(&at(), "<p>hello</p>").unwrap(),
            "<base href=\"/app/notes/\"><p>hello</p>"
        );
    }

    #[test]
    fn a_page_that_declares_its_own_base_is_left_alone() {
        // The app made a decision about its URLs. Overruling it is how a forward breaks the one
        // app that read the contract.
        for page in [
            "<html><head><base href=\"/app/notes/\"></head></html>",
            "<html><head><base target=\"_blank\"></head></html>",
            "<html><head><BASE HREF=\"/x/\"></head></html>",
        ] {
            assert_eq!(inject_base(&at(), page), None, "{page}");
        }
    }

    #[test]
    fn the_hop_by_hop_names_are_recognised_in_any_spelling() {
        for name in [
            "Connection",
            "KEEP-ALIVE",
            "Proxy-Authenticate",
            "te",
            "Trailer",
            "Transfer-Encoding",
            "Upgrade",
            "Content-Length",
        ] {
            assert!(is_hop_by_hop(name), "{name}");
        }
        for name in [
            "Content-Type",
            "Location",
            "Set-Cookie",
            "Cache-Control",
            "ETag",
        ] {
            assert!(!is_hop_by_hop(name), "{name}");
        }
    }
}
