//! An app's name — one string that is a directory on three platforms and a URL path segment.
//!
//! Two namespaces, one string, and their rules are not the same. The stricter rule wins, so the
//! alphabet is lowercase letters, digits and `-`:
//!
//! - **As a directory name** it is created on macOS, Windows and Linux, so the union of their
//!   rules applies. Windows reserves `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9` and
//!   `LPT1`–`LPT9` as file names — a directory with one of those names cannot be created there at
//!   all. Windows also strips a trailing dot or space, so `notes.` and `notes` would be one
//!   directory. And NTFS, like the APFS most Macs run, is case-insensitive, so `Notes` and
//!   `notes` are also one directory. Refusing uppercase removes that whole class of collision
//!   instead of leaving it to be detected later, when two apps quietly share a folder.
//! - **As a path segment** it appears in `/app/<slug>/`, where it has to be unambiguous with no
//!   percent-encoding: `[a-z0-9-]` cannot be read as an escape, a query or a separator. That is
//!   also why a `.` or a `/` in the name is refused rather than escaped — a slug that could spell
//!   `..` or contain a separator is a directory-traversal argument waiting for a caller that
//!   forgets to check.
//!
//! The name **starts and ends with a letter or a digit**. A leading `-` reads as a flag to every
//! command-line parser that might later be handed this name, a leading `.` hides the directory on
//! unix and would name something that is not a name at all, and a trailing `-` has no convention
//! behind it. Refusing both ends keeps the rule one sentence.
//!
//! **No deepnest name is reserved**, and that is a decision rather than an omission: an app lives
//! *under* `apps/` and is addressed *under* `/app/`, and neither namespace has another occupant —
//! `skills/`, `toolchain/`, `logs/` and `config.toml` are siblings of `apps/`, not of an app.

use std::fmt;

/// The longest accepted name, in characters.
///
/// Well under the 255 bytes a single path component gets on the filesystems involved, because the
/// name is also a log file name (`logs/<slug>.log`) and a URL segment, and a name whose safety
/// depended on the surrounding template would stop being checkable here.
pub const MAX_CHARS: usize = 64;

/// A validated app name.
///
/// Constructing one is the only way to obtain it, so a `Slug` in a directory path or a URL is
/// proof the rules above were applied.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slug(String);

/// Why a name is not usable as an app's slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlugError {
    /// Nothing was given.
    Empty,
    /// Whitespace at either end. It is invisible in a directory listing, so it is refused rather
    /// than trimmed: `" notes"` and `"notes "` would be different apps that look identical.
    SurroundingWhitespace,
    /// Longer than [`MAX_CHARS`].
    TooLong { chars: usize },
    /// A character outside `[a-z0-9-]`.
    Illegal { character: char, position: usize },
    /// A `-` at the start or the end.
    HyphenAtEdge { edge: &'static str },
    /// A name Windows reserves for a device.
    ReservedDeviceName,
}

impl fmt::Display for SlugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "an app's name cannot be empty"),
            Self::SurroundingWhitespace => write!(
                f,
                "has whitespace at the ends, which is invisible in every directory listing"
            ),
            Self::TooLong { chars } => write!(
                f,
                "is {chars} characters long; the limit is {MAX_CHARS}"
            ),
            Self::Illegal { character, position } => write!(
                f,
                "contains {character:?} at position {position}; an app's name may use lowercase \
                 letters, digits and '-' (uppercase would name the same directory as the lowercase \
                 spelling on Windows and on a default macOS volume, and anything else would have to \
                 be escaped in a URL)"
            ),
            Self::HyphenAtEdge { edge } => write!(
                f,
                "{edge} with '-'; an app's name starts and ends with a letter or a digit"
            ),
            Self::ReservedDeviceName => write!(
                f,
                "is a name Windows reserves for a device (CON, PRN, AUX, NUL, COM1-9, LPT1-9), so a \
                 directory with it cannot be created there"
            ),
        }
    }
}

impl std::error::Error for SlugError {}

impl Slug {
    /// Validate `raw` as an app name.
    ///
    /// Nothing is normalized on the way in: no trimming, no lowercasing, no separator rewriting.
    /// Each of those would make a name the user did not write, and the directory it names would
    /// then be findable only by knowing the transformation.
    pub fn parse(raw: &str) -> Result<Self, SlugError> {
        if raw.is_empty() {
            return Err(SlugError::Empty);
        }
        if raw.trim() != raw {
            return Err(SlugError::SurroundingWhitespace);
        }

        let characters: Vec<char> = raw.chars().collect();
        if characters.len() > MAX_CHARS {
            return Err(SlugError::TooLong {
                chars: characters.len(),
            });
        }

        for (position, character) in characters.iter().enumerate() {
            if !matches!(character, 'a'..='z' | '0'..='9' | '-') {
                return Err(SlugError::Illegal {
                    character: *character,
                    position,
                });
            }
        }

        if characters.first() == Some(&'-') {
            return Err(SlugError::HyphenAtEdge { edge: "starts" });
        }
        if characters.last() == Some(&'-') {
            return Err(SlugError::HyphenAtEdge { edge: "ends" });
        }
        if is_reserved_device_name(raw) {
            return Err(SlugError::ReservedDeviceName);
        }

        Ok(Self(raw.to_string()))
    }

