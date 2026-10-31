//! The deepnest binary: the smallest thing that turns five library crates into a program.
//!
//! Nothing that shipped before this crate could be started. This one **starts**: it reads the
//! one file the user must supply, writes the token that guards the panel, takes the single
//! listening port as its own, and answers on it. Everything above that line — the apps, the
//! gateway, the panel itself — arrives in later crates; nothing here pretends to be them, and
//! every route it does not serve says so by name.
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
//! What this crate deliberately is **not**: an HTTP server. [`http`] speaks exactly the one
//! request it needs to, on one connection, and closes. A real server is `crates/gateway`'s job,
//! and it comes with routing, header rewriting and WebSocket upgrade — a different problem than
//! proving the process starts and proves it on three platforms.

pub mod config;
pub mod home;
pub mod http;
pub mod router;
pub mod server;
pub mod token;

pub use config::{Config, ConfigError, Llm};
pub use home::{home, prepare_home, HOME_ENV};
pub use router::Router;
pub use server::{start, BindAttempts, BindError, Server, ServerOptions, DEFAULT_PORT, LOOPBACK};
pub use token::Token;
