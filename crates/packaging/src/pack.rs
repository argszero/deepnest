//! Taking a payload off disk: a whole toolchain in, a `zstd` archive out.
//!
//! [`unpack`](crate::unpack) is the reader; this is the writer it was written against. The release
//! pipeline builds a toolchain somewhere and hands it here, and what comes out is embedded in the
//! binary and read back on a machine that has never compiled anything.
//!
//! Four decisions:
//!
//! - **A stream out, never a `Vec`.** The payload is the largest thing deepnest carries, so it is
//!   compressed straight into the caller's writer. A `pack` that returned bytes would hold a second
//!   copy of the whole toolchain in memory at exactly the moment the machine is already busy.
//! - **The same tree packs to the same bytes.** Entries are sorted, and the header's time and
//!   ownership fields are zeroed: a packer that emitted whatever `read_dir` returned first, with each
//!   file's own modification time, would turn an unchanged tree into a different archive on every
//!   build — and this archive is signed and shipped, so "the payload did not change" has to be
//!   something two builds can agree on.
//! - **What the reader refuses, the writer refuses first.** A symlink whose target climbs out of the
//!   tree is refused here as well, through the reader's own [`checked_link`] rather than a second
//!   copy of the rule, and so is an entry that is not a file, a directory or a link. The reader has
//!   to refuse them because it cannot see who built the archive; the packer must refuse them because
//!   it can — at the one moment the fix is a line in a build script instead of a first run that ends
//!   in a sentence about a toolchain the user never asked about.
//! - **The mode travels.** The executable bit is the difference between a toolchain and a compressed
//!   pile of text, and nothing else in the archive records it.
//!
//! A tree deepnest has already unpacked is refused rather than packed again: it carries the marker
//! [`adopt`](crate::toolchain::Staging::adopt) writes, and an archive carrying one would contradict
//! the id the next unpack is told to publish. That refusal is a sentence naming the file, because
//! pointing the packer at `~/.deepnest/toolchain/<version>/` is the mistake it is easy to make.
//!
//! What this module does **not** do: sign anything, or decide where the archive lives afterwards.
//! The signature covers bytes this writer produces, and where it goes is the release pipeline's.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::toolchain::MARKER;
use crate::unpack::{checked_link, Refusal};

/// How hard the payload is compressed.
///
/// The trade is the release build's time against the user's first run: what is compressed once here
/// is decompressed once on every machine that installs deepnest. This is `zstd`'s own default, and
/// the level every existing payload in this crate's tests is built at.
const LEVEL: i32 = 3;

/// What a pack produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    /// How many entries were written — the number [`crate::unpack::unpack`] will read back, which is
    /// the one check a packer can make about its own output before shipping it.
    pub entries: usize,
}

/// Why a tree could not be packed.
///
/// As with [`crate::unpack::UnpackError`], the difference between the variants is what each says
/// about the tree rather than what the caller does next: nothing is written on any of them.
#[derive(Debug)]
pub enum PackError {
    /// The tree could not be read.
    Read {
        /// The entry that could not be read, as the archive would spell it.
        entry: String,
        /// What the filesystem said.
        source: io::Error,
    },
    /// An entry was refused before anything was written for it.
    Refused {
        /// The entry's path, as the archive would spell it.
        entry: String,
        /// Which refusal it met.
        why: Refusal,
    },
    /// The archive could not be written.
    Archive(io::Error),
    /// The tree is one deepnest has already unpacked, not one to pack.
    AlreadyUnpacked {
        /// The directory the packer was pointed at.
        dir: PathBuf,
    },
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { entry, source } => {
                write!(
                    f,
                    "the toolchain tree could not be read at `{entry}`: {source}"
                )
            }
            Self::Refused { entry, why } => {
                write!(f, "the toolchain tree was refused at `{entry}`: {why}")
            }
            Self::Archive(error) => {
                write!(f, "the toolchain payload could not be written: {error}")
            }
            Self::AlreadyUnpacked { dir } => write!(
                f,
                "`{}` is already an unpacked toolchain: it carries `{MARKER}`, which the unpack \
                 writes and the tree being packed does not",
                dir.display()
            ),
        }
    }
}

impl std::error::Error for PackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Archive(error) => Some(error),
            Self::Refused { .. } | Self::AlreadyUnpacked { .. } => None,
        }
    }
}

