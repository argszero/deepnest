//! Which files in a tree are Mach-O binaries, and therefore which ones a release has to sign.
//!
//! The design of record says every Mach-O inside the payload is signed **before** it is packed,
//! because first-run unpacking does not re-sign and an unsigned child process is refused under
//! hardened runtime. That leaves one question the pipeline cannot answer by itself — *which files
//! are they* — and it is worth being a module of its own rather than a `find | xargs codesign` in a
//! workflow: a missed binary is not a build error, it is a payload that installs, opens, and then
//! refuses to start a compiler on the user's machine, where nobody can read this repository to find
//! out why.
//!
//! Four decisions, in the order they cost something:
//!
//! - **Four bytes decide it, so a 140 MB binary costs eight.** A Mach-O begins with a magic number,
//!   and that is what the loader reads too. Nothing else in the file is inspected: this module never
//!   parses a load command, never looks for `LC_CODE_SIGNATURE`, and never needs to be taught a new
//!   file format — because what it answers is "is this a binary the linker wrote", and the linker
//!   always wrote the magic first.
//! - **One magic means two things, and the count tells them apart.** `0xcafebabe` is both
//!   `FAT_MAGIC` and the Java class-file magic, and the payload may well hold a `.class` file — a
//!   toolchain archive is not only binaries. The four bytes after the magic are a fat binary's
//!   architecture count and a class file's minor-then-major version, and those ranges do not
//!   overlap: Java 1.1 is major 45 and today's is 65, while no binary anybody built carries 45
//!   architectures. So a count of 1 to 20 is a fat binary and anything above it is a class file.
//!   Getting this wrong in the safe direction — signing a class file — costs one `codesign` and no
//!   correctness; getting it wrong the other way ships an unsigned fat binary.
//! - **A symlink is skipped, not followed.** Signing follows a link to its target, so following one
//!   inside the tree signs the same bytes twice under two names, and following one that leaves the
//!   tree signs a file that is not part of the payload at all. The target inside the tree is found
//!   on its own, by the path that really holds it, which is the name the archive will carry.
//! - **The order is sorted, and an unreadable entry is refused rather than passed over.** Sorted,
//!   because a signing log that reorders itself between two identical builds cannot be diffed, and
//!   this crate's packer already made the same choice for the same reason. Refused, because the
//!   silent alternative — skip what cannot be read — is the failure this module exists to prevent,
//!   wearing a green build.
//!
//! What this module deliberately does **not** do: sign anything, or know that `codesign` exists. It
//! compiles and its tests pass on the two platforms that ship no `.app`s and no `codesign` at all,
//! and that is the point: the enumeration is the half of "sign every Mach-O" that fails quietly, so
//! it is the half that has to be checkable everywhere rather than on one CI leg.
//!
//! Nothing here looks for payload markers or a toolchain directory either — [`machos_in`] is given a
//! directory and walks it. The caller owns the question of *which* directory, and the release
//! pipeline answers it differently for a toolchain and for a bundle.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// How many bytes of a file decide whether it is a Mach-O.
///
/// Eight rather than four: the thin magics are the first four, and a fat magic carries the
/// architecture count in the four after them — which is the only thing that tells a fat binary from
/// a Java class file.
pub const HEADER_BYTES: usize = 8;

/// A 32-bit binary in the byte order the file was written in.
const THIN_32: [u8; 4] = [0xfe, 0xed, 0xfa, 0xce];
/// The same, written byte-swapped: a big-endian file on a little-endian machine, or the reverse.
const THIN_32_SWAPPED: [u8; 4] = [0xce, 0xfa, 0xed, 0xfe];
/// A 64-bit binary — what every arm64 mac produces.
const THIN_64: [u8; 4] = [0xfe, 0xed, 0xfa, 0xcf];
/// The same, byte-swapped.
const THIN_64_SWAPPED: [u8; 4] = [0xcf, 0xfa, 0xed, 0xfe];
/// Several architectures in one file, count in the next four bytes, big-endian.
const FAT: [u8; 4] = [0xca, 0xfe, 0xba, 0xbe];
/// The same, little-endian.
const FAT_SWAPPED: [u8; 4] = [0xbe, 0xba, 0xfe, 0xca];
/// The 64-bit fat header, where each architecture entry is wider.
const FAT_64: [u8; 4] = [0xca, 0xfe, 0xba, 0xbf];
/// The same, little-endian.
const FAT_64_SWAPPED: [u8; 4] = [0xbf, 0xba, 0xfe, 0xca];

