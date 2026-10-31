//! The control token: the one secret that stands between a random web page and this API.
//!
//! Loopback is not a security boundary. Any page the user's browser loads can send a request to
//! `127.0.0.1:4270` — that is what makes CSRF and DNS rebinding real for a local server, and it
//! is why the design pairs a token with `Host` validation instead of trusting the address. A
//! page can reach the port; what it cannot do is read a `0600` file in the user's home.
//!
//! Three properties are the point:
//!
//! - **Unpredictable**: 32 bytes from the OS CSPRNG, not a counter and not a timestamp.
//! - **Replaced atomically**: written to a sibling temp file and renamed, so a reader never
//!   sees a half-written token and locks the panel out.
//! - **Self-healing**: [`reassert`] puts the *running* token back after an external deletion.
//!   Generating a new one there would be worse than doing nothing — the panel holds the old one
//!   in memory and would be refused by its own server until the next restart.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The file name inside the deepnest home directory.
pub const FILE_NAME: &str = "deepnest.token";

/// How much entropy a token carries before hex encoding.
pub const BYTES: usize = 32;

/// The length of a token's textual form.
const HEX_LEN: usize = BYTES * 2;

/// A control token, in its hex form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token(String);

/// Why a token could not be produced or stored.
#[derive(Debug)]
pub enum TokenError {
    /// The operating system's random source failed.
    Entropy,
    /// The token could not be written where it belongs.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Entropy => write!(f, "the operating system's random source failed"),
            Self::Io { path, source } => write!(f, "cannot write {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for TokenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Entropy => None,
            Self::Io { source, .. } => Some(source),
        }
    }
}

impl Token {
    /// A fresh token from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, TokenError> {
        let mut raw = [0u8; BYTES];
        getrandom::getrandom(&mut raw).map_err(|_| TokenError::Entropy)?;
        Ok(Self(to_hex(&raw)))
    }

    /// Accept a token read back from its file, or return `None` if it is not one.
    ///
    /// Lowercased on the way in so a hand-edited file with uppercase hex still matches.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let usable = text.len() == HEX_LEN && text.bytes().all(|byte| byte.is_ascii_hexdigit());
        usable.then(|| Self(text.to_ascii_lowercase()))
    }

    /// The token as it appears in an `Authorization: Bearer …` header.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Does `candidate` carry this token?
    ///
    /// Compared without an early return on the first differing byte, so the time it takes says
    /// nothing about how much of the token was guessed correctly. The length check does return
    /// early — a token's length is published in this file, not guessed.
    pub fn matches(&self, candidate: &str) -> bool {
        let candidate = candidate.trim();
        if candidate.len() != self.0.len() {
            return false;
        }
        let mut difference = 0u8;
        for (mine, theirs) in self.0.bytes().zip(candidate.bytes()) {
            difference |= mine ^ theirs;
        }
        difference == 0
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Read the token at `path`, minting and storing one when the file is absent or unusable.
pub fn ensure(path: &Path) -> Result<Token, TokenError> {
    if let Some(token) = read(path) {
        return Ok(token);
    }
    let token = Token::generate()?;
    persist(path, &token)?;
    Ok(token)
}

/// Put `token` back at `path` if it is missing, unreadable or different.
///
/// Runs on a keepalive so an external deletion heals. Best effort by design: a tick that cannot
/// write is not a reason to fall over, because the next tick tries again with the same token.
pub fn reassert(path: &Path, token: &Token) -> Result<(), TokenError> {
    if read(path).as_ref() == Some(token) {
        return Ok(());
    }
    persist(path, token)
}

/// Write `token` to `path`, mode `0600`, atomically.
pub fn persist(path: &Path, token: &Token) -> Result<(), TokenError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }

    // A sibling in the same directory, so the rename below stays within one filesystem and is
    // therefore atomic.
    let staging = path.with_extension("token.tmp");
    let _ = fs::remove_file(&staging);

    {
        let mut file = open_private(&staging).map_err(|source| io_error(&staging, source))?;
        file.write_all(token.as_str().as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| io_error(&staging, source))?;
    }

    if fs::rename(&staging, path).is_err() {
        // Windows refuses to rename onto an existing file. The target is unusable at this point
        // (that is why we are writing), so removing it costs nothing that a valid token would
        // have protected.
        fs::remove_file(path).map_err(|source| io_error(path, source))?;
        fs::rename(&staging, path).map_err(|source| io_error(path, source))?;
    }
    Ok(())
}

