//! The deepnest binary: the smallest thing that turns the library crates beside it into a program.
//!
//! Nothing that shipped before this crate could be started. This one **starts**: it reads the
//! one file the user must supply, writes the token that guards the panel, takes the single
//! listening port as its own, and answers on it. What sits above that line is a crate beside this
//! one: the HTTP layer and the forward are [`deepnest_gateway`], and the panel itself is
//! `deepnest-web`. Every route it does not serve it names, rather than answering a `404` that looks
//! like a bug in a path the caller spelled correctly.
//!
//! Three of its decisions are load-bearing, and each is stated where it is made:
//!
//! - **Single-instance admission is the bind itself** ([`server::bind`]). The kernel refuses a
//!   second listener on `127.0.0.1:4270`, so there is no PID file to forge and no window
//!   between "am I alone?" and "serve".
//! - **A missing config is first run, not a fault; an unparseable one is a fault**
//!   ([`config::load`]). Starting with a base URL the binary only half understood moves the
//!   diagnosis to the first request, which is far from the cause.
//! - **The token is `0600` and re-asserted on a keepalive** ([`token::reassert`]). Loopback is
//!   not a security boundary: any page the browser loads can reach the port, so what stops such
//!   a page acting through it is a secret it cannot read — and what keeps that secret working
//!   is a file that heals after an external deletion instead of locking the panel out until the
//!   next restart.
//!
//! What this crate deliberately is **not**: an HTTP server. It routes, and the layer underneath —
//! the parser, the response type, and the forward that serves an app under `/app/<slug>/` — is
//! [`deepnest_gateway`], re-exported here as [`http`]. The split is not decoration: a forwarded
//! answer carries the app's own content type and headers, so the response type has to hold them,
//! and a type that can hold an arbitrary header belongs with the code that forwards them.

pub mod chat;
pub mod config;
pub mod escalation;
pub mod grant;
pub mod home;
pub mod model;
pub mod open;
pub mod rebuild;
pub mod router;
pub mod server;
pub mod token;
pub mod updates;

/// The HTTP layer itself, which lives in the gateway: choosing it is that crate's decision, and a
/// response the gateway forwards carries headers this binary never writes.
pub use deepnest_gateway::http;

pub use config::{Config, ConfigError, Llm};
pub use home::{home, prepare_home, HOME_ENV};
pub use router::Router;
pub use server::{start, BindAttempts, BindError, Server, ServerOptions, DEFAULT_PORT, LOOPBACK};
pub use token::Token;