/// The most architectures this will believe a fat binary carries.
///
/// Above it the number is not an architecture count but something else that starts with the same
/// magic — in practice a Java class file's version, which is 45 at the oldest and 65 today. Twenty
/// is generous for a real one: the widest a shipping fat binary gets is three or four.
const MOST_ARCHITECTURES: u32 = 20;

/// Whether `header` — the first bytes of a file — begins a Mach-O binary.
///
/// A slice shorter than a magic is not one: a file too short to hold the magic was not written by a
/// linker, and reading it is not an error to report but a fact to return.
pub fn is_mach_o(header: &[u8]) -> bool {
    let magic = match header.get(..4) {
        Some(four) => [four[0], four[1], four[2], four[3]],
        None => return false,
    };
    match magic {
        THIN_32 | THIN_32_SWAPPED | THIN_64 | THIN_64_SWAPPED => true,
        // A fat header with no room for its count is not a fat binary, and the count decides
        // whether this magic is a binary at all.
        FAT | FAT_64 => is_an_architecture_count(architectures(header, u32::from_be_bytes)),
        FAT_SWAPPED | FAT_64_SWAPPED => {
            is_an_architecture_count(architectures(header, u32::from_le_bytes))
        }
        _ => false,
    }
}

/// The four bytes after a fat magic, read in the file's own byte order; zero when they are absent.
fn architectures(header: &[u8], order: fn([u8; 4]) -> u32) -> u32 {
    match header.get(4..8) {
        Some(four) => order([four[0], four[1], four[2], four[3]]),
        None => 0,
    }
}

/// Whether `count` is a number of architectures rather than a Java class file's version.
fn is_an_architecture_count(count: u32) -> bool {
    count > 0 && count <= MOST_ARCHITECTURES
}

/// Why a tree could not be scanned.
///
/// As in the rest of this crate, the variants differ in what each says about the input rather than
/// in what the caller does next: every one of them means no list, and therefore nothing signed.
#[derive(Debug)]
pub enum ScanError {
    /// The path to scan is not a directory that can be read.
    Root {
        /// The path that was named.
        path: PathBuf,
        /// What the filesystem said, or why it was refused.
        source: io::Error,
    },
    /// A directory inside the tree could not be listed.
    List {
        /// The directory.
        path: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
    /// An entry could not be examined, or a file's first bytes could not be read.
    Read {
        /// The entry or file.
        path: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
}

impl fmt::Display for ScanError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root { path, source } => write!(
                out,
                "cannot scan {} for binaries to sign: {source}",
                path.display()
            ),
            Self::List { path, source } => {
                write!(
                    out,
                    "cannot list what is inside {}: {source}",
                    path.display()
                )
            }
            Self::Read { path, source } => write!(
                out,
                "cannot read {} to decide whether it must be signed: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Root { source, .. } | Self::List { source, .. } | Self::Read { source, .. } => {
                Some(source)
            }
        }
    }
}

/// Every Mach-O inside `root`, deepest walk included, sorted by path.
///
/// Public because it is the list a signing step iterates: the release pipeline unpacks or builds a
/// tree, calls this, and signs what comes back. An empty list is a real answer — a tree of scripts
/// and configuration holds no binaries — and is not an error.
pub fn machos_in(root: &Path) -> Result<Vec<PathBuf>, ScanError> {
    let metadata = fs::metadata(root).map_err(|source| ScanError::Root {
        path: root.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(ScanError::Root {
            path: root.to_path_buf(),
            source: io::Error::new(io::ErrorKind::NotADirectory, "not a directory"),
        });
    }

    let mut found = Vec::new();
    walk(root, &mut found)?;
    // Sorted here rather than maintained while walking: the walk is a depth-first descent by name,
    // which is already deterministic, and this is what makes the order the one a reader expects
    // when two paths of different depths sit beside each other in the log.
    found.sort();
    Ok(found)
}

/// Adds every Mach-O under `directory` to `found`.
fn walk(directory: &Path, found: &mut Vec<PathBuf>) -> Result<(), ScanError> {
    let listing = fs::read_dir(directory).map_err(|source| ScanError::List {
        path: directory.to_path_buf(),
        source,
    })?;

    // The names are collected before anything is examined, so the walk does not depend on what
    // order the filesystem hands entries out in — the same tree then produces the same list on
    // every machine, which is what lets a diff of two signing logs mean something.
    let mut names: Vec<OsString> = Vec::new();
    for entry in listing {
        let entry = entry.map_err(|source| ScanError::List {
            path: directory.to_path_buf(),
            source,
        })?;
        names.push(entry.file_name());
    }
    names.sort();

    for name in names {
        let path = directory.join(&name);
        // `symlink_metadata`, so a link is seen as the link it is. The payload may hold them — a
        // toolchain is full of them — and following one would sign a file the tree does not own.
        let metadata = fs::symlink_metadata(&path).map_err(|source| ScanError::Read {
            path: path.clone(),
            source,
        })?;
        if metadata.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            walk(&path, found)?;
        } else if metadata.is_file() && begins_with_a_mach_o_magic(&path)? {
            found.push(path);
        }
        // Anything else — a fifo, a socket, a device node — is not a file a linker wrote, and a walk
        // that tried to read one would block on the first pipe it met.
    }
    Ok(())
}

