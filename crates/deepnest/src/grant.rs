//! The one credential a page without the control token may hold.
//!
//! The floating dialog lives inside an app's page, and that page cannot carry the control token:
//! the app's own script shares this origin, so a token written into the page is a token the app
//! can read and spend — on every route there is. Without a credential of its own the dialog cannot
//! send a message, and the product's whole loop (the user says what they want, the agent writes it)
//! stops at the first step.
//!
//! So the capability is narrowed instead of the token being handed over. When this server answers
//! an app's page it mints a value and sets it as a cookie naming **one path** — that app's chat
//! route and nothing else. Five properties, each of which is load-bearing rather than decorative:
//!
//! - **Random, from the operating system's CSPRNG, and remembered here.** Not derived from the
//!   token: a derivation would need a hash this crate does not have and should not hand-roll, and
//!   remembering the value costs a `HashMap` entry. It also means the value cannot outlive the
//!   process that minted it.
//! - **`Path=/api/apps/<slug>/chat`.** The browser, not this code, is what confines it: the cookie
//!   is never sent to `/api/config`, to another app, or to the panel's own API.
//! - **`SameSite=Strict`.** Without it a page on another origin could make the request — it cannot
//!   read this cookie, but it does not need to read a cookie to have it sent — and spend the user's
//!   agent turns from a page the user never opened. This attribute is the CSRF defence, and the
//!   whole reason a cookie is acceptable here at all.
//! - **`HttpOnly`.** The app's script cannot read the value, so it cannot copy it anywhere or
//!   replay it from something that is not this browser.
//! - **A session cookie, with a lifetime this side enforces.** The browser forgets it when it
//!   closes; the server forgets it after [`TTL`], so a value copied out of a browser profile stops
//!   being useful on a clock this process controls.
//!
//! # What this does not protect against, stated rather than implied
//!
//! **The app's own script can make that request.** It shares the origin, so the cookie rides along
//! whether or not the script meant it, and an app can therefore spend its own agent turns. What it
//! cannot do is spend *another app's*, reach the config, start or stop anything, or read the
//! control token. That difference — one route, one app, one verb — is the whole point, and a design
//! that hid it would be claiming a boundary it does not have.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::http::Request;

/// The cookie's name.
///
/// Prefixed rather than generic, because an app may set a cookie called `session` and this one must
/// never be mistaken for it — or for one the app planted to be mistaken.
pub const COOKIE: &str = "deepnest-chat";

/// How long a minted value stays usable.
///
/// Long enough for a day's work in one tab, short enough that a value lifted out of a browser
/// profile is not a permanent key. The failure at the far end is cheap and named: the dialog answers
/// a `401` by telling the user to reload the page, which mints a new one.
pub const TTL: Duration = Duration::from_secs(8 * 60 * 60);

/// How much entropy a value carries before hex encoding. The same amount as the control token, for
/// the same reason: guessing it must be hopeless, not merely unlikely.
const BYTES: usize = 32;

/// The route the capability names — the same spelling the router routes on.
pub fn path(slug: &str) -> String {
    format!("/api/apps/{slug}/{}", crate::chat::ACTION)
}

/// The cookie header that installs `value` for `slug`'s chat route.
///
/// No `Max-Age` and no `Expires`: a session cookie, so closing the browser is enough to be rid of
/// it. [`TTL`] is what bounds it otherwise, and it is enforced on the way back in.
pub fn cookie(slug: &str, value: &str) -> String {
    format!(
        "{COOKIE}={value}; Path={}; HttpOnly; SameSite=Strict",
        path(slug)
    )
}

/// The value this request presents, if it presents one.
///
/// Read by hand rather than through a dependency: a `Cookie` header is a `;`-separated list of
/// `name=value`, and the whole of what is needed here is to find one name in it. Multiple `Cookie`
/// headers are not expected — a browser sends one — and the first is the one read.
pub fn presented(request: &Request) -> Option<&str> {
    for pair in request.header("cookie")?.split(';') {
        if let Some((name, value)) = pair.trim().split_once('=') {
            if name.trim() == COOKIE {
                return Some(value.trim());
            }
        }
    }
    None
}

/// Every value this process has minted and still honours.
///
/// `Clone` shares the map rather than copying it, because a router is cloned per connection and a
/// grant minted while serving a page has to be the one the next request is checked against.
#[derive(Debug, Clone)]
pub struct Grants {
    ttl: Duration,
    live: Arc<Mutex<HashMap<String, Holder>>>,
}

/// One minted value: which app it is for, and when it was issued.
#[derive(Debug, Clone)]
struct Holder {
    slug: String,
    issued: Instant,
}

impl Default for Grants {
    fn default() -> Self {
        Self::new()
    }
}

impl Grants {
    /// Grants that honour [`TTL`].
    pub fn new() -> Self {
        Self::with_ttl(TTL)
    }

