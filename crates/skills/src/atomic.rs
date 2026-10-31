//! Writing a file whole: a temporary neighbour, then a rename.
//!
//! Every file this crate writes — the catalog, the state file, an installed skill — is read by
//! something else while a previous write may still be in flight. A half-written catalog parses as
//! an empty one, a half-written state file reads as "nothing is managed", and a half-written skill
//! is instructions the agent may act on. A rename is the one operation a reader cannot catch
//! halfway, so each of those writes goes through here rather than through `fs::write`.
//!
//! The scratch name carries the process id, so two deepnest processes writing the same target do
//! not consume each other's temporary file. The loser of that race is overwritten rather than
//! corrupted, which is the outcome worth having for a file whose content is a snapshot anyway.

use std::fs;
use std::io;
use std::path::Path;

/// Write `text` to `target`, creating its directory, whole or not at all.
pub(crate) fn write(target: &Path, text: &str) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let scratch = parent.join(format!(
        ".{}.{}.tmp",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        std::process::id()
    ));
    fs::write(&scratch, text)?;
    match fs::rename(&scratch, target) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&scratch);
            Err(error)
        }
    }
}
