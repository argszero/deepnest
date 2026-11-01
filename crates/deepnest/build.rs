//! Writes the toolchain payload beside this crate's objects, so `payload.rs` can embed it.
//!
//! `include_bytes!` is resolved at compile time and fails on a path that does not exist, so a
//! development build — and every CI leg — would fail on a repository that carries no archive. This
//! script always writes the file: the archive when the build was given one, zero bytes when it was
//! not. An empty file is the reading `payload::embedded` turns into "this binary carries no
//! payload", which is a build that resolves its tools from `PATH`.
//!
//! `DEEPNEST_PAYLOAD` names the archive. It is the release pipeline's input, and it fails the build
//! when it is set to something unreadable: a packager that meant to carry a toolchain and silently
//! shipped a binary without one would find out from a user's failed build instead of from here.
//!
//! A relative name is resolved against the workspace root — the directory `payload/` lives in, and
//! the one the default below points into — and not against this package. A build script's working
//! directory is the package's own, so reading `payload/toolchain.zst` literally would look under
//! `crates/deepnest/`: that is not a hypothetical, it is how the release pipeline's first run failed,
//! with a build that could not read a file sitting at the repository root.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(
        env::var_os("OUT_DIR").expect("cargo sets OUT_DIR for a package with a build script"),
    );
    let dest = out.join("toolchain.payload");
    let workspace = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .expect("cargo sets CARGO_MANIFEST_DIR for a build script"),
    )
    .join("..")
    .join("..");

    println!("cargo:rerun-if-env-changed=DEEPNEST_PAYLOAD");
    let named = env::var_os("DEEPNEST_PAYLOAD").map(PathBuf::from);
    let source = named
        .clone()
        .map(|named| workspace.join(named))
        .unwrap_or_else(|| workspace.join("payload/toolchain.zst"));
    // The archive is a file the build reads, so a change to it has to rebuild this crate; naming a
    // path that is not there yet is what makes its first appearance trigger a build too.
    println!("cargo:rerun-if-changed={}", source.display());

    match fs::read(&source) {
        Ok(bytes) => fs::write(&dest, &bytes).expect("cannot write the payload beside the objects"),
        Err(error) => {
            if named.is_some() {
                panic!(
                    "DEEPNEST_PAYLOAD names {}, which cannot be read: {error}",
                    source.display()
                );
            }
            fs::write(&dest, b"").expect("cannot write an empty payload beside the objects");
        }
    }
}