/// Pack the tree under `root` into `out`, and say how many entries went in.
///
/// Everything is checked before anything is written: the whole tree is walked into a list, sorted,
/// and only then compressed. A tree that will be refused is therefore refused without a partial
/// archive handed to a caller who might ship it.
///
/// `root` is the **contents** of a toolchain, not a directory containing one: what is under it is
/// what a first run will find under `~/.deepnest/toolchain/<version>/`.
pub fn pack<W: Write>(root: &Path, out: W) -> Result<Packed, PackError> {
    if fs::symlink_metadata(root.join(MARKER)).is_ok() {
        return Err(PackError::AlreadyUnpacked {
            dir: root.to_path_buf(),
        });
    }
    let entries = collect(root)?;

    let mut encoder = zstd::Encoder::new(out, LEVEL).map_err(PackError::Archive)?;
    {
        let mut builder = tar::Builder::new(&mut encoder);
        for (relative, kind) in &entries {
            write_entry(&mut builder, root, relative, kind)?;
        }
        builder.finish().map_err(PackError::Archive)?;
    }
    encoder.finish().map_err(PackError::Archive)?;

    Ok(Packed {
        entries: entries.len(),
    })
}

/// One entry the walk found, and what the archive has to record about it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// A regular file, with the permission bits to carry into the header.
    ///
    /// A hard link is packed as a file: it is a second name for the same bytes, and finding the
    /// pairs would mean an inode table this crate has no other use for. What is unpacked is the same
    /// tree either way — the duplicate costs one entry, not a wrong toolchain.
    File { mode: u32 },
    /// A directory, which is listed explicitly so a tree without files still arrives whole.
    Dir { mode: u32 },
    /// A symbolic link, already known to point inside the tree.
    Link { target: PathBuf },
}

/// Walk `root` into a list ordered the way it will be written.
fn collect(root: &Path) -> Result<Vec<(PathBuf, Kind)>, PackError> {
    let mut found = Vec::new();
    walk(root, Path::new(""), &mut found)?;
    // Sorted, so two builds of one tree are one archive. Component order also puts a directory
    // before everything under it, which is the order an unpack is happiest with.
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(found)
}

/// Append everything under `root/prefix` to `found`, depth first.
fn walk(root: &Path, prefix: &Path, found: &mut Vec<(PathBuf, Kind)>) -> Result<(), PackError> {
    let dir = root.join(prefix);
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|source| read_failed(prefix, source))? {
        let entry = entry.map_err(|source| read_failed(prefix, source))?;
        names.push(entry.file_name());
    }
    names.sort();

    for name in names {
        let relative = prefix.join(&name);
        let path = root.join(&relative);
        // `symlink_metadata`, so a link to a directory is packed as the link it is: following it
        // would copy the directory's contents in and then record the link too — twice the tree, one
        // of them wrong.
        let meta = fs::symlink_metadata(&path).map_err(|source| read_failed(&relative, source))?;
        let file_type = meta.file_type();

        if file_type.is_symlink() {
            let target = fs::read_link(&path).map_err(|source| read_failed(&relative, source))?;
            checked_link(&relative, &target).map_err(|why| PackError::Refused {
                entry: said(&relative),
                why,
            })?;
            found.push((relative, Kind::Link { target }));
        } else if file_type.is_dir() {
            let mode = mode(&meta, 0o755);
            found.push((relative.clone(), Kind::Dir { mode }));
            walk(root, &relative, found)?;
        } else if file_type.is_file() {
            let mode = mode(&meta, 0o644);
            found.push((relative, Kind::File { mode }));
        } else {
            // A device node, a fifo or a socket. Nothing in a toolchain is one, and a reader that
            // would have to create one needs a privilege the first run should not hold.
            return Err(PackError::Refused {
                entry: said(&relative),
                why: Refusal::NotAPayload,
            });
        }
    }
    Ok(())
}

/// Write one entry into the archive.
fn write_entry<W: Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    relative: &Path,
    kind: &Kind,
) -> Result<(), PackError> {
    let path = root.join(relative);
    let mut header = tar::Header::new_gnu();
    // Zeroed rather than taken from the file: two builds of an unchanged tree have to produce the
    // same bytes, and a checkout's timestamps and ownership are neither the toolchain's nor stable.
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);

    match kind {
        Kind::Dir { mode } => {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(*mode);
            header.set_cksum();
            builder
                .append_data(&mut header, relative, io::empty())
                .map_err(PackError::Archive)?;
        }
        Kind::File { mode } => {
            let entry = said(relative);
            let mut file =
                fs::File::open(&path).map_err(|source| PackError::Read { entry, source })?;
            // The size the open file reports, not the one the walk saw: a file that grew between
            // the two would otherwise be written with a header describing less than its body.
            let size = file
                .metadata()
                .map_err(|source| read_failed(relative, source))?
                .len();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(size);
            header.set_mode(*mode);
            header.set_cksum();
            builder
                .append_data(&mut header, relative, &mut file)
                .map_err(|source| read_failed(relative, source))?;
        }
        Kind::Link { target } => {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_link_name(target).map_err(PackError::Archive)?;
            header.set_cksum();
            builder
                .append_data(&mut header, relative, io::empty())
                .map_err(PackError::Archive)?;
        }
    }
    Ok(())
}

