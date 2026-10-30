//! `glob` and `grep` — finding things by name and by content.
//!
//! Both walk the tree themselves, and both obey two rules stated once here rather than in each:
//!
//! - **A symlinked directory is not followed.** A walk that follows them has no bound: a link
//!   pointing at an ancestor turns a small tree into an endless one, and the tool call ends as a
//!   timeout that says nothing about the cause.
//! - **The noise list is about volume, not concealment.** `.git`, `node_modules`, `target` and
//!   friends are skipped because a grep over them answers with a dependency's code; a hidden
//!   *file* such as `.env` is searched, because that is exactly the file a caller looks for.
//!
//! The glob syntax is a deliberate subset — `*`, `**`, `?` and `[...]` — because a partial
//! implementation of a larger one is a tool that silently matches nothing. Anything outside the
//! subset is matched literally, and the patterns this crate accepts are the ones its tests pin.
//! A pattern with no `/` in it is matched against the file name, recursively, which is what a
//! caller writing `*.rs` means.

use std::path::{Path, PathBuf};

use deepnest_llm::ToolDefinition;
use regex::Regex;
use serde_json::{json, Value};

use crate::args::{count_argument, optional_string_argument, string_argument};
use crate::path::resolve_target;
use crate::{Tool, ToolContext, ToolResult, MAX_RESULT_CHARS};

/// Directory names skipped by both walks.
pub const NOISE_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    ".venv",
    "__pycache__",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
];

/// How deep a walk may descend before it stops.
const MAX_DEPTH: usize = 64;

/// How many entries one walk may visit before it stops counting.
const MAX_VISITED: usize = 200_000;

/// How many matches a search shows when the caller does not say.
const DEFAULT_LIMIT: u64 = 200;

/// The bytes of a file examined when deciding whether it is text.
const BINARY_PROBE_BYTES: usize = 8_192;

fn is_noise(name: &str) -> bool {
    NOISE_DIRS.contains(&name)
}

/// Walk `base` and hand every regular file to `visit`, in name order.
///
/// `visit` returns `false` to stop the walk, which is how a caller says "I have enough".
fn walk_files(base: &Path, visit: &mut dyn FnMut(&Path) -> bool) {
    let mut visited = 0usize;
    walk_into(base, 0, &mut visited, visit);
}

fn walk_into(dir: &Path, depth: usize, visited: &mut usize, visit: &mut dyn FnMut(&Path) -> bool) {
    if depth > MAX_DEPTH || *visited > MAX_VISITED {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        // An unreadable directory is skipped rather than fatal: a search that refuses everything
        // because one subdirectory is unreadable answers a question nobody asked.
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        *visited += 1;
        if *visited > MAX_VISITED {
            return;
        }
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => continue,
        };
        // `symlink_metadata` rather than `metadata`: a link is classified as a link, so a linked
        // directory is never descended into.
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if is_noise(name) {
                continue;
            }
            walk_into(&path, depth + 1, visited, visit);
        } else if file_type.is_file() && !visit(&path) {
            return;
        }
    }
}

/// Whether a file is text enough to search.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_PROBE_BYTES).any(|byte| *byte == 0)
}

