//! Where a tool call's path actually points, and the kinds of subject a tool cannot touch.
//!
//! Two decisions live here because both are about the path *before* any I/O, and both are the
//! kind of thing that goes wrong silently:
//!
//! - **A relative path joins onto the working directory the caller supplied**, never onto the
//!   process's own cwd. The two differ (the daemon's cwd is wherever it was launched), and the
//!   fence judges the joined path — so a tool that resolved one way while the fence judged
//!   another would be a hole with no bug in either.
//! - **A special file is refused by kind, before it is opened.** A FIFO, a socket or a device
//!   node does not fail on open: it **blocks** until something on the other end appears, so a
//!   tool that reaches `open` never returns. A directory is the fourth refused subject, and the
//!   only one that would have failed loudly instead.

use std::path::{Path, PathBuf};

use deepnest_sandbox::canonical_path;

/// A subject that cannot be read or written as an ordinary file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialFile {
    Directory,
    Fifo,
    Socket,
    Device,
    SymlinkLoop,
    /// A path that exists but whose kind this build cannot name — refused rather than guessed
    /// at, because guessing "regular file" is how a blocking open happens.
    Unknown,
}

impl SpecialFile {
    pub fn as_str(self) -> &'static str {
        match self {
            SpecialFile::Directory => "directory",
            SpecialFile::Fifo => "named pipe (FIFO)",
            SpecialFile::Socket => "socket",
            SpecialFile::Device => "device node",
            SpecialFile::SymlinkLoop => "symlink loop",
            SpecialFile::Unknown => "special file",
        }
    }

    /// The refusal, naming the kind and a way forward.
    ///
    /// Each of these blocks or fails in a way that says nothing about the cause, so the message
    /// has to say it: "is a directory" from an `open` is several dozen lines deep in a stack the
    /// model never sees.
    pub fn refusal(self, path: &Path, tool: &str) -> String {
        let kind = self.as_str();
        let advice = match self {
            SpecialFile::Directory => "list it with the glob tool, or name a file inside it",
            SpecialFile::Fifo | SpecialFile::Socket => {
                "reading one waits for a writer and never returns; drive the process that owns \
                 it with the bash tool instead"
            }
            SpecialFile::Device => "device nodes are not readable files in the agent's reach",
            SpecialFile::SymlinkLoop | SpecialFile::Unknown => {
                "the path does not name one ordinary file; resolve the link or name the target"
            }
        };
        format!(
            "{tool} cannot act on {} because it is a {kind}: {advice}",
            path.display()
        )
    }
}

/// The kind of subject at `path`, or `None` when it is an ordinary file.
///
/// `None` also covers a path that does not exist: a path that is not there has no kind to
/// judge, and for the write tool creating it is the call's job.
#[cfg(unix)]
pub fn special_file_kind(path: &Path) -> Option<SpecialFile> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        // Follow one hop by hand: `metadata` answers for the target, and a loop would come back
        // as an I/O error the caller cannot tell apart from "not there".
        return match std::fs::metadata(path) {
            Ok(target) => classify(&target),
            Err(_) => Some(SpecialFile::SymlinkLoop),
        };
    }
    classify(&metadata)
}

#[cfg(unix)]
fn classify(metadata: &std::fs::Metadata) -> Option<SpecialFile> {
    use std::os::unix::fs::FileTypeExt;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        return Some(SpecialFile::Directory);
    }
    if file_type.is_fifo() {
        return Some(SpecialFile::Fifo);
    }
    if file_type.is_socket() {
        return Some(SpecialFile::Socket);
    }
    if file_type.is_char_device() || file_type.is_block_device() {
        return Some(SpecialFile::Device);
    }
    None
}

/// The non-unix reading: a directory is the only special kind std names portably.
#[cfg(not(unix))]
pub fn special_file_kind(path: &Path) -> Option<SpecialFile> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.is_dir() {
        Some(SpecialFile::Directory)
    } else if !metadata.is_file() {
        Some(SpecialFile::Unknown)
    } else {
        None
    }
}

/// Resolve a tool call's `file_path` into the path the fence judges and the bytes touch.
///
/// Canonical, because that is the identity the fence compares; a relative path is joined onto
/// `workdir` first, so the same relative spelling means the same file in every tool.
pub fn resolve_target(file_path: &str, workdir: &Path) -> PathBuf {
    let spelled = Path::new(file_path);
    let joined = if spelled.is_absolute() {
        spelled.to_path_buf()
    } else {
        workdir.join(spelled)
    };
    canonical_path(&joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("deepnest-tools-path-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_relative_path_joins_the_caller_s_workdir_not_the_process_cwd() {
        let scratch = Scratch::new("relative");
        let resolved = resolve_target("src/main.rs", &scratch.base);
        assert!(
            resolved.starts_with(canonical_path(&scratch.base)),
            "{resolved:?}"
        );
        assert!(resolved.ends_with("src/main.rs"), "{resolved:?}");
    }

    #[test]
    fn an_absolute_path_is_used_as_given() {
        let scratch = Scratch::new("absolute");
        let file = scratch.base.join("a.txt");
        let resolved = resolve_target(&file.to_string_lossy(), Path::new("/nowhere"));
        assert_eq!(resolved, canonical_path(&file));
    }

    #[test]
    fn a_directory_is_named_as_one_rather_than_read() {
        let scratch = Scratch::new("dir");
        assert_eq!(
            special_file_kind(&scratch.base),
            Some(SpecialFile::Directory)
        );
        assert!(SpecialFile::Directory
            .refusal(&scratch.base, "read")
            .contains("glob tool"));
    }

    #[test]
    fn an_ordinary_file_has_no_special_kind() {
        let scratch = Scratch::new("regular");
        let file = scratch.base.join("a.txt");
        fs::write(&file, b"hello").unwrap();
        assert_eq!(special_file_kind(&file), None);
    }

    #[test]
    fn a_path_that_does_not_exist_has_no_kind_to_judge() {
        let scratch = Scratch::new("missing");
        assert_eq!(special_file_kind(&scratch.base.join("absent.txt")), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_before_it_can_block() {
        // The failure this prevents: `open` on a FIFO waits for the other end, so the tool call
        // never returns and the agent's whole turn dies with it.
        let scratch = Scratch::new("fifo");
        let fifo = scratch.base.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(status.success());
        assert_eq!(special_file_kind(&fifo), Some(SpecialFile::Fifo));
        assert!(SpecialFile::Fifo
            .refusal(&fifo, "write")
            .contains("waits for a writer"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_an_ordinary_file_is_judged_by_its_target() {
        let scratch = Scratch::new("symlink");
        let real = scratch.base.join("real.txt");
        fs::write(&real, b"x").unwrap();
        let link = scratch.base.join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(special_file_kind(&link), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_refused_rather_than_reported_as_missing() {
        let scratch = Scratch::new("loop");
        let first = scratch.base.join("loop-a");
        let second = scratch.base.join("loop-b");
        std::os::unix::fs::symlink(&second, &first).unwrap();
        std::os::unix::fs::symlink(&first, &second).unwrap();
        assert_eq!(special_file_kind(&first), Some(SpecialFile::SymlinkLoop));
    }
}