    /// The name, as it will appear in a path and in a URL.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for Slug {
    type Err = SlugError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

/// Whether Windows would read the name as a device rather than as a file name.
///
/// The comparison is done on the lowercase form regardless of how the caller spelled it: the check
/// runs only after the alphabet check, so `raw` is already lowercase — but a future change to that
/// alphabet must not turn this function into one that misses `CON`.
fn is_reserved_device_name(raw: &str) -> bool {
    let lowered = raw.to_ascii_lowercase();
    if matches!(lowered.as_str(), "con" | "prn" | "aux" | "nul") {
        return true;
    }
    // `COM1`–`COM9` and `LPT1`–`LPT9`: a digit, not a longer number — `com10` is an ordinary name.
    for prefix in ["com", "lpt"] {
        if let Some(rest) = lowered.strip_prefix(prefix) {
            let mut characters = rest.chars();
            if let (Some(digit), None) = (characters.next(), characters.next()) {
                if digit.is_ascii_digit() && digit != '0' {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts(raw: &str) {
        assert!(Slug::parse(raw).is_ok(), "{raw:?} should be accepted");
    }

    fn refuses(raw: &str) -> SlugError {
        Slug::parse(raw).expect_err(&format!("{raw:?} should be refused"))
    }

    #[test]
    fn ordinary_names_are_accepted() {
        for name in ["notes", "a", "1", "my-notes", "app2", "a-b-c-1", "2fa"] {
            accepts(name);
        }
    }

    #[test]
    fn the_accepted_name_survives_parsing_unchanged() {
        // No trimming, no case folding: the slug has to name the directory the user asked for.
        let slug = Slug::parse("my-notes").unwrap();
        assert_eq!(slug.as_str(), "my-notes");
        assert_eq!(slug.to_string(), "my-notes");
    }

    #[test]
    fn an_empty_name_is_refused() {
        assert_eq!(refuses(""), SlugError::Empty);
    }

    #[test]
    fn surrounding_whitespace_is_refused_rather_than_trimmed() {
        // Trimming would create a directory whose name differs from the one the user typed, and
        // every listing would then show a name they did not write.
        assert_eq!(refuses(" notes"), SlugError::SurroundingWhitespace);
        assert_eq!(refuses("notes "), SlugError::SurroundingWhitespace);
        assert_eq!(refuses("notes\t"), SlugError::SurroundingWhitespace);
    }

    #[test]
    fn uppercase_is_refused_and_the_message_says_why() {
        let error = refuses("Notes");
        assert_eq!(
            error,
            SlugError::Illegal {
                character: 'N',
                position: 0
            }
        );
        let message = error.to_string();
        assert!(message.contains("lowercase"), "{message}");
        assert!(message.contains("Windows"), "{message}");
    }

    #[test]
    fn a_path_separator_cannot_be_smuggled_into_a_name() {
        // The slug becomes a directory name under `apps/`, so `../skills` must not parse: it would
        // be traversal in every caller that joins without checking.
        for hostile in ["../skills", "a/b", "a\\b", "..", "."] {
            let error = refuses(hostile);
            assert!(
                matches!(error, SlugError::Illegal { .. }),
                "{hostile:?} gave {error:?}"
            );
        }
    }

    #[test]
    fn length_is_counted_in_characters_and_capped() {
        accepts(&"a".repeat(MAX_CHARS));
        assert_eq!(
            refuses(&"a".repeat(MAX_CHARS + 1)),
            SlugError::TooLong {
                chars: MAX_CHARS + 1
            }
        );
    }

    #[test]
    fn a_hyphen_at_either_end_is_refused() {
        assert_eq!(
            refuses("-notes"),
            SlugError::HyphenAtEdge { edge: "starts" }
        );
        assert_eq!(refuses("notes-"), SlugError::HyphenAtEdge { edge: "ends" });
        accepts("notes-2");
    }

    #[test]
    fn the_names_windows_reserves_for_devices_are_refused() {
        for reserved in ["con", "prn", "aux", "nul", "com1", "com9", "lpt1", "lpt9"] {
            assert_eq!(
                refuses(reserved),
                SlugError::ReservedDeviceName,
                "{reserved:?}"
            );
        }
    }

    #[test]
    fn names_that_merely_start_like_a_device_name_are_fine() {
        // `com0`, `com10` and `console` are ordinary names; a prefix test would refuse them.
        for ordinary in ["com0", "com10", "lpt", "console", "null", "aux2"] {
            accepts(ordinary);
        }
    }

    #[test]
    fn a_slug_parses_from_a_string_via_from_str() {
        let slug: Slug = "notes".parse().unwrap();
        assert_eq!(slug.as_str(), "notes");
    }
}
