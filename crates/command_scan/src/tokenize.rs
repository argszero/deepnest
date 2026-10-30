//! Reading a command's text into the words a rule can judge.
//!
//! A tokenizer here rather than at each call site, because the two rules that consume it
//! ([`crate::refuse`]) must agree about what the command *says*, and two tokenizers drift into
//! two spellings of one command — the failure that lets one rule see `git checkout .` and the
//! other see three words.
//!
//! Four properties are deliberate, each one measured as a hole somewhere before it was a rule
//! here (`ref: emrg/tools/command_scan.py`, whose docstring records the measurements):
//!
//! - **A newline is a token, not whitespace.** A shell runs both lines; a tokenizer that eats
//!   the newline glues them into one stream, and the second line's command word stops being in
//!   command position. That is how `echo ok` + newline + a destructive command reads as data.
//! - **The backtick is punctuation.** Without it a substitution stays glued to its words
//!   (`` `cmd` `` arrives as `` `cmd `` and `` ` ``), so the program word never matches.
//! - **Quoting is respected.** A command named inside a string literal is one argument: the
//!   agent may talk about a command without running it, and a scanner that refuses the
//!   conversation refuses the work.
//! - **Unparseable input is split on whitespace rather than dropped.** A guard that fails open
//!   on odd input is not a guard; this one over-blocks in the loud direction and says so.

/// The words a shell hands to `execve`, with operators kept as their own tokens.
///
/// Quotes are resolved (their content stays one token), inside-quote newlines are content, and
/// a separator is always its own token so a consumer can find command positions.
pub fn tokenize(command: &str) -> Vec<String> {
    let mut lexer = Lexer::default();
    let chars: Vec<char> = command.chars().collect();
    let mut index = 0;

    while index < chars.len() {
        match chars[index] {
            ' ' | '\t' | '\r' => {
                lexer.flush();
                index += 1;
            }
            '\n' => {
                lexer.operator("\n");
                index += 1;
            }
            '\'' => {
                index += 1;
                while index < chars.len() && chars[index] != '\'' {
                    lexer.push(chars[index]);
                    index += 1;
                }
                index += 1; // the closing quote, or one past the end for an unterminated one
            }
            '"' => {
                index += 1;
                while index < chars.len() && chars[index] != '"' {
                    if chars[index] == '\\' && index + 1 < chars.len() {
                        // Inside double quotes a backslash escapes only these; anything else
                        // keeps both characters, which is what the shell does.
                        let next = chars[index + 1];
                        if matches!(next, '"' | '\\' | '$' | '`') {
                            lexer.push(next);
                            index += 2;
                            continue;
                        }
                    }
                    lexer.push(chars[index]);
                    index += 1;
                }
                index += 1;
            }
            '\\' => {
                if index + 1 < chars.len() {
                    lexer.push(chars[index + 1]);
                    index += 2;
                } else {
                    lexer.push('\\');
                    index += 1;
                }
            }
            '&' => {
                if chars.get(index + 1) == Some(&'&') {
                    lexer.operator("&&");
                    index += 2;
                } else {
                    lexer.operator("&");
                    index += 1;
                }
            }
            '|' => {
                if chars.get(index + 1) == Some(&'|') {
                    lexer.operator("||");
                    index += 2;
                } else {
                    lexer.operator("|");
                    index += 1;
                }
            }
            ';' | '(' | ')' | '`' => {
                let op = chars[index].to_string();
                lexer.operator(&op);
                index += 1;
            }
            other => {
                lexer.push(other);
                index += 1;
            }
        }
    }
    lexer.flush();
    lexer.tokens
}

/// The token being accumulated. A tiny state machine rather than a set of flags in the caller:
/// "is there a token in progress" is the one thing that must not be forgotten, and forgetting it
/// is how an empty argument appears or a word is lost.
#[derive(Default)]
struct Lexer {
    tokens: Vec<String>,
    current: String,
    open: bool,
}