/// The path as the caller should see it: relative to the base when it is under it.
fn display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Translate the glob subset into a regex, whole-string anchored.
fn glob_regex(pattern: &str) -> Result<Regex, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("^");
    let mut index = 0usize;
    while index < chars.len() {
        let c = chars[index];
        match c {
            '*' => {
                if index + 1 < chars.len() && chars[index + 1] == '*' {
                    if index + 2 < chars.len() && chars[index + 2] == '/' {
                        // `**/` matches no directory at all as well as many, so `**/*.rs`
                        // matches `main.rs`.
                        out.push_str("(?:.*/)?");
                        index += 3;
                    } else {
                        out.push_str(".*");
                        index += 2;
                    }
                } else {
                    out.push_str("[^/]*");
                    index += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                index += 1;
            }
            '[' => {
                let mut cursor = index + 1;
                let mut class = String::from("[");
                if cursor < chars.len() && (chars[cursor] == '!' || chars[cursor] == '^') {
                    class.push('^');
                    cursor += 1;
                }
                let mut closed = false;
                while cursor < chars.len() {
                    let inner = chars[cursor];
                    if inner == ']' && class.len() > 1 {
                        closed = true;
                        break;
                    }
                    match inner {
                        '\\' => class.push_str("\\\\"),
                        '^' | '[' => {
                            class.push('\\');
                            class.push(inner);
                        }
                        _ => class.push(inner),
                    }
                    cursor += 1;
                }
                if !closed {
                    return Err(format!(
                        "{pattern:?} has an unterminated [ character class; close it with ]"
                    ));
                }
                class.push(']');
                out.push_str(&class);
                index = cursor + 1;
            }
            '\\' | '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' => {
                out.push('\\');
                out.push(c);
                index += 1;
            }
            _ => {
                out.push(c);
                index += 1;
            }
        }
    }
    out.push('$');
    Regex::new(&out).map_err(|err| format!("{pattern:?} is not a pattern this tool can use: {err}"))
}

/// The base directory of a search call, defaulting to the app directory.
fn search_base(args: &Value, ctx: &ToolContext, tool: &str) -> Result<PathBuf, ToolResult> {
    let spelled = match optional_string_argument(args, &["path", "directory"]) {
        Ok(Some(spelled)) => spelled,
        Ok(None) => return Ok(ctx.workdir.clone()),
        Err(reason) => return Err(ToolResult::refused(tool, reason)),
    };
    let base = resolve_target(&spelled, &ctx.workdir);
    match std::fs::metadata(&base) {
        Ok(metadata) if metadata.is_dir() => Ok(base),
        Ok(_) => Err(ToolResult::error(
            tool,
            format!("{} is a file, not a directory; give the pattern a path to search, or use the read tool", base.display()),
        )),
        Err(err) => Err(ToolResult::error(
            tool,
            format!("cannot search {}: {err}", base.display()),
        )),
    }
}

// ── glob ────────────────────────────────────────────────────────────────────────────────────

/// Find files by name.
pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "glob",
            "Find files by name pattern, recursively. Supports *, **, ? and [abc]. A pattern with \
             no / in it is matched against the file name at any depth, so `*.rs` finds every Rust \
             file under the given path. Results are sorted by path.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "The pattern, for example `**/*.ts` or `src/*.rs`."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search. Defaults to the app directory."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": format!("Maximum number of paths to return. Defaults to {DEFAULT_LIMIT}.")
                    }
                },
                "required": ["pattern"]
            }),
        )
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let pattern = match string_argument(args, &["pattern"]) {
            Ok(pattern) => pattern,
            Err(reason) => return ToolResult::refused("glob", reason),
        };
        let limit = match count_argument(args, &["limit", "max_results"], 1, Some(DEFAULT_LIMIT)) {
            Ok(Some(limit)) => limit,
            Ok(None) => DEFAULT_LIMIT,
            Err(reason) => return ToolResult::refused("glob", reason),
        };
        let base = match search_base(args, ctx, "glob") {
            Ok(base) => base,
            Err(refusal) => return refusal,
        };
        let by_name = !pattern.contains('/');
        let regex = match glob_regex(&pattern) {
            Ok(regex) => regex,
            Err(reason) => return ToolResult::refused("glob", reason),
        };
        let mut matches: Vec<String> = Vec::new();
        let mut truncated = false;
        walk_files(&base, &mut |path| {
            let subject = if by_name {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                display_path(path, &base)
            };
            if regex.is_match(&subject) {
                if matches.len() as u64 >= limit {
                    truncated = true;
                    return false;
                }
                matches.push(display_path(path, &base));
            }
            true
        });
        if matches.is_empty() {
            return ToolResult::ok(
                "glob",
                format!("no file under {} matches {pattern:?}", base.display()),
            );
        }
        let mut body = matches.join("\n");
        if truncated {
            body.push_str(&format!(
                "\n[showing the first {limit} matches, and there are more; narrow the pattern or raise limit]"
            ));
        }
        ToolResult::ok("glob", body).truncate(MAX_RESULT_CHARS)
    }
}

