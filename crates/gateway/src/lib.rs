//! The single door: everything a browser reaches, and the HTTP layer under it.
//!
//! deepnest listens on one port (`host 20:45`) and every app lives behind a path prefix on that
//! port. This crate owns both halves of that decision — the HTTP itself, and the forward that
//! serves an app under `/app/<slug>/`.
//!
//! **The HTTP layer lives here rather than in the binary** (`decided in implementation`,
//! 2026-10-31). The design left "which server stack" to this crate, and a forward decides more of
//! it than a route table does: a proxied response carries whatever content type and headers the
//! app chose, so the response type has to hold them, and a type that can hold them is the gateway's
//! business. The binary re-exports it, so a name that used to be `crate::http` still is.
//!
//! [`proxy`] is the forward. Its whole job is the requirement §2 states in one line — **do not lie
//! to the app**: an app that was written to be served from `/` must run unchanged under
//! `/app/<slug>/`, which means the paths it sees, the URLs it sends back and the page it returns
//! all have to be adjusted, and adjusted in a way that leaves an app that already does the right
//! thing alone.
//!
//! **An answer is not always a body** (`decided in implementation`, 2026-10-31). A WebSocket
//! upgrade and an SSE stream both begin with a head and continue for as long as the user is there,
//! so the forward returns one of two things: a [`Response`], read whole and adapted to the prefix,
//! or an [`Answer::Stream`] — the app's head plus its live connection, passed on as bytes arrive.
//! That is the whole difference between a proxy that carries pages and one that carries a
//! conversation, and it is why the listening loop in the binary is no longer one connection at a
//! time.
//!
//! **The floating dialog is injected here, and the app's CSP moves with it** (`decided in
//! implementation`, 2026-10-31). [`dialog`] adds the dialog to every `text/html` answer and widens
//! the policy that would otherwise refuse its script — because a ball that a browser refuses to
//! draw is the same as no ball, except that the page looks fine.
//!
//! What this does **not** do yet, and what it is:
//!
//! **The escalation prompt is in the dialog, and this crate is only the carrier.** The jump table
//! is `crates/sandbox`'s, the question is `crates/agent`'s, and the wait is the binary's; what the
//! gateway does is inject the page the prompt appears on and widen the app's policy far enough for
//! its script to load. So the prompt inherits the same rule as the ball: an app that rewrites its
//! own page cannot remove it, and an app whose policy says `script-src 'none'` still gets it.

pub mod dialog;
pub mod http;
pub mod proxy;
pub mod rewrite;
pub mod transport;

pub use proxy::{
    exchange, forward, is_upgrade, Answer, ForwardError, Forwarder, StdForwarder, Stream,
    StreamKind, Target,
};
pub use transport::PlainHttp;
