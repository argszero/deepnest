//! `read`, `write` and `edit` — the three tools that touch one file.
//!
//! Reads are not fenced and writes are: the sandbox's job is to say where a *write* may land
//! (`Policy::write_refusal`, the same answer the OS profile is built from), and a read that the
//! profile allows the process to see is a read. What both obey is the kind of subject
//! ([`special_file_kind`]) — a FIFO or a device node blocks instead of failing, so it is refused
//! before any descriptor is opened.
//!
//! The `edit` contract is the one with a history: an `old_string` that matches **more than once**
//! is an error rather than a first-match edit, because a model that meant a different occurrence
//! has no way to see the difference in the result. `replace_all` is the explicit way to say "all
//! of them", and it is read as a boolean or as the two string spellings of one — never as a
//! truthiness test (see [`crate::args`]).

use std::fs;
use std::path::{Path, PathBuf};

use deepnest_llm::ToolDefinition;
use serde_json::{json, Value};

use crate::args::{boolean_argument, count_argument, string_argument};
use crate::path::{resolve_target, special_file_kind};
use crate::{Tool, ToolContext, ToolResult, MAX_RESULT_CHARS};

/// How many lines a read returns when the caller does not say.
pub const DEFAULT_LINE_LIMIT: u64 = 2_000;

/// The ceiling a caller may raise `line_limit` to.
///
/// A cap rather than an unbounded read: one tool result is one message, and a 40 MB log read
/// whole costs the turn it was meant to inform.
pub const MAX_LINE_LIMIT: u64 = 8_000;

/// The `file_path` of a call, or the refusal its absence earns.
fn file_path(args: &Value, tool: &str) -> Result<String, ToolResult> {
    string_argument(args, &["file_path"]).map_err(|reason| ToolResult::refused(tool, reason))
}

/// The resolved, canonical target of a call.
fn resolve(spelled: &str, ctx: &ToolContext, tool: &str) -> Result<PathBuf, ToolResult> {
    let path = resolve_target(spelled, &ctx.workdir);
    if let Some(kind) = special_file_kind(&path) {
        return Err(ToolResult::refused(tool, kind.refusal(&path, tool)));
    }
    Ok(path)
}

/// The fence, asked once, in the one place every write passes through.
fn require_write(path: &Path, ctx: &ToolContext, tool: &str) -> Result<(), ToolResult> {
    match ctx.policy.write_refusal(path) {
        Some(reason) => Err(ToolResult::refused(tool, reason)),
        None => Ok(()),
    }
}

/// Read one file as text, refusing a binary subject instead of decoding it lossily.
fn read_text(path: &Path, tool: &str) -> Result<String, ToolResult> {
    let bytes = fs::read(path)
        .map_err(|err| ToolResult::error(tool, format!("cannot read {}: {err}", path.display())))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(err) => Err(ToolResult::error(
            tool,
            format!(
                "{} is not UTF-8 text ({} bytes in total, the first byte that is not valid text \
                 is at offset {}), so it is not something this tool can show. Inspect it with the \
                 bash tool instead, for example `head -c 2000 {}`.",
                path.display(),
                err.as_bytes().len(),
                err.utf8_error().valid_up_to(),
                path.display()
            ),
        )),
    }
}

// ── read ────────────────────────────────────────────────────────────────────────────────────

