//! Putting a payload on disk: a `zstd` archive in, a whole toolchain out.
//!
//! [`toolchain`] owns the contract — where an unpacked payload lives, what makes one whole, and
//! what an interrupted unpack leaves behind. This module is the one thing that writes inside that
//! contract: it fills a [`Staging`] and calls [`Staging::adopt`], and nothing else in the workspace
//! writes under `toolchain/`.
//!
//! Five decisions:
//!
//! - **The archive is a tar inside a zstd stream.** A toolchain is a tree — names, nested
//!   directories, file modes, links — and a bare zstd stream carries bytes in an order but not the
//!   names to hang them on. `tar` is the container rustup itself distributes a toolchain in, so the
//!   packer has a proven way to write one and this reader has a shape to match.
//! - **Every path is checked before anything is written for it, and the check reads components, not
//!   the string.** `entry.unpack_in` would do a check of its own, and this module deliberately does
//!   not depend on it: the two implementations are independent, and a path that leaves the tree is
//!   refused here with a reason a packer can act on rather than silently skipped. The archive is
//!   built by a different step of the release pipeline than the one that reads it, so the threat
//!   this defends against is a mistake in the packer — not an adversary, who would have to be
//!   inside the binary already.
//! - **A link may point anywhere inside the tree and nowhere outside.** A symlink is the way a
//!   payload writes through a name: an entry that names a symlink as its directory writes wherever
//!   that link points. So the target is resolved lexically against the entry's own parent, and a
//!   target that would climb above the toolchain root — or that is absolute — is refused. Where a
//!   link cannot be made at all — Windows, without a privilege a plain process does not have — the
//!   entry is refused by name rather than turned into a copy, because a copy is a different tree.
//! - **A payload makes files, directories and links, and nothing else.** A device node, a fifo or a
//!   socket in a toolchain payload is not a toolchain; refusing them keeps this module from needing
//!   a privilege it should not have.
//! - **The mode is taken from the archive.** The executable bit is the difference between a
//!   toolchain and 140 MB of text, and a packer's `0755` is the only record of it.
//!
//! What this module does **not** do: verify a signature, and decompress from anywhere but a reader
//! the caller hands it. Where the archive itself comes from — embedded in the binary, or beside it
//! — is the packer's decision, and it is not made yet.

use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use crate::toolchain::Staging;

/// What an unpack produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    /// The published directory — the one [`crate::toolchain::status`] will now call
    /// [`crate::toolchain::Status::Ready`].
    pub dir: PathBuf,
    /// How many entries were written. Worth printing: it is the only number a user sees while a
    /// first run spends a while writing a toolchain.
    pub entries: usize,
}

/// Why an entry was refused.
///
/// Separate readings rather than one message because they send a packer to different places: an
/// absolute path is a header built from the wrong kind of input, a link that leaves the tree is a
/// link written with the wrong base, and a device node is a packing tool asked for the wrong thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The entry's path is absolute, or carries a Windows drive or UNC prefix.
    AbsolutePath,
    /// The entry's path climbs out of the toolchain with `..`, or names nothing at all.
    LeavesTheTree,
    /// A link whose target is absolute, or climbs above the toolchain root.
    LinkEscapes,
    /// A device node, fifo, socket, or an entry type this reader does not know.
    NotAPayload,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let said = match self {
            Self::AbsolutePath => "its path is absolute",
            Self::LeavesTheTree => "its path leaves the toolchain directory",
            Self::LinkEscapes => "its link target leaves the toolchain directory",
            Self::NotAPayload => {
                "it is not a file, a directory or a link, and a toolchain holds nothing else"
            }
        };
        f.write_str(said)
    }
}

/// Why an unpack did not publish a toolchain.
///
/// Every variant means the same thing to the caller — `toolchain/` holds no new directory, because
/// the [`Staging`] removes what it wrote on the way out — and the difference is what it says about
/// the archive.
#[derive(Debug)]
pub enum UnpackError {
    /// The reader did not carry a zstd stream, or the tar inside it ended early.
    Archive(io::Error),
    /// An entry was refused before anything was written for it.
    Refused {
        /// The entry's path, as the archive spelled it.
        entry: String,
        /// Which of the four refusals it met.
        why: Refusal,
    },
    /// The tree could not be written.
    Write {
        /// The entry being written when it failed.
        entry: String,
        /// What the filesystem said.
        source: io::Error,
    },
    /// The payload is already unpacked, or cannot be published.
    ///
    /// [`Staging::adopt`] refuses to replace a directory holding this same payload, and a caller
    /// that has already read [`crate::toolchain::status`] will not get here.
    Adopt(io::Error),
}

