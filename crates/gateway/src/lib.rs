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
//! What this does **not** do yet, and what each one is:
//!
//! - **No WebSocket upgrade.** An upgrade is not a forward: the connection stops being two
//!   request/response pairs and becomes one tunnel, and the hand-written layer here reads one
//!   request per connection. It is the next slice, and it is the one that changes this crate's
//!   connection model rather than adding to it.
//! - **No streaming.** The body is read whole before it is answered, so an SSE endpoint answers
//!   only when it ends. Unbuffered streaming is the same slice as the upgrade.
//! - **No floating dialog, and so no CSP rewrite.** The dialog is injected into every
//!   `/app/<slug>/` response so that an agent rewriting the app's page wholesale cannot delete it;
//!   until there is a dialog to inject, rewriting the app's CSP would be loosening a policy for
//!   nothing.

pub mod http;
pub mod proxy;
pub mod rewrite;

pub use proxy::{exchange, forward, ForwardError, Forwarder, StdForwarder, Target};