/// The permission bits an entry carries into the archive.
///
/// Only the permission bits travel, the same field the reader takes them from: a `tar` mode field is
/// documented to hold the file type as well, and writing that back would describe a directory that
/// is not one.
#[cfg(unix)]
fn mode(meta: &fs::Metadata, _fallback: u32) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

/// Windows has no permission bits, and no reading platform applies a mode it does not have: a
/// payload is built for one platform and unpacked on that same platform. The constants are what a
/// well-formed header carries, not a claim about the tree.
#[cfg(not(unix))]
fn mode(_meta: &fs::Metadata, fallback: u32) -> u32 {
    fallback
}

fn read_failed(entry: &Path, source: io::Error) -> PackError {
    PackError::Read {
        entry: said(entry),
        source,
    }
}

/// An archive path as the reader will print it, with `/` between components on every platform.
fn said(relative: &Path) -> String {
    if relative.as_os_str().is_empty() {
        return ".".to_string();
    }
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Read;

    use crate::toolchain::{staging, status, Status};
    use crate::unpack::{unpack, UnpackError};

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-pack-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn tree(&self) -> PathBuf {
            self.base.join("tree")
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

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// A tree as a list of lines, so one assertion shows the difference rather than a boolean.
    ///
    /// The mode is read on the platforms that have one: on Windows neither the packer nor the
    /// unpacker applies it, so comparing it there would compare two constants.
    fn shape(root: &Path) -> Vec<String> {
        fn walk(root: &Path, prefix: &Path, out: &mut Vec<String>) {
            let dir = root.join(prefix);
            let mut names: Vec<_> = fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            for name in names {
                let relative = prefix.join(&name);
                let path = root.join(&relative);
                let meta = fs::symlink_metadata(&path).unwrap();
                let (kind, body) = if meta.file_type().is_symlink() {
                    (
                        "link".to_string(),
                        fs::read_link(&path).unwrap().display().to_string(),
                    )
                } else if meta.is_dir() {
                    ("dir".to_string(), String::new())
                } else {
                    let mut text = String::new();
                    fs::File::open(&path)
                        .unwrap()
                        .read_to_string(&mut text)
                        .unwrap();
                    ("file".to_string(), text)
                };
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    format!(" {:o}", meta.permissions().mode() & 0o777)
                };
                #[cfg(not(unix))]
                let mode = String::new();
                out.push(format!("{}{mode} {kind} {body}", said(&relative)));
                if meta.is_dir() {
                    walk(root, &relative, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, Path::new(""), &mut out);
        out
    }

    fn unpack_bytes(home: &Path, bytes: &[u8]) -> Result<PathBuf, UnpackError> {
        let staging = staging(home, "0.1.0").unwrap();
        unpack(io::Cursor::new(bytes.to_vec()), staging, "0.1.0").map(|unpacked| unpacked.dir)
    }

    #[test]
    fn a_packed_tree_unpacks_to_the_tree_it_was_packed_from() {
        // The whole point, in one assertion: what the release pipeline packs is what a first run
        // finds. The mode is part of it — an executable unpacked without its bit is not a toolchain.
        let scratch = Scratch::new("round-trip");
        let tree = scratch.tree();
        write(&tree.join("bin").join("rustc"), "#!/bin/sh\n");
        write(&tree.join("share").join("doc.txt"), "read me\n");
        fs::create_dir_all(tree.join("empty")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                tree.join("bin").join("rustc"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let mut archive = Vec::new();
        let packed = pack(&tree, &mut archive).unwrap();
        let dir = unpack_bytes(&scratch.home(), &archive).unwrap();

        // The unpack adds exactly one thing the packer did not pack: the marker `adopt` writes, so
        // that a later run can tell what it is trusting. Everything else is byte for byte the tree
        // above, mode included.
        //
        // The marker is found rather than spelled: `shape` prints a mode only where the platform has
        // one, so an expected line carrying `644` would read as a Windows failure in a test about a
        // payload. What it must be is a file whose body is the payload id, and that is what is
        // asserted.
        let mut rest = shape(&dir);
        let at = rest
            .iter()
            .position(|line| line.split(' ').next() == Some(MARKER))
            .unwrap_or_else(|| panic!("the unpack published no `{MARKER}`: {rest:?}"));
        let marker = rest.remove(at);
        assert!(
            marker.ends_with(" file 0.1.0\n"),
            "the marker does not name the payload it was unpacked for: {marker:?}"
        );
        assert_eq!(shape(&tree), rest);
        assert_eq!(
            packed.entries,
            shape(&tree).len(),
            "the packer counted a different number of entries than the tree holds"
        );
        // And it is published: a tree on disk that `status` does not call ready is one a first run
        // will unpack again.
        match status(&scratch.home(), "0.1.0", "0.1.0").unwrap() {
            Status::Ready { dir: published } => assert_eq!(published, dir),
            other => panic!("the unpacked tree was not published: {other:?}"),
        }
    }

    #[test]
    fn the_same_tree_packs_to_the_same_bytes() {
        // A payload that changes between two builds of an unchanged tree is one nobody can compare,
        // and this one is signed: the signature would differ for a reason no diff can show.
        let scratch = Scratch::new("deterministic");
        let tree = scratch.tree();
        write(&tree.join("b").join("second.txt"), "two\n");
        write(&tree.join("a").join("first.txt"), "one\n");

        let mut first = Vec::new();
        let mut second = Vec::new();
        // A later mtime on one of the files, which a header built from the file's own metadata
        // would carry into the archive.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&tree.join("a").join("first.txt"), "one\n");

        pack(&tree, &mut first).unwrap();
        pack(&tree, &mut second).unwrap();
        assert_eq!(first, second, "two packs of one tree produced two archives");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_inside_the_tree_travels_as_a_link() {
        // Links are how a toolchain is laid out without copying itself, so a packer that turned one
        // into a file would ship something that works and is twice the size — or, worse, resolve it
        // once and ship a stale copy.
        let scratch = Scratch::new("link");
        let tree = scratch.tree();
        write(&tree.join("bin").join("real"), "real\n");
        std::os::unix::fs::symlink("real", tree.join("bin").join("alias")).unwrap();

        let mut archive = Vec::new();
        pack(&tree, &mut archive).unwrap();
        let dir = unpack_bytes(&scratch.home(), &archive).unwrap();

        let alias = dir.join("bin").join("alias");
        assert!(
            fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the alias arrived as something other than a link"
        );
        assert_eq!(fs::read_link(&alias).unwrap(), Path::new("real"));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_climbs_out_is_refused_before_anything_is_written() {
        // The reader refuses this because it cannot see who built the archive. The packer refuses it
        // because it can — where the fix is a line in a build script rather than a first run that
        // ends in a sentence about a toolchain the user never asked about.
        let scratch = Scratch::new("escaping-link");
        let tree = scratch.tree();
        fs::create_dir_all(&tree).unwrap();
        let outside = scratch.base.join("outside");
        write(&outside, "not part of any toolchain\n");
        // No link can be made without a privilege Windows does not give a plain process, so this
        // case is unreachable there through a tree — it reaches the reader through an archive
        // instead, which is the reader's test rather than this one's.
        std::os::unix::fs::symlink("../outside", tree.join("escape")).unwrap();

        let mut archive = Vec::new();
        let error = pack(&tree, &mut archive).unwrap_err();
        assert!(matches!(
            error,
            PackError::Refused {
                why: Refusal::LinkEscapes,
                ..
            }
        ));
        assert!(archive.is_empty(), "a refused tree was still written out");
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_is_refused() {
        // A device node, a fifo or a socket is not a toolchain. A packer that walked past one would
        // ship an archive the reader has to refuse, which is the same fault one step later and in
        // front of a user instead of a build log.
        let scratch = Scratch::new("socket");
        let tree = scratch.tree();
        fs::create_dir_all(&tree).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(tree.join("s")).unwrap();

        let mut archive = Vec::new();
        let error = pack(&tree, &mut archive).unwrap_err();
        assert!(matches!(
            error,
            PackError::Refused {
                why: Refusal::NotAPayload,
                ..
            }
        ));
    }

    #[test]
    fn an_already_unpacked_toolchain_is_refused_by_the_marker_it_carries() {
        // An archive carrying a marker would contradict the id the next unpack is told to publish,
        // and `adopt` would overwrite one of them without a word. Pointing the packer at
        // `~/.deepnest/toolchain/<version>/` is the mistake that produces it.
        let scratch = Scratch::new("marker");
        let tree = scratch.tree();
        write(&tree.join(MARKER), "0.1.0\n");

        let mut archive = Vec::new();
        let error = pack(&tree, &mut archive).unwrap_err();
        match &error {
            PackError::AlreadyUnpacked { dir } => assert_eq!(dir, &tree),
            other => panic!("expected the marker to be named, got {other}"),
        }
        let said = error.to_string();
        assert!(said.contains("already an unpacked toolchain"), "{said}");
        assert!(said.contains(MARKER), "{said}");
    }

    #[test]
    fn a_tree_that_is_not_there_is_an_error_naming_the_directory() {
        let scratch = Scratch::new("absent");
        let mut archive = Vec::new();
        let error = pack(&scratch.tree(), &mut archive).unwrap_err();
        assert!(matches!(error, PackError::Read { .. }), "{error}");
        assert!(error.to_string().contains('.'), "{error}");
    }
}
