//! A skill file's frontmatter: the few lines above the instructions, read without a YAML library.
//!
//! A skill is Markdown with a header:
//!
//! ```text
//! ---
//! name: browser-harness
//! description: Always use this for any web interaction.
//! ---
//! # Instructions
//! ```
//!
//! Three rules, and each one is here because the alternative was already paid for somewhere:
//!
//! - **Indented lines are not fields.** A skill may carry a nested block — a `metadata:` list, for
//!   instance — and a nested `description:` must not overwrite the top-level one. Reading every
//!   line with a colon in it is how a skill ends up describing itself with a sentence from further
//!   down its own file.
//! - **The closing delimiter is a line, not an occurrence.** Splitting the text on `---` finds the
//!   first three hyphens anywhere, which fails on a description that contains them and on a body
//!   that does. This reads line by line, so the header ends where the file says it does.
//! - **A quoted value is unquoted, and nothing else is interpreted.** `name: "notes"` is `notes`;
//!   there is no second pass for escapes, because a skill's name and description are text the
//!   agent reads, not a format anything re-parses.

use std::fmt;

/// The fields above a skill's instructions.
///
/// A small ordered list rather than a map: the number of fields is tiny, order is stable for
/// tests, and lookup is a scan of something that fits in a cache line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    fields: Vec<(String, String)>,
}

impl Frontmatter {
    /// A field's value, by name. The first occurrence wins.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every field, in the order the file listed them.
    pub fn fields(&self) -> &[(String, String)] {
        &self.fields
    }

    /// Whether the header carried no field at all.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Why a file has no header this loader can read.
///
/// Three variants rather than one `None`, because they want three different fixes: a file that
/// does not open with `---` was never meant to be a skill, a header that never closes is a typo
/// with the delimiter, and an empty header is a file someone started and did not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoFrontmatter {
    /// The file does not open with a `---` line.
    Missing,
    /// It opens with `---` and no later line closes it.
    Unclosed,
    /// Both delimiters are there and there is no field between them.
    Empty,
}

impl fmt::Display for NoFrontmatter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("it does not start with a `---` line"),
            Self::Unclosed => f.write_str("its `---` header is never closed"),
            Self::Empty => f.write_str("its header has no fields"),
        }
    }
}

impl std::error::Error for NoFrontmatter {}

/// Split a skill file into its header and its instructions.
///
/// A leading byte-order mark is dropped first: an editor on Windows writes one, and a file that
/// reads as headerless because of an invisible character is a file whose author would have to be
/// told something that is not their fault.
pub fn split(text: &str) -> Result<(Frontmatter, &str), NoFrontmatter> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut offset = 0;
    let mut opened = false;
    let mut fields = Vec::new();

    for line in text.split_inclusive('\n') {
        offset += line.len();
        let bare = line.trim_end_matches(['\r', '\n']);

        if !opened {
            if bare.trim_end() != "---" {
                return Err(NoFrontmatter::Missing);
            }
            opened = true;
            continue;
        }
        if bare.trim_end() == "---" {
            let frontmatter = Frontmatter { fields };
            if frontmatter.is_empty() {
                return Err(NoFrontmatter::Empty);
            }
            return Ok((frontmatter, text[offset..].trim()));
        }

        // Indented → a nested block, not a top-level field. This is the rule that keeps a skill's
        // own `description` from being overwritten by one further down its file.
        if bare.starts_with([' ', '\t']) {
            continue;
        }
        let trimmed = bare.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        fields.push((key.to_string(), unquote(value.trim())));
    }

    // Opened and never closed: the whole file is header, which is a mistake in the file rather
    // than a skill with no instructions.
    if opened {
        Err(NoFrontmatter::Unclosed)
    } else {
        Err(NoFrontmatter::Missing)
    }
}