impl Lexer {
    /// Add one character, starting a token if there is not one in progress.
    fn push(&mut self, ch: char) {
        self.open = true;
        self.current.push(ch);
    }

    /// End the token in progress, if any.
    fn flush(&mut self) {
        if self.open {
            self.tokens.push(std::mem::take(&mut self.current));
            self.open = false;
        }
    }

    /// Emit an operator as its own token.
    fn operator(&mut self, op: &str) {
        self.flush();
        self.tokens.push(op.to_string());
    }
}

/// The tokens that separate one command from the next.
pub fn is_separator(token: &str) -> bool {
    matches!(
        token,
        "&&" | "||" | ";" | "|" | "&" | "\n" | "(" | "{" | "!" | ")" | "`"
    )
}

/// The words after which the next word is a command rather than an argument.
///
/// `builtin` and `command` prefix exactly one word; `eval`, `-exec` and the exec wrappers hand
/// theirs to `execve`. The set is an over-approximation on purpose: `echo builtin cd x` reads
/// `cd` as an invocation too, which is a refusal in the loud direction rather than a gap.
const COMMAND_PREFIXES: &[&str] = &[
    "eval", "-exec", "-execdir", "builtin", "command", "exec", "env", "sudo", "doas", "nohup",
    "time", "timeout", "nice", "setsid", "stdbuf", "unshare", "nsenter", "chroot", "busybox",
    "xargs", "watch", "ionice", "chrt",
];

/// Shell keywords after which the next word is a command.
const KEYWORD_POSITIONS: &[&str] = &["if", "then", "elif", "else", "while", "until", "do"];

/// The shells that re-parse an argument as a command, and the flag that does it.
const SHELLS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "ash",
    "pwsh",
    "powershell",
];

/// Whether the token at `index` sits where a shell would look for a command.
///
/// The walk is what keeps `grep -rn kill .` from reading as a signaller: a word that is another
/// program's operand is data. It errs toward "yes" — a command word preceded by anything
/// unrecognised is treated as an invocation — because the cost is a loud refusal, while the
/// other direction is a command that runs unseen.
pub fn is_command_position(tokens: &[String], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = tokens[index - 1].as_str();
    if is_separator(previous) {
        return true;
    }
    let previous_base = basename(previous);
    if COMMAND_PREFIXES.contains(&previous_base) || KEYWORD_POSITIONS.contains(&previous_base) {
        return true;
    }
    // `FOO=1 git status` — an assignment is a prefix, not a command.
    if is_assignment(previous) {
        return true;
    }
    // A flag's *value* is data (`grep -rn kill .`), so the word right after a flag is not a
    // command either. But a flag standing between a prefix and the command hides it
    // (`sudo -u root cmd`), so a value/flag pair is stepped over rather than ending the walk.
    if previous.starts_with('-') {
        return false;
    }
    if index >= 2 && tokens[index - 2].starts_with('-') && !is_separator(&tokens[index - 2]) {
        let mut cursor = index - 2;
        loop {
            if cursor == 0 {
                return true;
            }
            let candidate = tokens[cursor - 1].as_str();
            if is_separator(candidate) || is_assignment(candidate) {
                return true;
            }
            let candidate_base = basename(candidate);
            if COMMAND_PREFIXES.contains(&candidate_base)
                || KEYWORD_POSITIONS.contains(&candidate_base)
            {
                return true;
            }
            // Another flag/value pair: step over it as well.
            if cursor >= 2
                && tokens[cursor - 2].starts_with('-')
                && !is_separator(&tokens[cursor - 2])
            {
                cursor -= 2;
                continue;
            }
            return false;
        }
    }
    false
}

/// The program word of a token: its last path component, without a Windows `.exe`.
pub fn basename(token: &str) -> &str {
    let trimmed = token.trim_start_matches('{').trim_start();
    let last = trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(trimmed);
    last.strip_suffix(".exe").unwrap_or(last)
}