impl fmt::Display for UnpackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Archive(error) => write!(f, "the toolchain payload could not be read: {error}"),
            Self::Refused { entry, why } => {
                write!(f, "the toolchain payload refused `{entry}`: {why}")
            }
            Self::Write { entry, source } => {
                write!(
                    f,
                    "the toolchain payload could not write `{entry}`: {source}"
                )
            }
            Self::Adopt(error) => {
                write!(f, "the toolchain payload could not be published: {error}")
            }
        }
    }
}

impl std::error::Error for UnpackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Archive(error) | Self::Adopt(error) => Some(error),
            Self::Write { source, .. } => Some(source),
            Self::Refused { .. } => None,
        }
    }
}

/// Unpack `archive` into `staging`, publish it as `payload`, and say where it landed.
///
/// The whole archive is checked and written before anything is published, and [`Staging::adopt`]
/// writes the marker before the rename — so a reader arriving at any moment sees either no version
/// directory or a complete one, never a published name with half a tree under it. A failure here
/// removes the staging directory on the way out, through [`Staging`]'s own `Drop`.
pub fn unpack<R: Read>(
    archive: R,
    staging: Staging,
    payload: &str,
) -> Result<Unpacked, UnpackError> {
    let root = staging.dir().to_path_buf();
    let entries = extract(archive, &root)?;
    let dir = staging.adopt(payload).map_err(UnpackError::Adopt)?;
    Ok(Unpacked { dir, entries })
}

/// Write every entry of the tar inside `archive` under `root`, and count them.
fn extract<R: Read>(archive: R, root: &Path) -> Result<usize, UnpackError> {
    let decoder = zstd::Decoder::new(archive).map_err(UnpackError::Archive)?;
    let mut tar = tar::Archive::new(decoder);
    let entries = tar.entries().map_err(UnpackError::Archive)?;

    let mut written = 0usize;
    for entry in entries {
        let mut entry = entry.map_err(UnpackError::Archive)?;
        let said = entry
            .path()
            .map_err(UnpackError::Archive)?
            .to_string_lossy()
            .into_owned();
        let relative = checked_relative(Path::new(&said)).map_err(|why| UnpackError::Refused {
            entry: said.clone(),
            why,
        })?;
        let destination = root.join(&relative);

        match entry.header().entry_type() {
            kind if kind.is_dir() => {
                fs::create_dir_all(&destination).map_err(|source| UnpackError::Write {
                    entry: said.clone(),
                    source,
                })?;
            }
            kind if kind.is_file() => {
                let mode = entry.header().mode().map_err(UnpackError::Archive)?;
                write_file(&mut entry, &destination, &said, mode)?;
            }
            kind if kind.is_symlink() || kind.is_hard_link() => {
                let target = entry
                    .link_name()
                    .map_err(UnpackError::Archive)?
                    .ok_or_else(|| UnpackError::Refused {
                        entry: said.clone(),
                        why: Refusal::LeavesTheTree,
                    })?
                    .into_owned();
                checked_link(&relative, &target).map_err(|why| UnpackError::Refused {
                    entry: said.clone(),
                    why,
                })?;
                write_link(&relative, &target, &destination, &said, kind.is_hard_link())?;
            }
            _ => {
                return Err(UnpackError::Refused {
                    entry: said,
                    why: Refusal::NotAPayload,
                })
            }
        }
        written += 1;
    }
    Ok(written)
}

/// Copy one regular entry to `destination`, with the mode the archive gave it.
fn write_file<R: Read>(
    entry: &mut R,
    destination: &Path,
    said: &str,
    mode: u32,
) -> Result<(), UnpackError> {
    let write = |source: io::Error| UnpackError::Write {
        entry: said.to_string(),
        source,
    };
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(write)?;
    }
    let mut file = fs::File::create(destination).map_err(write)?;
    io::copy(entry, &mut file).map_err(write)?;
    drop(file);
    grant(mode, destination).map_err(write)?;
    Ok(())
}

/// Give `path` the mode the archive asked for.
///
/// Only the permission bits travel: a `tar` header's mode field is documented to hold the file type
/// as well, and applying that to a real file would hand it a type its own creation call did not.
#[cfg(unix)]
fn grant(mode: u32, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
}

