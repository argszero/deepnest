//! The credential an app's own process holds, and the narrow routes it opens.
//!
//! An app is a program deepnest starts, and until this module existed it was a program with **no
//! credential at all**: [`crate::token`]'s file is `0600` and the app runs as the same user, so
//! there was nothing stopping it from reading the control token and spending it on every route —
//! starting apps, rewriting the config, stopping another app. Nothing did that; nothing had to.
//!
//! The agents an app runs change that. An app's code now asks this server to run a turn, and the
//! answer to "who is asking" has to be a value the app can hold and this server can refuse when it
//! is wrong. So a start mints a ticket and hands it over in the environment
//! ([`crate::apps::Supervisor::with_env`]), and the ticket opens exactly the routes an app's own
//! code needs: its agent turns, its tools, and nothing else.
//!
//! # What this does and does not protect against
//!
//! **It is not a boundary against a hostile app, and saying otherwise would be a lie.** The app
//! runs as the same operating-system user, so it can read `deepnest.token` whatever this module
//! does; a ticket narrows what an app is *given* and what a route accepts, not what a determined
//! same-user process can steal. What it buys is the difference between an app that has the
//! control token by construction and one that has to go looking for it, and that is the difference
//! between an accident and an attack.
//!
//! **It is per app and per start.** A ticket names one app, so `todo`'s ticket is refused at
//! `notes/…`; and it dies with the process it was minted for, because a restart mints a new one and
//! forgets the old. A ticket lifted from one app's environment opens nothing belonging to another.
//!
//! **It is not a cookie.** Cookies are what a page holds (`crate::grant`); this is what a program
//! holds, and it travels in `Authorization` for that reason — an app's own code is not a browser
//! and has no business being handed a `Set-Cookie` it cannot read.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The environment variable a started app reads its ticket from.
///
/// Named for what it opens rather than for what it is, because the first thing an app's author
/// asks is "which value do I send", and the answer should be readable in their own `main`.
pub const ENV: &str = "DEEPNEST_APP_TICKET";

/// The environment variable a started app reads deepnest's own address from.
///
/// A ticket says *who* the app is; this says *where* to send it. Without it an app would have to
/// hard-code `127.0.0.1:4270`, which is wrong the moment someone runs deepnest with
/// `$DEEPNEST_PORT` set — and it is not a secret, so it travels beside the ticket rather than
/// inside it. The value has no trailing slash: an app joins a path onto it.
pub const URL_ENV: &str = "DEEPNEST_URL";

/// The environment variable a started app reads its own slug from.
///
/// Every route an app's ticket opens carries the slug in the path — `/api/apps/<slug>/agent` — so an
/// app that does not know its own name cannot call any of them. The app directory is not a reliable
/// substitute: the binary is started from `target/release/`, so the name is three parents up and
/// one rename away from wrong.
pub const SLUG_ENV: &str = "DEEPNEST_APP_SLUG";

/// What a start tells an app, in its environment and nowhere else.
///
/// Two entries and two different kinds of fact, which is why this is one function rather than two
/// closures: a start with no ticket still knows where deepnest is, and an app that can reach
/// deepnest without a credential should still be told. An empty `base` omits the variable instead
/// of writing an empty one — an app reading `""` and an app reading nothing should take the same
/// branch, and neither should build a URL out of nothing.
pub fn environment(ticket: Option<String>, base: &str, slug: &str) -> Vec<(String, String)> {
    let mut env = Vec::with_capacity(3);
    // No ticket rather than a weak one: an app started without a credential answers `401` and says
    // why, which is a reportable state. A predictable value would be a credential that only looks
    // like one.
    if let Some(value) = ticket {
        env.push((ENV.to_string(), value));
    }
    if !base.trim().is_empty() {
        env.push((URL_ENV.to_string(), base.trim_end_matches('/').to_string()));
    }
    if !slug.trim().is_empty() {
        env.push((SLUG_ENV.to_string(), slug.trim().to_string()));
    }
    env
}

/// How much entropy a ticket carries before hex encoding. The same as the control token: guessing
/// one has to be hopeless, not merely unlikely.
const BYTES: usize = 32;

/// The routes an app's own process may reach with its ticket, as the router spells them.
///
/// One entry per verb an app needs to run its own agent, and no entry for anything that manages
/// the machine. `agent` is the turn, `tools` is where the tool handoff goes back to, and
/// `agent-result` continues a paused turn — the three together are the whole of the loop in
/// `docs/design.md`, and nothing else is here on purpose.
pub const ROUTES: [&str; 3] = ["agent", "agent-result", "tools"];

/// Whether `action` is a route an app's ticket opens.
pub fn opens(action: &str) -> bool {
    ROUTES.contains(&action)
}

/// The tickets this process has minted and still honours.
///
/// `Clone` shares the map rather than copying it: a router is cloned per connection, and a ticket
/// minted when an app was started has to be the one the next request is checked against.
#[derive(Debug, Clone, Default)]
pub struct Tickets {
    live: Arc<Mutex<HashMap<String, Holder>>>,
}