fn is_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Every text a shell would re-parse as a command, taken from the command's own text.
///
/// Three spellings, and the reason each is here rather than left to the signaller rule: a
/// payload behind `sh -c` is a string literal to the tokenizer, `eval` joins its arguments and
/// runs the result, and `$( … )` / backticks hand a span to a subshell. `env -S "<string>"` is
/// included because splitting and execing that string is `env`'s documented job.
pub fn nested_texts(command: &str) -> Vec<String> {
    let mut texts = Vec::new();
    let tokens = tokenize(command);

    // `sh -c '<payload>'` and `eval <payload>`
    for (index, token) in tokens.iter().enumerate() {
        let base = basename(token).to_lowercase();
        if SHELLS.contains(&base.as_str()) {
            if let Some(payload) = payload_after_shell_flag(&tokens, index) {
                texts.push(payload);
            }
        } else if base == "eval" {
            texts.push(collect_until_separator(&tokens, index + 1));
        } else if base == "env" {
            if let Some(payload) = env_split_string(&tokens, index) {
                texts.push(payload);
            }
        }
    }

    texts.extend(substitutions(command));
    texts.retain(|text| !text.trim().is_empty());
    texts
}

/// The argument a shell's `-c` (or a `-ec`-style cluster containing `c`) introduces.
fn payload_after_shell_flag(tokens: &[String], shell_index: usize) -> Option<String> {
    let mut index = shell_index + 1;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if is_separator(token) {
            return None;
        }
        if token.starts_with('-') && token.len() > 1 {
            // `-c` and clusters such as `-ec`. `-o` and friends take the next word as a value,
            // so a cluster that does not contain `c` ends the search for a payload here.
            if token.trim_start_matches('-').contains('c') {
                return tokens.get(index + 1).cloned();
            }
        } else {
            return None;
        }
        index += 1;
    }
    None
}

/// The string `env -S` splits into argv, if this `env` carries one.
fn env_split_string(tokens: &[String], env_index: usize) -> Option<String> {
    let mut index = env_index + 1;
    while index < tokens.len() && !is_separator(&tokens[index]) {
        let token = tokens[index].as_str();
        if token == "-S" || token == "--split-string" {
            return tokens.get(index + 1).cloned();
        }
        if let Some(rest) = token.strip_prefix("-S") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
        if !token.starts_with('-') {
            return None;
        }
        index += 1;
    }
    None
}

fn collect_until_separator(tokens: &[String], start: usize) -> String {
    let mut parts = Vec::new();
    for token in tokens.iter().skip(start) {
        if is_separator(token) {
            break;
        }
        parts.push(token.as_str());
    }
    parts.join(" ")
}

/// `$( … )` and backtick spans, with their delimiters removed.
///
/// A character scan rather than a token walk: the span's own bytes are what a subshell reads,
/// and re-joining tokens would lose the separators inside it.
fn substitutions(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut texts = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        match (chars[index], chars.get(index + 1)) {
            ('$', Some('(')) => {
                if let Some((inner, next)) = span(&chars, index + 2, '(', ')') {
                    texts.push(inner);
                    index = next;
                    continue;
                }
            }
            ('`', _) => {
                if let Some((inner, next)) = span(&chars, index + 1, '`', '`') {
                    texts.push(inner);
                    index = next;
                    continue;
                }
            }
            _ => {}
        }
        index += 1;
    }
    texts
}