    /// Grants with a lifetime the caller chooses.
    ///
    /// A constructor rather than a test hook: the lifetime is the one thing about expiry worth
    /// exercising, and a clock is not something a test can wind forward.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            live: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Mint a value for `slug`'s chat route, or `None` when this machine's random source failed.
    ///
    /// A word on failure: a page served without a grant renders and its dialog opens; only the box
    /// declines to send, and it says so. That is the fail-closed half of "do not invent a value" —
    /// a predictable one would be worse than none.
    pub fn mint(&self, slug: &str) -> Option<String> {
        let mut raw = [0u8; BYTES];
        getrandom::getrandom(&mut raw).ok()?;
        let mut value = String::with_capacity(BYTES * 2);
        for byte in raw {
            // `write!` into a `String` cannot fail, which is why the result is dropped rather than
            // unwrapped: there is no `io::Error` a `String` can produce.
            let _ = write!(value, "{byte:02x}");
        }

        let now = Instant::now();
        // `ok()?` on a poisoned lock: a thread that panicked mid-mint leaves no value anybody can
        // trust, so the page gets no grant rather than a map in an unknown state.
        let mut live = self.live.lock().ok()?;
        // Prune before inserting, so a process that has served pages all day holds the grants that
        // are still usable and not the ones that are not.
        live.retain(|_, holder| now.saturating_duration_since(holder.issued) < self.ttl);
        live.insert(
            value.clone(),
            Holder {
                slug: slug.to_string(),
                issued: now,
            },
        );
        Some(value)
    }

    /// Does `presented` name `slug`'s chat route, and is it still inside its lifetime?
    ///
    /// Looked up *by* the presented value rather than compared against a stored one byte by byte:
    /// the map hashes the whole string first, so there is no prefix whose comparison a caller could
    /// time. A value this process never minted is simply absent.
    pub fn admits(&self, slug: &str, presented: &str) -> bool {
        if presented.is_empty() {
            return false;
        }
        let Ok(live) = self.live.lock() else {
            return false;
        };
        let Some(holder) = live.get(presented) else {
            return false;
        };
        holder.slug == slug && Instant::now().saturating_duration_since(holder.issued) < self.ttl
    }

    /// How many values are held. Bounded by [`Grants::mint`]'s pruning, and read by the test that
    /// says so.
    pub fn len(&self) -> usize {
        self.live.lock().map(|live| live.len()).unwrap_or(0)
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::parse;

    fn request_with(cookie: &str) -> Request {
        parse(&format!(
            "POST /api/apps/notes/chat HTTP/1.1\r\nHost: 127.0.0.1:4270\r\nCookie: {cookie}\r\n\r\n"
        ))
        .unwrap()
    }

    #[test]
    fn a_minted_value_is_accepted_for_the_app_it_was_minted_for() {
        let grants = Grants::new();
        let value = grants.mint("notes").unwrap();
        assert!(grants.admits("notes", &value));
        // And not for another app, which is the whole reason the app is recorded beside the value.
        assert!(!grants.admits("budget", &value));
    }

    #[test]
    fn a_value_this_process_never_minted_is_refused() {
        let grants = Grants::new();
        grants.mint("notes").unwrap();
        assert!(!grants.admits("notes", ""));
        assert!(!grants.admits("notes", "0000"));
        assert!(!grants.admits("notes", &"a".repeat(64)));
    }

    #[test]
    fn a_value_past_its_lifetime_is_refused() {
        let grants = Grants::with_ttl(Duration::ZERO);
        let value = grants.mint("notes").unwrap();
        assert!(!grants.admits("notes", &value));
    }

    #[test]
    fn minting_prunes_what_the_lifetime_has_run_out() {
        let grants = Grants::with_ttl(Duration::ZERO);
        grants.mint("notes").unwrap();
        grants.mint("notes").unwrap();
        assert_eq!(grants.len(), 1, "an expired value must not accumulate");
    }

    #[test]
    fn two_pages_of_one_app_can_both_send() {
        // A second tab must not take the first one's capability away: the map holds values, not
        // one value per app.
        let grants = Grants::new();
        let first = grants.mint("notes").unwrap();
        let second = grants.mint("notes").unwrap();
        assert_ne!(first, second);
        assert!(grants.admits("notes", &first));
        assert!(grants.admits("notes", &second));
    }

    #[test]
    fn the_cookie_names_one_route_and_the_attributes_that_confine_it() {
        let cookie = cookie("notes", "abc123");
        assert!(cookie.starts_with("deepnest-chat=abc123"), "{cookie}");
        // The path is the app's chat route and nothing wider — no `/api/`, no `/`.
        assert!(cookie.contains("Path=/api/apps/notes/chat"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        // A session cookie: closing the browser is enough to lose it.
        assert!(!cookie.contains("Max-Age"), "{cookie}");
        assert!(!cookie.contains("Expires"), "{cookie}");
        // And it is not the control token under another name.
        assert!(!cookie.contains("Bearer"), "{cookie}");
    }

    #[test]
    fn the_value_is_found_among_the_other_cookies_a_page_carries() {
        let request = request_with("prefs=dark; deepnest-chat=abc123; theme=light");
        assert_eq!(presented(&request), Some("abc123"));
    }

    #[test]
    fn a_request_with_no_cookie_of_that_name_presents_nothing() {
        assert_eq!(presented(&request_with("prefs=dark; theme=light")), None);
        let bare =
            parse("POST /api/apps/notes/chat HTTP/1.1\r\nHost: 127.0.0.1:4270\r\n\r\n").unwrap();
        assert_eq!(presented(&bare), None);
    }

    #[test]
    fn a_name_that_merely_contains_the_cookies_name_is_not_it() {
        // `deepnest-chat-admin=…` is a different cookie, and a prefix match would accept it.
        let request = request_with("deepnest-chat-admin=abc123");
        assert_eq!(presented(&request), None);
    }
}