/// Whether the file at `path` begins with a Mach-O magic, reading no more than [`HEADER_BYTES`].
fn begins_with_a_mach_o_magic(path: &Path) -> Result<bool, ScanError> {
    let mut file = fs::File::open(path).map_err(|source| ScanError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut header = [0u8; HEADER_BYTES];
    let mut read = 0;
    while read < header.len() {
        match file.read(&mut header[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            // A read interrupted by a signal is a read to be repeated, not a file to be refused.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(ScanError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        }
    }
    Ok(is_mach_o(&header[..read]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What an arm64 mac writes: a 64-bit binary, little-endian, and then bytes nothing reads.
    const BINARY: &[u8] = b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01 and the rest is not read";

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-macho-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        /// A file with the bytes a linker would have written first.
        fn binary(&self, relative: &str) -> PathBuf {
            self.file(relative, BINARY)
        }

        fn text(&self, relative: &str) -> PathBuf {
            self.file(relative, b"#!/bin/sh\nexit 0\n")
        }

        fn file(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.base.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn the_two_thin_magics_and_their_swapped_spellings_are_all_a_mach_o() {
        for magic in [
            b"\xfe\xed\xfa\xce", // 32-bit, as written
            b"\xce\xfa\xed\xfe", // 32-bit, swapped
            b"\xfe\xed\xfa\xcf", // 64-bit, as written
            b"\xcf\xfa\xed\xfe", // 64-bit, swapped — what arm64 produces
        ] {
            assert!(is_mach_o(magic), "{magic:?} was not read as a Mach-O");
        }
    }

    #[test]
    fn something_that_is_not_a_binary_is_not_one() {
        // A file too short to hold a magic, an ELF, a PE, and a script: none of them is signed.
        assert!(!is_mach_o(b""));
        assert!(!is_mach_o(b"\xcf\xfa\xed"));
        assert!(!is_mach_o(b"\x7fELF\x02\x01\x01\x00"));
        assert!(!is_mach_o(b"MZ\x90\x00\x03\x00\x00\x00"));
        assert!(!is_mach_o(b"#!/bin/sh"));
    }

    #[test]
    fn a_java_class_file_is_not_a_fat_binary() {
        // `0xcafebabe` is the class-file magic too: minor 0, then the major version in the next two
        // bytes. Java 21 is 65, and 65 architectures is not a binary anybody built.
        assert!(!is_mach_o(b"\xca\xfe\xba\xbe\x00\x00\x00\x41"));
        // Java 1.1, major 45, is still far more architectures than exist.
        assert!(!is_mach_o(b"\xca\xfe\xba\xbe\x00\x00\x00\x2d"));

        // The same magic with a count that can only be a count is a fat binary, both spellings.
        assert!(is_mach_o(b"\xca\xfe\xba\xbe\x00\x00\x00\x02"));
        assert!(is_mach_o(b"\xca\xfe\xba\xbe\x00\x00\x00\x14"));
        assert!(is_mach_o(b"\xbf\xba\xfe\xca\x02\x00\x00\x00"));
        // A 64-bit fat header carries the same count in the same place.
        assert!(is_mach_o(b"\xca\xfe\xba\xbf\x00\x00\x00\x03"));

        // And a fat magic with no room for its count decides nothing, so it is not a binary.
        assert!(!is_mach_o(b"\xca\xfe\xba\xbe"));
        // A count of zero architectures is a header that describes nothing.
        assert!(!is_mach_o(b"\xca\xfe\xba\xbe\x00\x00\x00\x00"));
    }

    #[test]
    fn the_bundle_this_crate_writes_holds_exactly_one_binary_and_it_is_the_executable() {
        let scratch = Scratch::new("bundle");
        let source = scratch.binary("build/deepnest");
        let bundle = crate::write(&scratch.base.join("dist"), &source, "0.1.0").unwrap();

        // Scanned from `Contents`, which is the tree a release signs: the plist is not a binary and
        // the executable is the only one there is.
        let found = machos_in(&bundle.app.join("Contents")).unwrap();
        assert_eq!(found, vec![bundle.executable.clone()]);
    }

    #[test]
    fn every_binary_in_a_payload_is_found_and_a_script_beside_it_is_not() {
        let scratch = Scratch::new("payload");
        scratch.binary("toolchain/bin/rustc");
        scratch.binary("toolchain/lib/librustc_driver.dylib");
        // A nested one, at the depth a real toolchain puts the linker's frontend.
        scratch.binary("toolchain/lib/rustlib/aarch64-apple-darwin/bin/rust-lld");
        scratch.text("toolchain/bin/rustfmt");
        scratch.text("toolchain/share/doc/README.md");
        // A file that only looks like one from its name.
        scratch.file("toolchain/bin/README", b"not a binary either\n");

        let found = machos_in(&scratch.base.join("toolchain")).unwrap();
        let named: Vec<String> = found
            .iter()
            .map(|path| {
                path.strip_prefix(scratch.base.join("toolchain"))
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            named,
            [
                "bin/rustc",
                "lib/librustc_driver.dylib",
                "lib/rustlib/aarch64-apple-darwin/bin/rust-lld",
            ],
            "the list a signing step would iterate"
        );

        // The order is the same on every run, which is what makes two signing logs comparable.
        assert_eq!(machos_in(&scratch.base.join("toolchain")).unwrap(), found);
    }

    #[test]
    fn a_tree_of_scripts_and_noise_holds_no_binary_and_that_is_not_an_error() {
        let scratch = Scratch::new("nothing-to-sign");
        scratch.text("bin/one");
        scratch.file("empty", b"");
        assert!(machos_in(&scratch.base).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_a_binary_of_its_own() {
        let scratch = Scratch::new("symlink");
        let target = scratch.binary("toolchain/bin/clang");
        std::os::unix::fs::symlink(&target, scratch.base.join("toolchain/bin/cc")).unwrap();
        // A link that leaves the tree entirely — the payload does not own what it points at.
        let outside = scratch.binary("elsewhere/prebuilt");
        std::os::unix::fs::symlink(&outside, scratch.base.join("toolchain/bin/away")).unwrap();

        let found = machos_in(&scratch.base.join("toolchain")).unwrap();
        assert_eq!(found, vec![target]);
    }

    #[test]
    fn a_path_that_is_not_a_directory_is_refused() {
        let scratch = Scratch::new("not-a-directory");
        let binary = scratch.binary("build/deepnest");
        let refused = machos_in(&binary).unwrap_err();
        assert!(matches!(refused, ScanError::Root { .. }), "{refused}");

        let missing = scratch.base.join("nowhere");
        let refused = machos_in(&missing).unwrap_err();
        assert!(matches!(refused, ScanError::Root { .. }), "{refused}");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_read_is_refused_rather_than_passed_over() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new("unreadable");
        scratch.binary("toolchain/bin/rustc");
        let walled = scratch.binary("toolchain/bin/hidden");
        fs::set_permissions(&walled, fs::Permissions::from_mode(0o000)).unwrap();

        // Nothing is unreadable to root, so this machine has nothing to assert. The behaviour under
        // test is the ordinary user's, which is what CI runs as.
        if fs::read(&walled).is_ok() {
            fs::set_permissions(&walled, fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }

        let refused = machos_in(&scratch.base.join("toolchain")).unwrap_err();
        fs::set_permissions(&walled, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(refused, ScanError::Read { .. }), "{refused}");

        // Refused means no list: a caller that signs what a failed scan left would sign a partial
        // payload and call it done.
        assert!(refused.to_string().contains("hidden"), "{refused}");
    }
}