// ── grep ────────────────────────────────────────────────────────────────────────────────────

/// Search file contents.
pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "grep",
            "Search file contents with a regular expression, recursively. Each match is printed as \
             path:line:text; context lines are printed as path-line-text. Binary files and version \
             control and dependency directories are skipped. Give glob to search only some files.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "A regular expression, matched against one line at a time."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory (or file) to search. Defaults to the app directory."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Only search files whose path matches this glob pattern."
                    },
                    "context_before": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Lines to show before each match. Defaults to 0."
                    },
                    "context_after": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Lines to show after each match. Defaults to 0."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": format!("Maximum number of matches to print. Defaults to {DEFAULT_LIMIT}.")
                    }
                },
                "required": ["pattern"]
            }),
        )
    }

    fn call(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let pattern = match string_argument(args, &["pattern"]) {
            Ok(pattern) => pattern,
            Err(reason) => return ToolResult::refused("grep", reason),
        };
        let regex = match Regex::new(&pattern) {
            Ok(regex) => regex,
            Err(err) => {
                return ToolResult::refused(
                    "grep",
                    format!("pattern {pattern:?} is not a valid regular expression: {err}"),
                )
            }
        };
        let before = match count_argument(args, &["context_before", "before"], 0, Some(0)) {
            Ok(Some(count)) => count as usize,
            Ok(None) => 0,
            Err(reason) => return ToolResult::refused("grep", reason),
        };
        let after = match count_argument(args, &["context_after", "after"], 0, Some(0)) {
            Ok(Some(count)) => count as usize,
            Ok(None) => 0,
            Err(reason) => return ToolResult::refused("grep", reason),
        };
        let limit = match count_argument(args, &["limit", "max_results"], 1, Some(DEFAULT_LIMIT)) {
            Ok(Some(limit)) => limit,
            Ok(None) => DEFAULT_LIMIT,
            Err(reason) => return ToolResult::refused("grep", reason),
        };
        let filter = match optional_string_argument(args, &["glob"]) {
            Ok(Some(glob)) => match glob_regex(&glob) {
                Ok(regex) => Some((regex, !glob.contains('/'))),
                Err(reason) => return ToolResult::refused("grep", reason),
            },
            Ok(None) => None,
            Err(reason) => return ToolResult::refused("grep", reason),
        };
        let base = match search_base(args, ctx, "grep") {
            Ok(base) => base,
            Err(refusal) => return refusal,
        };
        let single_file = std::fs::metadata(&base)
            .map(|meta| meta.is_file())
            .unwrap_or(false);
        let mut out: Vec<String> = Vec::new();
        let mut shown = 0u64;
        let mut truncated = false;
        let mut files_searched = 0usize;

        let mut search_one = |path: &Path, out: &mut Vec<String>, shown: &mut u64| -> bool {
            if let Some((regex, by_name)) = &filter {
                let subject = if *by_name {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    display_path(path, &base)
                };
                if !regex.is_match(&subject) {
                    return true;
                }
            }
            let Ok(bytes) = std::fs::read(path) else {
                return true;
            };
            if looks_binary(&bytes) {
                return true;
            }
            files_searched += 1;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let lines: Vec<&str> = text.lines().collect();
            let name = display_path(path, &base);
            let mut last_end = 0usize;
            let mut pending_after = 0usize;
            for (index, line) in lines.iter().enumerate() {
                let number = index + 1;
                if !regex.is_match(line) {
                    if pending_after > 0 {
                        out.push(format!("{name}-{number}-{line}"));
                        pending_after -= 1;
                        last_end = number;
                    }
                    continue;
                }
                if *shown >= limit {
                    truncated = true;
                    return false;
                }
                let start = number.saturating_sub(before).max(1);
                if last_end + 1 < start && !out.is_empty() {
                    out.push("--".to_string());
                }
                for context in start..number {
                    if context > last_end {
                        out.push(format!("{name}-{context}-{}", lines[context - 1]));
                    }
                }
                out.push(format!("{name}:{number}:{line}"));
                *shown += 1;
                last_end = number;
                pending_after = after;
            }
            true
        };

        if single_file {
            search_one(&base, &mut out, &mut shown);
        } else {
            walk_files(&base, &mut |path| search_one(path, &mut out, &mut shown));
        }
        if out.is_empty() {
            return ToolResult::ok(
                "grep",
                format!(
                    "no match for {pattern:?} under {} ({files_searched} file(s) searched)",
                    base.display()
                ),
            );
        }
        let mut body = out.join("\n");
        if truncated {
            body.push_str(&format!(
                "\n[stopped at {limit} match(es); narrow the pattern or raise limit]"
            ));
        }
        ToolResult::ok("grep", body).truncate(MAX_RESULT_CHARS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_sandbox::{Mode, Policy};
    use std::fs;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "deepnest-tools-search-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(
                Policy::from_roots(
                    Mode::WorkspaceWrite,
                    self.base.clone(),
                    self.base.clone(),
                    self.base.clone(),
                ),
                self.base.clone(),
            )
        }

        fn file(&self, name: &str, content: &str) {
            let path = self.base.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_bare_pattern_matches_the_file_name_at_any_depth() {
        let scratch = Scratch::new("glob-bare");
        scratch.file("main.rs", "");
        scratch.file("src/lib.rs", "");
        scratch.file("src/nested/deep.rs", "");
        scratch.file("README.md", "");
        let result = GlobTool.call(&json!({"pattern": "*.rs"}), &scratch.ctx());
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content.lines().count(), 3, "{}", result.content);
        assert!(
            result.content.contains("src/nested/deep.rs"),
            "{}",
            result.content
        );
        assert!(!result.content.contains("README.md"), "{}", result.content);
    }

    #[test]
    fn a_pattern_with_a_slash_is_matched_against_the_path() {
        let scratch = Scratch::new("glob-path");
        scratch.file("src/main.rs", "");
        scratch.file("tests/main.rs", "");
        let result = GlobTool.call(&json!({"pattern": "src/*.rs"}), &scratch.ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("src/main.rs"), "{}", result.content);
        assert!(!result.content.contains("tests/"), "{}", result.content);
    }

    #[test]
    fn a_double_star_matches_no_directory_at_all_as_well_as_many() {
        let regex = glob_regex("**/*.rs").unwrap();
        assert!(regex.is_match("main.rs"), "a file at the root matches");
        assert!(regex.is_match("a/b/c.rs"), "so does a deeply nested one");
        let class = glob_regex("src/[ab].rs").unwrap();
        assert!(class.is_match("src/a.rs"));
        assert!(!class.is_match("src/c.rs"));
    }

    #[test]
    fn the_noise_directories_are_skipped() {
        let scratch = Scratch::new("glob-noise");
        scratch.file("src/main.rs", "");
        scratch.file("node_modules/pkg/index.rs", "");
        scratch.file("target/debug/build.rs", "");
        let result = GlobTool.call(&json!({"pattern": "*.rs"}), &scratch.ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("src/main.rs"), "{}", result.content);
        assert!(
            !result.content.contains("node_modules"),
            "{}",
            result.content
        );
        assert!(!result.content.contains("target/"), "{}", result.content);
    }

    #[test]
    fn a_glob_that_matches_nothing_says_so_with_the_base_it_searched() {
        let scratch = Scratch::new("glob-none");
        scratch.file("a.txt", "");
        let result = GlobTool.call(&json!({"pattern": "*.rs"}), &scratch.ctx());
        assert!(!result.is_error);
        assert!(
            result.content.contains("no file under"),
            "{}",
            result.content
        );
    }

    #[test]
    fn grep_prints_matches_as_path_line_text() {
        let scratch = Scratch::new("grep-match");
        scratch.file("src/a.rs", "fn one() {}\nfn two() {}\n");
        let result = GrepTool.call(&json!({"pattern": "fn two"}), &scratch.ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("src/a.rs:2:fn two() {}"),
            "{}",
            result.content
        );
    }

    #[test]
    fn grep_context_lines_are_marked_differently_from_matches() {
        let scratch = Scratch::new("grep-context");
        scratch.file("a.txt", "alpha\nbeta\ngamma\n");
        let result = GrepTool.call(
            &json!({"pattern": "beta", "context_before": 1, "context_after": 1}),
            &scratch.ctx(),
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("a.txt-1-alpha"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("a.txt:2:beta"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("a.txt-3-gamma"),
            "{}",
            result.content
        );
    }

    #[test]
    fn grep_skips_binary_files_and_hidden_noise() {
        let scratch = Scratch::new("grep-binary");
        fs::write(scratch.base.join("blob.bin"), [0u8, 1, 2]).unwrap();
        scratch.file(".git/config", "needle\n");
        scratch.file("real.txt", "needle\n");
        let result = GrepTool.call(&json!({"pattern": "needle"}), &scratch.ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("real.txt"), "{}", result.content);
        assert!(!result.content.contains("blob.bin"), "{}", result.content);
        assert!(!result.content.contains(".git"), "{}", result.content);
    }

    #[test]
    fn a_hidden_file_is_searched_because_it_is_what_a_caller_looks_for() {
        let scratch = Scratch::new("grep-hidden");
        scratch.file(".env", "API_KEY=needle\n");
        let result = GrepTool.call(&json!({"pattern": "needle"}), &scratch.ctx());
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains(".env"), "{}", result.content);
    }

    #[test]
    fn the_glob_filter_narrows_which_files_are_read() {
        let scratch = Scratch::new("grep-filter");
        scratch.file("a.rs", "needle\n");
        scratch.file("b.md", "needle\n");
        let result = GrepTool.call(
            &json!({"pattern": "needle", "glob": "*.rs"}),
            &scratch.ctx(),
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("a.rs"), "{}", result.content);
        assert!(!result.content.contains("b.md"), "{}", result.content);
    }

    #[test]
    fn an_invalid_regex_is_refused_with_the_compiler_s_words() {
        let scratch = Scratch::new("grep-bad-regex");
        let result = GrepTool.call(&json!({"pattern": "(unclosed"}), &scratch.ctx());
        assert!(result.is_error);
        assert!(
            result.content.contains("not a valid regular expression"),
            "{}",
            result.content
        );
    }

    #[test]
    fn a_negative_context_is_refused_rather_than_showing_no_line() {
        let scratch = Scratch::new("grep-negative");
        scratch.file("a.txt", "x\n");
        let result = GrepTool.call(
            &json!({"pattern": "x", "context_before": -2}),
            &scratch.ctx(),
        );
        assert!(result.is_error);
        assert!(
            result.content.contains("context_before"),
            "{}",
            result.content
        );
    }

    #[test]
    fn the_limit_stops_the_search_and_says_it_did() {
        let scratch = Scratch::new("grep-limit");
        scratch.file("a.txt", "needle\nneedle\nneedle\n");
        let result = GrepTool.call(&json!({"pattern": "needle", "limit": 2}), &scratch.ctx());
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            result.content.matches("needle").count(),
            2,
            "{}",
            result.content
        );
        assert!(
            result.content.contains("stopped at 2 match(es)"),
            "{}",
            result.content
        );
    }

    #[test]
    fn no_match_says_how_many_files_were_read() {
        let scratch = Scratch::new("grep-none");
        scratch.file("a.txt", "nothing here\n");
        let result = GrepTool.call(&json!({"pattern": "needle"}), &scratch.ctx());
        assert!(!result.is_error);
        assert!(
            result.content.contains("1 file(s) searched"),
            "{}",
            result.content
        );
    }
}