/// Show one file with line numbers.
///
/// The numbers are the point: `start_line` is how a model resumes a long file, and a hint that
/// names a line number the tool will then refuse is worse than no hint — hence the checks below
/// refuse an out-of-range `start_line` instead of silently returning nothing.
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "read",
            "Read a text file from the app's directory and show it with line numbers. Use \
             start_line and line_limit to page through a file that does not fit in one result; \
             the result says which lines it showed and how many the file has.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file. Relative paths are joined onto the app directory."
                    },
                    "start_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "First line to show, 1-based. Defaults to 1."
                    },
                    "line_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": format!("How many lines to show. Defaults to {DEFAULT_LINE_LIMIT}; at most {MAX_LINE_LIMIT}.")
                    }
                },
                "required": ["file_path"]
            }),
        )
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let spelled = match file_path(args, "read") {
            Ok(spelled) => spelled,
            Err(refusal) => return refusal,
        };
        let start = match count_argument(args, &["start_line", "offset"], 1, Some(1)) {
            Ok(Some(start)) => start,
            Ok(None) => 1,
            Err(reason) => return ToolResult::refused("read", reason),
        };
        let limit =
            match count_argument(args, &["line_limit", "limit"], 1, Some(DEFAULT_LINE_LIMIT)) {
                Ok(Some(limit)) => limit.min(MAX_LINE_LIMIT),
                Ok(None) => DEFAULT_LINE_LIMIT,
                Err(reason) => return ToolResult::refused("read", reason),
            };
        let path = match resolve(&spelled, ctx, "read") {
            Ok(path) => path,
            Err(refusal) => return refusal,
        };
        let text = match read_text(&path, "read") {
            Ok(text) => text,
            Err(refusal) => return refusal,
        };
        if text.is_empty() {
            return ToolResult::ok("read", format!("{} is empty", path.display()));
        }
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        if start as usize > total {
            return ToolResult::error(
                "read",
                format!(
                    "{} has {total} line(s), so start_line {start} is past the end. Read it again \
                     with start_line 1, or with a start_line at or below {total}.",
                    path.display()
                ),
            );
        }
        let first = start as usize;
        let last = (first - 1 + limit as usize).min(total);
        // The width matches the largest number the file could print, so the columns stay
        // aligned from one page to the next.
        let width = total.to_string().len().max(4);
        let mut body = String::new();
        for (index, line) in lines[first - 1..last].iter().enumerate() {
            let number = first + index;
            if number > first {
                body.push('\n');
            }
            body.push_str(&format!("{number:>width$}\t{line}"));
        }
        let footer = if last < total {
            format!(
                "\n\n[lines {first}-{last} of {total}; continue with start_line {}]",
                last + 1
            )
        } else {
            format!("\n\n[lines {first}-{last} of {total}]")
        };
        ToolResult::ok("read", format!("{body}{footer}")).truncate(MAX_RESULT_CHARS)
    }
}

// ── write ───────────────────────────────────────────────────────────────────────────────────

/// Write a whole file.
///
/// Whole-file on purpose: a partial write needs an edit's matching rules, and offering both
/// invites a model to append by rewriting a file it never read.
pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "write",
            "Write a file, replacing whatever it held. Parent directories are created. To change \
             part of an existing file, use the edit tool instead — it refuses to guess which \
             occurrence you meant.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file. Relative paths are joined onto the app directory."
                    },
                    "content": {
                        "type": "string",
                        "description": "The complete new contents of the file."
                    }
                },
                "required": ["file_path", "content"]
            }),
        )
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let spelled = match file_path(args, "write") {
            Ok(spelled) => spelled,
            Err(refusal) => return refusal,
        };
        let content = match string_argument(args, &["content"]) {
            Ok(content) => content,
            Err(reason) => return ToolResult::refused("write", reason),
        };
        let path = match resolve(&spelled, ctx, "write") {
            Ok(path) => path,
            Err(refusal) => return refusal,
        };
        if let Err(refusal) = require_write(&path, ctx, "write") {
            return refusal;
        }
        // The parent is a write too — a directory created outside the fence is the same effect
        // reached sideways — so it is judged before `create_dir_all` runs.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                if let Err(refusal) = require_write(parent, ctx, "write") {
                    return refusal;
                }
                if let Err(err) = fs::create_dir_all(parent) {
                    return ToolResult::error(
                        "write",
                        format!("cannot create {}: {err}", parent.display()),
                    );
                }
            }
        }
        if let Err(err) = fs::write(&path, content.as_bytes()) {
            return ToolResult::error("write", format!("cannot write {}: {err}", path.display()));
        }
        let lines = content.lines().count();
        ToolResult::ok(
            "write",
            format!(
                "wrote {} byte(s), {lines} line(s) to {}",
                content.len(),
                path.display()
            ),
        )
    }
}