fn io_error(path: &Path, source: io::Error) -> TokenError {
    TokenError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn read(path: &Path) -> Option<Token> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| Token::parse(&text))
}

/// Create the file, refusing to reuse an existing one, with the narrowest mode `unix` offers.
fn open_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // `mode` applies only at creation, and `create_new` guarantees this is creation — so the
        // token is never briefly world-readable, which a write-then-chmod would allow.
        options.mode(0o600);
    }
    options.open(path)
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-token-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn path(&self) -> PathBuf {
            self.base.join(FILE_NAME)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_token_is_32_bytes_of_hex() {
        let token = Token::generate().unwrap();
        assert_eq!(token.as_str().len(), HEX_LEN);
        assert!(token.as_str().bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn two_tokens_differ() {
        // A counter or a timestamp would pass every other test in this module.
        let first = Token::generate().unwrap();
        let second = Token::generate().unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn the_file_is_written_0600_and_round_trips() {
        let scratch = Scratch::new("roundtrip");
        let path = scratch.path();

        let written = ensure(&path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "loopback is not a boundary; the mode is");
        }

        // A second call reuses the file instead of minting a new token, which is what keeps a
        // restarted panel able to read the token a running client already holds.
        assert_eq!(ensure(&path).unwrap(), written);
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), written.as_str());
    }

    #[test]
    fn an_unusable_file_is_replaced() {
        let scratch = Scratch::new("garbage");
        let path = scratch.path();
        fs::write(&path, "not a token\n").unwrap();

        let token = ensure(&path).unwrap();
        assert_eq!(token.as_str().len(), HEX_LEN);
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), token.as_str());
    }

    #[test]
    fn reassert_restores_the_running_token_not_a_new_one() {
        let scratch = Scratch::new("reassert");
        let path = scratch.path();
        let token = ensure(&path).unwrap();

        fs::remove_file(&path).unwrap();
        reassert(&path, &token).unwrap();

        // The same token, because the panel in memory holds this one. A fresh token here would
        // refuse the panel it was minted for until the next restart.
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), token.as_str());
    }

    #[test]
    fn reassert_leaves_a_matching_file_alone() {
        let scratch = Scratch::new("reassert-noop");
        let path = scratch.path();
        let token = ensure(&path).unwrap();

        reassert(&path, &token).unwrap();
        assert_eq!(read(&path), Some(token));
    }

    #[test]
    fn comparison_rejects_a_prefix_and_a_different_token() {
        let token = Token::parse(&"a".repeat(HEX_LEN)).unwrap();
        assert!(token.matches(&"a".repeat(HEX_LEN)));
        assert!(token.matches(&format!("  {}\n", "a".repeat(HEX_LEN))));
        assert!(!token.matches(&"a".repeat(HEX_LEN - 1)));
        assert!(!token.matches(&"b".repeat(HEX_LEN)));
    }

    #[test]
    fn a_token_read_back_in_uppercase_still_matches() {
        let text = "A".repeat(HEX_LEN);
        let token = Token::parse(&text).unwrap();
        assert!(token.matches(&"a".repeat(HEX_LEN)));
    }

    #[test]
    fn a_short_or_non_hex_file_is_not_a_token() {
        assert!(Token::parse("").is_none());
        assert!(Token::parse("abc").is_none());
        assert!(Token::parse(&"z".repeat(HEX_LEN)).is_none());
    }
}