/// The text between `open`/`close` at `start`, nesting `open` inside, and the index after.
fn span(chars: &[char], start: usize, open: char, close: char) -> Option<(String, usize)> {
    let mut depth = 1usize;
    let mut index = start;
    let mut collected = String::new();
    while index < chars.len() {
        let ch = chars[index];
        if open != close && ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some((collected, index + 1));
            }
        }
        collected.push(ch);
        index += 1;
    }
    // An unterminated substitution is still a span the shell will try to run; report what came.
    if collected.is_empty() {
        None
    } else {
        Some((collected, index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_keeps_one_argument_one_token() {
        assert_eq!(
            tokenize(r#"git commit -m "fix: the 'thing' broke""#),
            ["git", "commit", "-m", "fix: the 'thing' broke"]
        );
        assert_eq!(tokenize(r#"echo 'a b' c"#), ["echo", "a b", "c"]);
    }

    #[test]
    fn operators_are_their_own_tokens() {
        assert_eq!(
            tokenize("a && b || c; d | e & f"),
            ["a", "&&", "b", "||", "c", ";", "d", "|", "e", "&", "f"]
        );
    }

    #[test]
    fn a_newline_separates_two_commands() {
        // The measured hole: without this, the second line's command word is an "argument".
        let tokens = tokenize("echo ok\ngit checkout .");
        assert_eq!(tokens, ["echo", "ok", "\n", "git", "checkout", "."]);
        assert!(is_command_position(&tokens, 3), "{tokens:?}");
    }

    #[test]
    fn a_backtick_substitution_is_broken_out() {
        let tokens = tokenize("echo `cmd arg`");
        assert!(tokens.contains(&"`".to_string()), "{tokens:?}");
    }

    #[test]
    fn an_unterminated_quote_does_not_drop_the_command() {
        let tokens = tokenize("echo 'unterminated --flag");
        assert_eq!(tokens[0], "echo");
        assert!(tokens.len() >= 2, "{tokens:?}");
    }

    #[test]
    fn basename_handles_paths_braces_and_the_windows_suffix() {
        assert_eq!(basename("/usr/bin/git"), "git");
        assert_eq!(basename(r"C:\Program Files\Git\git.exe"), "git");
        assert_eq!(basename("{ pkill"), "pkill");
        assert_eq!(basename("./deepnest"), "deepnest");
    }

    #[test]
    fn command_position_follows_separators_prefixes_and_assignments() {
        let tokens = tokenize("sleep 1; pkill -f deepnest");
        let at = tokens.iter().position(|t| t == "pkill").unwrap();
        assert!(is_command_position(&tokens, at));

        let tokens = tokenize("grep -rn kill deepnest/");
        let at = tokens.iter().position(|t| t == "kill").unwrap();
        assert!(
            !is_command_position(&tokens, at),
            "an operand is not an invocation"
        );

        let tokens = tokenize("FOO=1 sudo -u root cmd");
        assert!(is_command_position(&tokens, 2), "sudo is a prefix");
        let at = tokens.iter().position(|t| t == "cmd").unwrap();
        assert!(
            is_command_position(&tokens, at),
            "a flag's value is skipped"
        );
    }

    #[test]
    fn nested_texts_find_the_shell_payloads() {
        assert_eq!(
            nested_texts("sh -c 'deepnest server stop'"),
            ["deepnest server stop"]
        );
        assert_eq!(nested_texts("bash -ec 'rm -rf /'"), ["rm -rf /"]);
        assert_eq!(
            nested_texts("eval 'deepnest restart'"),
            ["deepnest restart"]
        );
        assert_eq!(nested_texts("env -S 'deepnest stop'"), ["deepnest stop"]);
        assert_eq!(
            nested_texts("echo $(deepnest restart)"),
            ["deepnest restart"]
        );
        assert_eq!(nested_texts("echo `deepnest stop`"), ["deepnest stop"]);
        assert!(nested_texts("echo hello").is_empty());
    }

    #[test]
    fn a_quoted_argument_of_a_program_that_cannot_run_it_is_not_nested() {
        // `echo "deepnest server stop"` prints the words; it does not run them.
        assert!(nested_texts("echo \"deepnest server stop\"").is_empty());
    }

    #[test]
    fn an_unterminated_substitution_still_yields_its_text() {
        assert_eq!(substitutions("echo $(deepnest stop"), ["deepnest stop"]);
    }
}
