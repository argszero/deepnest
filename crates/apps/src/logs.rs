//! What an app said when it ran, read from the end.
//!
//! Every app writes one log file, `<home>/logs/<slug>.log`, and the reader is usually an agent
//! diagnosing a build that just failed — so it wants the tail, not the file. Two decisions follow:
//!
//! - **The tail is bounded by bytes, not by lines.** Counting lines from the start means reading the
//!   whole file, and a build log is exactly the file that is allowed to grow without limit: a
//!   compiler error loop can produce hundreds of megabytes. The reader seeks to the last
//!   [`MAX_TAIL_BYTES`] and keeps the last lines of that window.
//! - **A window that starts mid-line drops that line.** The first bytes of a seeked window are the
//!   tail of a line whose beginning was never read, and presenting it as a line would put a
//!   truncated compiler error in front of whoever is reading.
//!
//! A missing log is `Ok(None)`, not an error: an app that has never been started has no log, and
//! that is the state a freshly created app is in.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::slug::Slug;

/// The directory under the deepnest home that holds every app's log.
pub const LOGS_DIR: &str = "logs";

/// How much of the end of a log is read at most.
pub const MAX_TAIL_BYTES: u64 = 64 * 1024;

/// Where an app's log lives.
pub fn log_path(home: &Path, slug: &Slug) -> PathBuf {
    home.join(LOGS_DIR).join(format!("{slug}.log"))
}

/// Where a build's output goes.
///
/// Separate from the app's own log because the two answer different questions — "why did the build
/// fail" and "what is the app saying now" — and one file holding both would make the tail of either
/// unreadable. The suffix is spelled here, beside the runtime path, so whoever finds one finds the
/// other.
pub fn build_log_path(home: &Path, slug: &Slug) -> PathBuf {
    home.join(LOGS_DIR).join(format!("{slug}.build.log"))
}

/// The last `lines` lines of `path`, or `None` when the file does not exist.
///
/// When older lines were left out, the result says so on its own last line: a tail that silently
/// began partway through looks exactly like a short log.
pub fn tail(path: &Path, lines: usize) -> io::Result<Option<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    let length = file.metadata()?.len();
    let from = length.saturating_sub(MAX_TAIL_BYTES);
    if from > 0 {
        file.seek(SeekFrom::Start(from))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    // A log is not required to be UTF-8: it is whatever a compiler and a program wrote to one
    // descriptor, and refusing to show it because one line is not decodable would hide the rest.
    let window = String::from_utf8_lossy(&bytes).into_owned();

    let (text, clipped) = if from > 0 {
        match window.split_once('\n') {
            Some((_, rest)) => (rest.to_string(), true),
            // One line longer than the whole window: none of it was read from its start, so none of
            // it can be shown as a line.
            None => (String::new(), true),
        }
    } else {
        (window, false)
    };

    let all: Vec<&str> = text.lines().collect();
    let over_budget = all.len() > lines;
    let kept = if over_budget {
        &all[all.len() - lines..]
    } else {
        &all[..]
    };

    let mut out = kept.join("\n");
    if clipped || over_budget {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("[earlier lines are not shown]");
    }
    Ok(Some(out))
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
            let base =
                std::env::temp_dir().join(format!("deepnest-logs-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn write(&self, text: &str) -> PathBuf {
            let dir = self.base.join(LOGS_DIR);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("notes.log");
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn numbered(count: usize) -> String {
        (1..=count)
            .map(|index| format!("line {index}\n"))
            .collect::<String>()
    }

    #[test]
    fn the_log_lives_under_the_home_in_one_place_per_app() {
        let slug = Slug::parse("notes").unwrap();
        assert_eq!(
            log_path(Path::new("/home/ada/.deepnest"), &slug),
            PathBuf::from("/home/ada/.deepnest/logs/notes.log")
        );
    }

    #[test]
    fn a_missing_log_is_none_rather_than_an_error() {
        // A freshly created app has never run; asking for its log is not a failure.
        let scratch = Scratch::new("missing");
        assert!(tail(&scratch.base.join("logs/notes.log"), 10)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_file_shorter_than_the_request_comes_back_whole_and_unsignposted() {
        let scratch = Scratch::new("short");
        let path = scratch.write("building\nbuilt\n");

        let text = tail(&path, 10).unwrap().unwrap();

        assert_eq!(text, "building\nbuilt");
        assert!(!text.contains("not shown"), "{text}");
    }

    #[test]
    fn a_longer_file_returns_its_last_lines() {
        let scratch = Scratch::new("long");
        let path = scratch.write(&numbered(100));

        let text = tail(&path, 3).unwrap().unwrap();

        assert!(text.starts_with("line 98\n"), "{text}");
        assert!(text.contains("line 100"), "{text}");
        assert!(!text.contains("line 97"), "{text}");
    }

    #[test]
    fn a_cut_tail_says_earlier_lines_are_missing() {
        // Without the note, a tail that began partway through reads as a complete short log.
        let scratch = Scratch::new("noted");
        let path = scratch.write(&numbered(10));

        let text = tail(&path, 2).unwrap().unwrap();
        assert!(text.ends_with("[earlier lines are not shown]"), "{text}");
    }

    #[test]
    fn only_the_end_of_a_large_file_is_read() {
        // The window is what keeps a 200 MB build log from becoming 200 MB of memory.
        let scratch = Scratch::new("large");
        let mut body = String::from("FIRST LINE MARKER\n");
        body.push_str(&"x".repeat(200 * 1024));
        body.push_str("\nlast line\n");
        let path = scratch.write(&body);

        let text = tail(&path, 5).unwrap().unwrap();

        assert!(!text.contains("FIRST LINE MARKER"), "read the whole file");
        assert!(text.contains("last line"), "{text}");
        assert!(text.ends_with("[earlier lines are not shown]"), "{text}");
    }

    #[test]
    fn a_line_longer_than_the_window_is_not_shown_half_read() {
        let scratch = Scratch::new("one-huge-line");
        let mut body = "A".repeat((MAX_TAIL_BYTES as usize) + 1000);
        body.push_str("\nok\n");
        let path = scratch.write(&body);

        let text = tail(&path, 10).unwrap().unwrap();

        assert!(
            !text.contains('A'),
            "a half-read line was presented: {text}"
        );
        assert!(text.starts_with("ok"), "{text}");
    }

    #[test]
    fn asking_for_no_lines_returns_nothing_but_the_signpost() {
        let scratch = Scratch::new("zero");
        let path = scratch.write(&numbered(5));

        let text = tail(&path, 0).unwrap().unwrap();
        assert_eq!(text, "[earlier lines are not shown]");
    }

    #[test]
    fn a_log_that_is_not_utf8_still_shows_its_readable_lines() {
        let scratch = Scratch::new("not-utf8");
        let dir = scratch.base.join(LOGS_DIR);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.log");
        fs::write(&path, b"build failed\n\xff\xfe\nsee above\n").unwrap();

        let text = tail(&path, 5).unwrap().unwrap();

        assert!(text.contains("build failed"), "{text}");
        assert!(text.contains("see above"), "{text}");
    }
}