/// Windows has no permission bits to set; a payload built for it does not rely on any.
#[cfg(not(unix))]
fn grant(_mode: u32, _path: &Path) -> io::Result<()> {
    Ok(())
}

/// Make one link entry, after [`checked_link`] has found its target inside the tree.
fn write_link(
    relative: &Path,
    target: &Path,
    destination: &Path,
    said: &str,
    hard: bool,
) -> Result<(), UnpackError> {
    let write = |source: io::Error| UnpackError::Write {
        entry: said.to_string(),
        source,
    };
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(write)?;
    }
    if hard {
        // A relative hard link names its target with the same base as the entry, which is what
        // `tar` writes; the archive's own root is this one, so the path is taken as written.
        let source = destination
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target);
        return fs::hard_link(&source, destination).map_err(write);
    }
    let _ = relative;
    symlink(target, destination).map_err(write)
}

#[cfg(unix)]
fn symlink(target: &Path, destination: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, destination)
}

/// A symlink on Windows needs a privilege a plain process does not have, and a payload that asks
/// for one is refused rather than silently turned into a copy: a copy is a different tree, and the
/// toolchain contract says a directory is what its contents are.
#[cfg(not(unix))]
fn symlink(_target: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a toolchain payload may not carry symbolic links on this platform",
    ))
}

/// The archive's path, as a relative path that cannot leave the toolchain directory.
///
/// `..` is refused rather than resolved: a path that has to climb in order to arrive is a path
/// written to leave, and a packer that emits one has a bug worth reporting instead of a tree worth
/// unpacking. `.` is dropped wherever it appears, because a leading `./` is ordinary in `tar`.
fn checked_relative(path: &Path) -> Result<PathBuf, Refusal> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => {}
            Component::ParentDir => return Err(Refusal::LeavesTheTree),
            Component::RootDir | Component::Prefix(_) => return Err(Refusal::AbsolutePath),
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(Refusal::LeavesTheTree);
    }
    Ok(relative)
}