/// One minted ticket: which app it belongs to.
#[derive(Debug, Clone)]
struct Holder {
    slug: String,
}

impl Tickets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a ticket for `slug`, or `None` when this machine's random source failed.
    ///
    /// A previous ticket for the same app is dropped: a ticket belongs to one run of one process,
    /// and a restart that left the old one usable would be a credential outliving the thing it was
    /// issued to.
    pub fn mint(&self, slug: &str) -> Option<String> {
        let mut raw = [0u8; BYTES];
        getrandom::getrandom(&mut raw).ok()?;
        let mut value = String::with_capacity(BYTES * 2);
        for byte in raw {
            let _ = std::fmt::Write::write_fmt(&mut value, format_args!("{byte:02x}"));
        }

        let mut live = self.live.lock().ok()?;
        live.retain(|_, holder| holder.slug != slug);
        live.insert(
            value.clone(),
            Holder {
                slug: slug.to_string(),
            },
        );
        Some(value)
    }

    /// Does `presented` belong to `slug`?
    ///
    /// Looked up *by* the presented value rather than compared against a stored one byte by byte:
    /// the map hashes the whole string first, so there is no prefix whose comparison a caller could
    /// time.
    pub fn admits(&self, slug: &str, presented: &str) -> bool {
        if presented.is_empty() {
            return false;
        }
        let Ok(live) = self.live.lock() else {
            return false;
        };
        live.get(presented)
            .is_some_and(|holder| holder.slug == slug)
    }

    /// Forget every ticket for `slug`, as a stop does.
    pub fn revoke(&self, slug: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.retain(|_, holder| holder.slug != slug);
        }
    }

    /// How many are held. Read by the tests that say a restart mints one and forgets the old.
    pub fn len(&self) -> usize {
        self.live.lock().map(|live| live.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_tells_the_app_where_deepnest_is_even_with_no_ticket() {
        // The address is not a credential: an app whose start could not mint a ticket is still an
        // app that has to reach this process to say so.
        let with = environment(Some("abc".into()), "http://127.0.0.1:4270/", "notes");
        assert_eq!(
            with,
            vec![
                (ENV.to_string(), "abc".to_string()),
                (URL_ENV.to_string(), "http://127.0.0.1:4270".to_string()),
                (SLUG_ENV.to_string(), "notes".to_string()),
            ]
        );
        let without = environment(None, "http://127.0.0.1:5000", "notes");
        assert_eq!(
            without,
            vec![
                (URL_ENV.to_string(), "http://127.0.0.1:5000".to_string()),
                (SLUG_ENV.to_string(), "notes".to_string()),
            ]
        );
        // Nothing to say about where we are says nothing, rather than an empty value an app would
        // join a path onto.
        assert!(environment(None, "  ", "").is_empty());
    }

    #[test]
    fn a_ticket_opens_its_own_app_and_no_other() {
        let tickets = Tickets::new();
        let value = tickets.mint("notes").unwrap();
        assert!(tickets.admits("notes", &value));
        assert!(!tickets.admits("todo", &value));
        assert!(!tickets.admits("notes", "not-a-ticket"));
        assert!(!tickets.admits("notes", ""));
    }

    #[test]
    fn restarting_forgets_the_ticket_the_previous_run_held() {
        let tickets = Tickets::new();
        let first = tickets.mint("notes").unwrap();
        let second = tickets.mint("notes").unwrap();
        assert_ne!(first, second, "a restart must not reuse a value");
        assert!(!tickets.admits("notes", &first));
        assert!(tickets.admits("notes", &second));
        assert_eq!(tickets.len(), 1, "one app holds one ticket");
    }

    #[test]
    fn stopping_revokes_it() {
        let tickets = Tickets::new();
        let value = tickets.mint("notes").unwrap();
        tickets.revoke("notes");
        assert!(!tickets.admits("notes", &value));
        assert!(tickets.is_empty());
    }

    #[test]
    fn two_apps_hold_two_tickets_that_do_not_open_each_other() {
        let tickets = Tickets::new();
        let notes = tickets.mint("notes").unwrap();
        let todo = tickets.mint("todo").unwrap();
        assert!(tickets.admits("notes", &notes));
        assert!(tickets.admits("todo", &todo));
        assert!(!tickets.admits("notes", &todo));
        assert_eq!(tickets.len(), 2);
    }

    #[test]
    fn only_the_routes_an_app_needs_are_opened_by_a_ticket() {
        // The three of the loop, and the absence of everything that manages the machine: a start,
        // a stop, the config, another app's page.
        assert!(opens("agent"));
        assert!(opens("agent-result"));
        assert!(opens("tools"));
        for closed in [
            "start",
            "stop",
            "restart",
            "chat",
            "escalation",
            "build",
            "logs",
        ] {
            assert!(!opens(closed), "a ticket opened `{closed}`");
        }
    }
}
