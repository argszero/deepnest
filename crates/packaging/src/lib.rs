//! What deepnest carries so it can run on a machine with nothing installed.
//!
//! deepnest is one binary, and part of that binary is a toolchain — a Rust compiler and a
//! JavaScript runtime — which cannot be used from inside an executable and therefore has to reach
//! the disk before it can be run. It unpacks on first use into `~/.deepnest/toolchain/`, and this
//! crate owns what ends up there: where an unpacked toolchain lives ([`toolchain::dir`]), what
//! makes one whole ([`Status::Ready`]), and what an unpack that was killed halfway leaves behind.
//!
//! **What lands first is the contract, not the unpack.** Decompressing the payload needs the
//! payload, which this build does not carry yet, and the steps that do need it — the zstd unpack
//! itself and macOS signing — can each be written against the shape below without changing a byte
//! of it. The order is the same one `crates/apps` took: the directory contract first, because it is
//! the part users' disks hold once it ships, and the thing that reads it is written against it
//! rather than the other way round.
//!
//! [`cc`] is the other half of what a machine needs: the C toolchain deepnest deliberately does not
//! carry, checked **before the first build** rather than discovered by a compiler halfway through
//! it. That check is a promise the README already makes, and it needs no payload at all — which is
//! why it landed before the unpack.
//!
//! The invariant the whole crate exists for is one sentence from the design: **a half-unpacked
//! toolchain must never be mistaken for a complete one.** A mistaken toolchain is worse than a
//! missing one — a missing one sends the user to the first-run page, while a half one sends them a
//! compiler that fails on a missing file nobody can explain — so every reading here fails closed,
//! and every ambiguous directory is reported rather than used.

pub mod cc;
pub mod toolchain;

pub use cc::{check, requirement, Finding, Kind, Probe, Report, Requirement, StdProbe};
pub use toolchain::{staging, status, sweep, Staging, Status, DIR_NAME, MARKER, PAYLOAD, VERSION};