/// Whether a link written at `entry` may point at `target` and stay inside the tree.
///
/// The check is lexical and made per entry, which is enough because a link can only be created
/// through this function: a link that would escape never exists in the tree, so no later entry can
/// reach outside by writing through it.
fn checked_link(entry: &Path, target: &Path) -> Result<(), Refusal> {
    if target.is_absolute() {
        return Err(Refusal::LinkEscapes);
    }
    // How deep the link itself sits, which is how many `..` steps it has before it climbs out.
    let mut depth = entry
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return Err(Refusal::LinkEscapes);
                }
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => return Err(Refusal::LinkEscapes),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    use crate::toolchain::{status, Staging, Status, MARKER};

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-unpack-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn home(&self) -> PathBuf {
            self.base.join("home")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// One entry of a test archive.
    enum Part {
        File(&'static str, &'static [u8], u32),
        Dir(&'static str),
        Symlink(&'static str, &'static str),
        Fifo(&'static str),
    }

    /// A real `tar.zst` archive, built with the same crates this module reads with — so the test
    /// measures a stream a packer would actually produce rather than a hand-made approximation.
    fn archive(parts: &[Part]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for part in parts {
            match part {
                Part::File(name, body, mode) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(body.len() as u64);
                    header.set_mode(*mode);
                    header.set_cksum();
                    builder.append_data(&mut header, name, *body).unwrap();
                }
                Part::Dir(name) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    header.set_mode(0o755);
                    header.set_cksum();
                    builder.append_data(&mut header, name, io::empty()).unwrap();
                }
                Part::Symlink(name, target) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    header.set_mode(0o777);
                    header.set_link_name(target).unwrap();
                    header.set_cksum();
                    builder.append_data(&mut header, name, io::empty()).unwrap();
                }
                Part::Fifo(name) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Fifo);
                    header.set_size(0);
                    header.set_mode(0o644);
                    header.set_cksum();
                    builder.append_data(&mut header, name, io::empty()).unwrap();
                }
            }
        }
        compress(&builder.into_inner().unwrap())
    }

    /// One entry of a hand-forged tar.
    struct Raw<'a> {
        name: &'a str,
        typeflag: u8,
        link: &'a str,
        body: &'a [u8],
    }

    /// A `tar.zst` whose headers are written byte by byte.
    ///
    /// The `tar` crate refuses to *write* a path that climbs out of the tree or is absolute — a good
    /// property of the writer, and the reason a payload from a packer carrying that bug cannot be
    /// produced through it. This reader still has to refuse such an archive, because what it reads
    /// is not necessarily what this workspace's writer made, so the test forges the bytes the way a
    /// packer with the bug would.
    fn raw_archive(entries: &[Raw]) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in entries {
            let mut header = [0u8; 512];
            let name = entry.name.as_bytes();
            assert!(name.len() < 100, "a test name must fit one header");
            header[..name.len()].copy_from_slice(name);
            octal(&mut header[100..108], 0o644);
            octal(&mut header[108..116], 0);
            octal(&mut header[116..124], 0);
            octal(&mut header[124..136], entry.body.len() as u64);
            octal(&mut header[136..148], 0);
            header[148..156].fill(b' ');
            header[156] = entry.typeflag;
            let link = entry.link.as_bytes();
            header[157..157 + link.len()].copy_from_slice(link);
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
            // Six octal digits, a NUL and a space, in that order.
            header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            out.extend_from_slice(&header);
            out.extend_from_slice(entry.body);
            let pad = (512 - entry.body.len() % 512) % 512;
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        out.extend(std::iter::repeat_n(0u8, 1024));
        compress(&out)
    }

    /// One numeric header field: octal digits, then a NUL filling the rest.
    fn octal(field: &mut [u8], value: u64) {
        let digits = field.len() - 1;
        let text = format!("{value:0digits$o}", digits = digits);
        field[..digits].copy_from_slice(text.as_bytes());
        field[digits] = 0;
    }

    fn compress(tar: &[u8]) -> Vec<u8> {
        let mut encoder = zstd::Encoder::new(Vec::new(), 3).unwrap();
        encoder.write_all(tar).unwrap();
        encoder.finish().unwrap()
    }

    fn unpack_bytes(home: &Path, bytes: &[u8], payload: &str) -> Result<Unpacked, UnpackError> {
        let staging = crate::toolchain::staging(home, "0.1.0").unwrap();
        unpack(io::Cursor::new(bytes.to_vec()), staging, payload)
    }

    #[test]
    fn a_whole_payload_becomes_a_ready_toolchain() {
        let scratch = Scratch::new("whole");
        let home = scratch.home();
        let bytes = archive(&[
            Part::Dir("bin"),
            Part::File("bin/rustc", b"#!/bin/sh\n", 0o755),
            Part::File("lib/notes.txt", b"hello", 0o644),
        ]);

        let unpacked = unpack_bytes(&home, &bytes, "payload-1").unwrap();
        assert_eq!(unpacked.entries, 3);
        assert!(unpacked.dir.join("bin/rustc").is_file());
        assert_eq!(
            fs::read(unpacked.dir.join("lib/notes.txt")).unwrap(),
            b"hello"
        );
        // The marker is what makes the directory whole, so publishing without it would be a
        // directory that every reader in `toolchain` calls interrupted.
        assert_eq!(
            fs::read_to_string(unpacked.dir.join(MARKER))
                .unwrap()
                .trim(),
            "payload-1"
        );
        // And the published name is what `status` reads: the two halves of this crate agree.
        assert!(matches!(
            status(&home, "0.1.0", "payload-1").unwrap(),
            Status::Ready { dir } if dir == unpacked.dir
        ));
        // No staging directory survives a successful unpack.
        assert!(!crate::toolchain::staging_dir(&home, "0.1.0").exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_is_the_archives_not_the_umask() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new("mode");
        let home = scratch.home();
        let bytes = archive(&[
            Part::File("bin/rustc", b"#!/bin/sh\n", 0o755),
            Part::File("README", b"text", 0o644),
        ]);

        let unpacked = unpack_bytes(&home, &bytes, "payload-1").unwrap();
        let mode = |name: &str| {
            fs::metadata(unpacked.dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(
            mode("bin/rustc"),
            0o755,
            "a compiler that cannot execute is not a compiler"
        );
        assert_eq!(mode("README"), 0o644);
    }

    #[test]
    fn an_entry_that_climbs_out_is_refused_and_nothing_is_published() {
        let scratch = Scratch::new("escape");
        let home = scratch.home();
        let bytes = raw_archive(&[
            Raw {
                name: "bin/rustc",
                typeflag: b'0',
                link: "",
                body: b"ok",
            },
            Raw {
                name: "../escaped.txt",
                typeflag: b'0',
                link: "",
                body: b"should never be written",
            },
        ]);

        let error = unpack_bytes(&home, &bytes, "payload-1").unwrap_err();
        assert!(
            matches!(
                error,
                UnpackError::Refused {
                    why: Refusal::LeavesTheTree,
                    ..
                }
            ),
            "{error}"
        );
        assert!(
            !home.join("escaped.txt").exists(),
            "an entry climbed out of the toolchain directory"
        );
        assert!(
            home.join("toolchain/0.1.0").symlink_metadata().is_err(),
            "a refused payload was published anyway"
        );
        assert!(
            !crate::toolchain::staging_dir(&home, "0.1.0").exists(),
            "a refused payload kept its staging directory"
        );
        assert!(matches!(
            status(&home, "0.1.0", "payload-1").unwrap(),
            Status::Absent
        ));
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let scratch = Scratch::new("absolute");
        let home = scratch.home();
        let bytes = raw_archive(&[Raw {
            name: "/etc/payload",
            typeflag: b'0',
            link: "",
            body: b"x",
        }]);

        let error = unpack_bytes(&home, &bytes, "payload-1").unwrap_err();
        assert!(
            matches!(
                error,
                UnpackError::Refused {
                    why: Refusal::AbsolutePath,
                    ..
                }
            ),
            "{error}"
        );
        assert!(!Path::new("/etc/payload").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_points_outside_is_refused() {
        let scratch = Scratch::new("link-out");
        let home = scratch.home();
        let bytes = archive(&[Part::Symlink("bin/rustc", "../../../etc/passwd")]);

        let error = unpack_bytes(&home, &bytes, "payload-1").unwrap_err();
        assert!(
            matches!(
                error,
                UnpackError::Refused {
                    why: Refusal::LinkEscapes,
                    ..
                }
            ),
            "{error}"
        );
    }

    // This expectation holds only where a link can be made at all: Windows refuses the entry
    // instead of creating one, which is a separate behaviour with its own test below.
    #[cfg(unix)]
    #[test]
    fn a_link_that_points_inside_is_kept() {
        let scratch = Scratch::new("link-in");
        let home = scratch.home();
        let bytes = archive(&[
            Part::Dir("bin"),
            Part::File("lib/rustc-real", b"binary", 0o755),
            Part::Symlink("bin/rustc", "../lib/rustc-real"),
        ]);

        let unpacked = unpack_bytes(&home, &bytes, "payload-1").unwrap();
        let link = unpacked.dir.join("bin/rustc");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("../lib/rustc-real")
        );
    }

    /// Where a link cannot be made, the entry is refused by name and the payload is not published.
    ///
    /// The refusal is the other half of the decision above: a link that cannot be created is not
    /// silently turned into a copy, because a copy is a different tree.
    #[cfg(not(unix))]
    #[test]
    fn a_link_is_refused_rather_than_copied_where_a_link_cannot_be_made() {
        let scratch = Scratch::new("link-platform");
        let home = scratch.home();
        let bytes = archive(&[
            Part::Dir("bin"),
            Part::File("lib/rustc-real", b"binary", 0o755),
            Part::Symlink("bin/rustc", "../lib/rustc-real"),
        ]);

        let error = unpack_bytes(&home, &bytes, "payload-1").unwrap_err();
        match &error {
            UnpackError::Write { entry, source } => {
                assert_eq!(entry, "bin/rustc");
                assert_eq!(source.kind(), io::ErrorKind::Unsupported);
                assert_eq!(
                    source.to_string(),
                    "a toolchain payload may not carry symbolic links on this platform"
                );
            }
            other => panic!("{other}"),
        }
        assert!(
            home.join("toolchain/0.1.0").symlink_metadata().is_err(),
            "a payload carrying a link was published anyway"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_would_not_escape_before_it_arrives_is_still_checked_as_written() {
        let scratch = Scratch::new("link-depth");
        let home = scratch.home();

        // One `..` from the root of the tree is out, so a link at the root may not carry one —
        // while the same target from inside `bin/` is still inside. The depth is the entry's, not
        // the target's.
        let escaping = archive(&[Part::Symlink("rustc", "../anything")]);
        let refused = unpack_bytes(&home, &escaping, "payload-1").unwrap_err();
        assert!(
            matches!(
                refused,
                UnpackError::Refused {
                    why: Refusal::LinkEscapes,
                    ..
                }
            ),
            "{refused}"
        );

        let inside = archive(&[
            Part::Dir("bin"),
            Part::Symlink("bin/rustc", "../rustc-real"),
        ]);
        let unpacked = unpack_bytes(&home, &inside, "payload-1").unwrap();
        assert_eq!(
            fs::read_link(unpacked.dir.join("bin/rustc")).unwrap(),
            Path::new("../rustc-real")
        );
    }

    #[test]
    fn a_device_node_is_refused_rather_than_made() {
        let scratch = Scratch::new("fifo");
        let home = scratch.home();
        let bytes = archive(&[Part::Fifo("pipe")]);

        let error = unpack_bytes(&home, &bytes, "payload-1").unwrap_err();
        assert!(
            matches!(
                error,
                UnpackError::Refused {
                    why: Refusal::NotAPayload,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_stream_that_is_not_zstd_says_so_and_publishes_nothing() {
        let scratch = Scratch::new("not-zstd");
        let home = scratch.home();

        let error = unpack_bytes(&home, b"this is not a zstd stream", "payload-1").unwrap_err();
        assert!(matches!(error, UnpackError::Archive(_)), "{error}");
        assert!(home.join("toolchain/0.1.0").symlink_metadata().is_err());
    }

    #[test]
    fn a_box_the_archive_promised_and_did_not_deliver_is_not_a_toolchain() {
        let scratch = Scratch::new("truncated");
        let home = scratch.home();

        // The tar is cut in the middle of the file's body, which is what a payload killed during
        // packing looks like: the header is complete and the bytes behind it are not.
        let whole = archive(&[Part::File("bin/rustc", &[b'x'; 4096], 0o755)]);
        let mut short = Vec::new();
        let mut encoder = zstd::Encoder::new(Vec::new(), 3).unwrap();
        let full_tar = {
            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(4096);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "bin/rustc", &[b'x'; 4096][..])
                .unwrap();
            builder.into_inner().unwrap()
        };
        // Keep the first 1024 bytes of the uncompressed tar: header plus part of the body.
        encoder.write_all(&full_tar[..1024]).unwrap();
        short.extend(encoder.finish().unwrap());

        let error = unpack_bytes(&home, &short, "payload-1").unwrap_err();
        assert!(matches!(error, UnpackError::Archive(_)), "{error}");
        assert!(
            home.join("toolchain/0.1.0").symlink_metadata().is_err(),
            "a truncated payload was published as a toolchain"
        );
        // The whole one still unpacks, so the truncation — not the archive shape — is what failed.
        assert!(unpack_bytes(&home, &whole, "payload-1").is_ok());
    }

    #[test]
    fn publishing_the_payload_that_is_already_there_is_refused() {
        let scratch = Scratch::new("twice");
        let home = scratch.home();
        let bytes = archive(&[Part::File("bin/rustc", b"first", 0o755)]);
        let first = unpack_bytes(&home, &bytes, "payload-1").unwrap();

        let again = unpack_bytes(&home, &bytes, "payload-1").unwrap_err();
        assert!(matches!(again, UnpackError::Adopt(_)), "{again}");
        // The working toolchain is untouched: a refused second unpack is not a repair.
        assert_eq!(
            fs::read(first.dir.join("bin/rustc")).unwrap(),
            b"first".to_vec()
        );
    }

    #[test]
    fn a_dot_slash_is_ordinary_and_not_a_refusal() {
        let scratch = Scratch::new("dot");
        let home = scratch.home();
        let bytes = archive(&[Part::File("./bin/rustc", b"ok", 0o755)]);

        let unpacked = unpack_bytes(&home, &bytes, "payload-1").unwrap();
        assert!(unpacked.dir.join("bin/rustc").is_file());
    }

    #[test]
    fn a_staging_directory_left_by_a_killed_unpack_is_removed() {
        let scratch = Scratch::new("drop");
        let home = scratch.home();
        let bytes = raw_archive(&[Raw {
            name: "../out",
            typeflag: b'0',
            link: "",
            body: b"x",
        }]);

        let staging: Staging = crate::toolchain::staging(&home, "0.1.0").unwrap();
        let written = staging.dir().join("half");
        fs::write(&written, b"partial").unwrap();
        let error = unpack(io::Cursor::new(bytes), staging, "payload-1").unwrap_err();
        assert!(matches!(error, UnpackError::Refused { .. }), "{error}");
        assert!(
            !written.exists(),
            "the staging directory outlived a failed unpack"
        );
    }
}