// ── edit ────────────────────────────────────────────────────────────────────────────────────

/// Replace an exact string inside one file.
///
/// The rule the tool exists for: **an ambiguous match is refused**. A first-match edit on a
/// string that occurs twice changes a place the caller did not name, and the result reports
/// success either way — the model cannot tell, so it cannot recover.
pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "edit",
            "Replace an exact string in a file. old_string must match exactly once, whitespace \
             and indentation included; if it matches more than once the call is refused unless \
             replace_all is true. Read the file first — the tool does not guess at your \
             indentation.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file. Relative paths are joined onto the app directory."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The exact text to replace, including indentation."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The text to put in its place. An empty string deletes it."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence instead of requiring a unique one. Defaults to false."
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        )
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let spelled = match file_path(args, "edit") {
            Ok(spelled) => spelled,
            Err(refusal) => return refusal,
        };
        let old_string = match string_argument(args, &["old_string"]) {
            Ok(text) => text,
            Err(reason) => return ToolResult::refused("edit", reason),
        };
        let new_string = match string_argument(args, &["new_string"]) {
            Ok(text) => text,
            Err(reason) => return ToolResult::refused("edit", reason),
        };
        let replace_all = match boolean_argument(args, &["replace_all"], false) {
            Ok(flag) => flag,
            Err(reason) => return ToolResult::refused("edit", reason),
        };
        if old_string.is_empty() {
            // An empty needle matches everywhere, including at byte zero and at the very end:
            // there is no reading of it that names one place.
            return ToolResult::refused(
                "edit",
                "old_string is empty, so it matches at every position of the file; give the text \
                 you mean to replace",
            );
        }
        let path = match resolve(&spelled, ctx, "edit") {
            Ok(path) => path,
            Err(refusal) => return refusal,
        };
        if let Err(refusal) = require_write(&path, ctx, "edit") {
            return refusal;
        }
        let text = match read_text(&path, "edit") {
            Ok(text) => text,
            Err(refusal) => return refusal,
        };
        let occurrences = text.matches(&old_string).count();
        if occurrences == 0 {
            let lines = text.lines().count();
            return ToolResult::error(
                "edit",
                format!(
                    "old_string was not found in {} ({lines} line(s)). It must match the file \
                     exactly, indentation included; read the file and copy the text from what it \
                     shows.",
                    path.display()
                ),
            );
        }
        if occurrences > 1 && !replace_all {
            return ToolResult::refused(
                "edit",
                format!(
                    "old_string occurs {occurrences} times in {}; this call is refused rather \
                     than applied to the first one. Include enough surrounding text to name one \
                     occurrence, or pass replace_all: true to change all {occurrences}.",
                    path.display()
                ),
            );
        }
        let replaced = text.replace(&old_string, &new_string);
        if let Err(err) = fs::write(&path, replaced.as_bytes()) {
            return ToolResult::error("edit", format!("cannot write {}: {err}", path.display()));
        }
        ToolResult::ok(
            "edit",
            format!(
                "replaced {occurrences} occurrence(s) in {}; it is now {} line(s)",
                path.display(),
                replaced.lines().count()
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_sandbox::{Mode, Policy};
    use std::path::PathBuf;

    /// A real temporary directory, and a policy whose writable set is it.
    ///
    /// Real paths matter here: the fence canonicalises, and macOS resolves `/tmp` to
    /// `/private/tmp`, so a hard-coded spelling would test the mock rather than the fence.
    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "deepnest-tools-files-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn ctx(&self, mode: Mode) -> ToolContext {
            ToolContext::new(
                Policy::from_roots(
                    mode,
                    self.base.clone(),
                    self.base.clone(),
                    self.base.clone(),
                ),
                self.base.clone(),
            )
        }

        fn file(&self, name: &str, content: &str) -> PathBuf {
            let path = self.base.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn read_numbers_the_lines_it_shows() {
        let scratch = Scratch::new("read-numbers");
        scratch.file("a.rs", "one\ntwo\nthree\n");
        let result = ReadTool.call(
            &json!({"file_path": "a.rs"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("1\tone"), "{}", result.content);
        assert!(result.content.contains("3\tthree"), "{}", result.content);
        assert!(
            result.content.contains("lines 1-3 of 3"),
            "{}",
            result.content
        );
    }

    #[test]
    fn read_pages_and_names_the_next_start_line() {
        let scratch = Scratch::new("read-pages");
        scratch.file("a.txt", "a\nb\nc\nd\ne\n");
        let result = ReadTool.call(
            &json!({"file_path": "a.txt", "start_line": 2, "line_limit": 2}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("2\tb"), "{}", result.content);
        assert!(!result.content.contains("4\td"), "{}", result.content);
        assert!(
            result.content.contains("start_line 4"),
            "{}",
            result.content
        );
    }

    #[test]
    fn a_start_line_past_the_end_is_refused_with_the_real_length() {
        // The measured failure: a hint named a line the tool then refused, so the model could
        // neither read on nor learn why.
        let scratch = Scratch::new("read-past-end");
        scratch.file("a.txt", "a\nb\n");
        let result = ReadTool.call(
            &json!({"file_path": "a.txt", "start_line": 9}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error);
        assert!(result.content.contains("2 line(s)"), "{}", result.content);
    }

    #[test]
    fn read_refuses_a_directory_instead_of_failing_inside_open() {
        let scratch = Scratch::new("read-dir");
        fs::create_dir_all(scratch.base.join("sub")).unwrap();
        let result = ReadTool.call(
            &json!({"file_path": "sub"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error);
        assert!(result.content.contains("directory"), "{}", result.content);
    }

    #[test]
    fn read_refuses_binary_rather_than_decoding_it() {
        let scratch = Scratch::new("read-binary");
        fs::write(scratch.base.join("blob"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let result = ReadTool.call(
            &json!({"file_path": "blob"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error);
        assert!(result.content.contains("not UTF-8"), "{}", result.content);
    }

    #[test]
    fn write_creates_the_parent_and_reports_what_it_wrote() {
        let scratch = Scratch::new("write");
        let result = WriteTool.call(
            &json!({"file_path": "src/deep/file.txt", "content": "hello\n"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            fs::read_to_string(scratch.base.join("src/deep/file.txt")).unwrap(),
            "hello\n"
        );
        assert!(result.content.contains("1 line(s)"), "{}", result.content);
    }

    #[test]
    fn a_write_outside_the_writable_set_is_refused_with_the_policy_s_own_words() {
        let scratch = Scratch::new("write-outside");
        let outside = std::env::temp_dir().join(format!(
            "deepnest-tools-files-outside-{}",
            std::process::id()
        ));
        let result = WriteTool.call(
            &json!({"file_path": outside.to_string_lossy(), "content": "x"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error);
        assert!(
            result.content.contains("outside the writable set"),
            "{}",
            result.content
        );
        assert!(!outside.exists());
    }

    #[test]
    fn a_read_only_policy_refuses_every_write() {
        let scratch = Scratch::new("write-readonly");
        let result = WriteTool.call(
            &json!({"file_path": "a.txt", "content": "x"}),
            &scratch.ctx(Mode::ReadOnly),
        );
        assert!(result.is_error);
        assert!(result.content.contains("read-only"), "{}", result.content);
        assert!(!scratch.base.join("a.txt").exists());
    }

    #[test]
    fn edit_replaces_a_unique_occurrence() {
        let scratch = Scratch::new("edit-unique");
        scratch.file("a.rs", "fn main() {\n    run();\n}\n");
        let result = EditTool.call(
            &json!({"file_path": "a.rs", "old_string": "    run();", "new_string": "    run_twice();"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            fs::read_to_string(scratch.base.join("a.rs")).unwrap(),
            "fn main() {\n    run_twice();\n}\n"
        );
        assert!(
            result.content.contains("1 occurrence(s)"),
            "{}",
            result.content
        );
    }

    #[test]
    fn edit_refuses_an_ambiguous_match_rather_than_taking_the_first() {
        let scratch = Scratch::new("edit-ambiguous");
        scratch.file("a.txt", "x = 1;\nx = 1;\n");
        let result = EditTool.call(
            &json!({"file_path": "a.txt", "old_string": "x = 1;", "new_string": "x = 2;"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error);
        assert!(result.content.contains("2 times"), "{}", result.content);
        // And the file is untouched: a refused call has no effect.
        assert_eq!(
            fs::read_to_string(scratch.base.join("a.txt")).unwrap(),
            "x = 1;\nx = 1;\n"
        );
    }

    #[test]
    fn replace_all_changes_every_occurrence() {
        let scratch = Scratch::new("edit-all");
        scratch.file("a.txt", "x = 1;\nx = 1;\n");
        let result = EditTool.call(
            &json!({"file_path": "a.txt", "old_string": "x = 1;", "new_string": "x = 2;", "replace_all": true}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            fs::read_to_string(scratch.base.join("a.txt")).unwrap(),
            "x = 2;\nx = 2;\n"
        );
    }

    #[test]
    fn the_string_spelling_false_does_not_turn_replace_all_on() {
        // The measured bug: `"false"` was truthy, so a call that excluded a place edited it.
        let scratch = Scratch::new("edit-string-false");
        scratch.file("a.txt", "x;\nx;\n");
        let result = EditTool.call(
            &json!({"file_path": "a.txt", "old_string": "x;", "new_string": "y;", "replace_all": "false"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error, "{}", result.content);
        assert_eq!(
            fs::read_to_string(scratch.base.join("a.txt")).unwrap(),
            "x;\nx;\n"
        );
    }

    #[test]
    fn edit_reports_a_missing_string_with_the_file_s_length() {
        let scratch = Scratch::new("edit-missing");
        scratch.file("a.txt", "one\ntwo\n");
        let result = EditTool.call(
            &json!({"file_path": "a.txt", "old_string": "three", "new_string": "four"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error);
        assert!(result.content.contains("2 line(s)"), "{}", result.content);
    }

    #[test]
    fn an_empty_old_string_is_refused_because_it_names_no_place() {
        let scratch = Scratch::new("edit-empty-old");
        scratch.file("a.txt", "one\n");
        let result = EditTool.call(
            &json!({"file_path": "a.txt", "old_string": "", "new_string": "x"}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(result.is_error);
        assert!(result.content.contains("empty"), "{}", result.content);
    }

    #[test]
    fn an_empty_new_string_deletes_the_match() {
        let scratch = Scratch::new("edit-delete");
        scratch.file("a.txt", "keep\ndrop\nkeep\n");
        // Unique with the trailing newline: exactly one line carries it.
        let result = EditTool.call(
            &json!({"file_path": "a.txt", "old_string": "drop\n", "new_string": ""}),
            &scratch.ctx(Mode::WorkspaceWrite),
        );
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            fs::read_to_string(scratch.base.join("a.txt")).unwrap(),
            "keep\nkeep\n"
        );
    }

    #[test]
    fn every_tool_names_itself_and_requires_a_file_path() {
        for tool in [
            Box::new(ReadTool) as Box<dyn Tool>,
            Box::new(WriteTool) as Box<dyn Tool>,
            Box::new(EditTool) as Box<dyn Tool>,
        ] {
            let definition = tool.definition();
            assert_eq!(definition.function.name, tool.name());
            assert!(definition.function.parameters["required"]
                .as_array()
                .unwrap()
                .contains(&json!("file_path")));
        }
    }
}