/// A value with one layer of matching quotes removed.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'"' || first == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_is_read_and_the_instructions_follow_it() {
        let text = "---\nname: notes\ndescription: What this does\n---\n# Body\ntext\n";
        let (frontmatter, body) = split(text).unwrap();
        assert_eq!(frontmatter.get("name"), Some("notes"));
        assert_eq!(frontmatter.get("description"), Some("What this does"));
        assert_eq!(body, "# Body\ntext");
    }

    #[test]
    fn a_nested_field_does_not_overwrite_the_top_level_one() {
        // The failure this exists for: a skill that also carries a `metadata:` block would
        // otherwise describe itself with the nested sentence. Both orders are checked, because
        // only one of them is caught by "the first field of a name wins" — a nested block that
        // appears *before* the real field is the ordering that makes indentation load-bearing.
        for text in [
            "---\nname: notes\ndescription: The real one\nmetadata:\n  description: nested\n---\nbody",
            "---\nname: notes\nmetadata:\n  description: nested\ndescription: The real one\n---\nbody",
        ] {
            let (frontmatter, _) = split(text).unwrap();
            assert_eq!(frontmatter.get("description"), Some("The real one"), "{text}");
            // Three: the two real fields and the `metadata:` key the nested block hangs from.
            assert_eq!(frontmatter.fields().len(), 3, "{text}");
        }
    }

    #[test]
    fn a_tab_indented_field_is_nested_too() {
        let text = "---\nname: notes\ndescription: top\nmetadata:\n\tdescription: nested\n---\n";
        assert_eq!(split(text).unwrap().0.get("description"), Some("top"));
    }

    #[test]
    fn a_description_containing_hyphens_is_not_a_closing_delimiter() {
        // Splitting the text on `---` finds the first occurrence anywhere; reading lines does not.
        let text = "---\ndescription: a --- b and c\n---\nbody";
        let (frontmatter, body) = split(text).unwrap();
        assert_eq!(frontmatter.get("description"), Some("a --- b and c"));
        assert_eq!(body, "body");
    }

    #[test]
    fn a_body_containing_the_delimiter_survives_whole() {
        let text = "---\ndescription: d\n---\nbefore\n---\nafter\n";
        let (_, body) = split(text).unwrap();
        assert_eq!(body, "before\n---\nafter");
    }

    #[test]
    fn a_quoted_value_is_unquoted_and_nothing_else_is_interpreted() {
        let text = "---\nname: \"notes\"\ndescription: 'a: b'\n---\n";
        let (frontmatter, _) = split(text).unwrap();
        assert_eq!(frontmatter.get("name"), Some("notes"));
        assert_eq!(frontmatter.get("description"), Some("a: b"));
    }

    #[test]
    fn a_mismatched_quote_is_left_alone() {
        let (frontmatter, _) = split("---\ndescription: \"half\n---\n").unwrap();
        assert_eq!(frontmatter.get("description"), Some("\"half"));
    }

    #[test]
    fn comments_and_blank_lines_are_not_fields() {
        let (frontmatter, _) = split("---\n# a note\n\nname: notes\n---\n").unwrap();
        assert_eq!(frontmatter.fields().len(), 1);
    }

    #[test]
    fn crlf_line_endings_are_read_like_lf() {
        let text = "---\r\nname: notes\r\ndescription: d\r\n---\r\nbody\r\n";
        let (frontmatter, body) = split(text).unwrap();
        assert_eq!(frontmatter.get("name"), Some("notes"));
        assert_eq!(body, "body");
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_header() {
        let text = "\u{feff}---\nname: notes\n---\nbody";
        assert_eq!(split(text).unwrap().0.get("name"), Some("notes"));
    }

    #[test]
    fn the_three_ways_a_header_can_be_wrong_are_told_apart() {
        assert_eq!(split("no header here").unwrap_err(), NoFrontmatter::Missing);
        assert_eq!(
            split("---\nname: notes\n").unwrap_err(),
            NoFrontmatter::Unclosed
        );
        assert_eq!(split("---\n---\nbody").unwrap_err(), NoFrontmatter::Empty);
        // And a file that is only a header is unclosed, not empty.
        assert_eq!(split("---\n").unwrap_err(), NoFrontmatter::Unclosed);
    }

    #[test]
    fn an_empty_body_is_fine() {
        let (frontmatter, body) = split("---\nname: notes\n---").unwrap();
        assert_eq!(frontmatter.get("name"), Some("notes"));
        assert_eq!(body, "");
    }
}
