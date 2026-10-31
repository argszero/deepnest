//! `~/.deepnest/config.toml` — the whole of deepnest's configuration.
//!
//! deepnest ships no model of its own. The agent asks an OpenAI-compatible endpoint the user
//! names, with the user's key, and those three values are the only things no default can
//! invent. So this file is small, and its two failure modes are not the same thing:
//!
//! - **The file is absent → first run.** That is not an error. The binary starts, the panel
//!   reports the endpoint as unconfigured, and `crates/web` collects it. Refusing to start
//!   would mean the first thing a new user sees is a message telling them to hand-write TOML.
//! - **The file exists and does not parse → an error, and startup stops.** Starting with a
//!   base URL the binary only half understood converts one clear failure at startup into a
//!   confusing one at the first request, which is far from the typo that caused it.
//!
//! Unknown keys are rejected rather than ignored for the same reason: `api_keyy = "sk-…"` is a
//! typo, and silently reading "no api_key" from a file that plainly contains one is exactly the
//! diagnosis that is hard to make.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file name inside the deepnest home directory.
pub const FILE_NAME: &str = "config.toml";

/// A parsed `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub llm: Llm,
}

/// The model endpoint the user supplies.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Llm {
    /// The endpoint's base URL, e.g. `https://api.deepseek.com`.
    pub base_url: String,
    /// The endpoint's key. `key` is accepted as a spelling too: the design names the field
    /// `key`, EMRG's config calls the same thing `api_key`, and a user migrating a config
    /// should not have to discover the difference from a parse error.
    #[serde(alias = "key")]
    pub api_key: String,
    /// The model name to ask for.
    pub model: String,
}

/// Why a config could not be read or accepted.
#[derive(Debug)]
pub enum ConfigError {
    /// The file exists but could not be read.
    Unreadable { path: PathBuf, source: io::Error },
    /// The file was read but is not valid TOML, or does not match the shape above.
    Unparseable { path: PathBuf, detail: String },
    /// The file parsed, but a value cannot be used.
    Unusable {
        path: PathBuf,
        field: &'static str,
        reason: &'static str,
    },
    /// The config could not be written — a read-only home, a full disk.
    Unwritable { path: PathBuf, source: io::Error },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            Self::Unparseable { path, detail } => {
                write!(f, "{} is not a usable config: {detail}", path.display())
            }
            Self::Unusable {
                path,
                field,
                reason,
            } => write!(f, "{}: `{field}` {reason}", path.display()),
            Self::Unwritable { path, source } => {
                write!(f, "cannot write {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } | Self::Unwritable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Write `llm` to `path`, mode `0600`, atomically.
///
/// The same three checks [`load`] applies run *before* anything is written, so a form that submits
/// `api.deepseek.com` without a scheme is refused now, by the field name, rather than at the next
/// start — a config the binary would refuse to boot on is worse than a form that says no.
///
/// Written to a temporary file beside it and renamed: a half-written `config.toml` would be a file
/// that does not parse, and [`load`] treats that as a startup failure. `0600` because the file
/// holds the user's API key, and it is applied at creation so the key is never briefly readable.
pub fn save(path: &Path, llm: &Llm) -> Result<(), ConfigError> {
    let config = Config { llm: llm.clone() };
    config.check(path)?;

    let text = toml::to_string(&config).map_err(|error| ConfigError::Unparseable {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;

    let temporary = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    let unwritable = |source: io::Error| ConfigError::Unwritable {
        path: path.to_path_buf(),
        source,
    };

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(unwritable)?;
    use io::Write as _;
    file.write_all(text.as_bytes()).map_err(unwritable)?;
    // Before the rename, so the bytes are on disk under the name that matters rather than at the
    // rename's mercy.
    file.sync_all().map_err(unwritable)?;
    drop(file);

    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        unwritable(error)
    })
}

/// Read the config at `path`.
///
/// `Ok(None)` is first run — there is no file yet, and that is a state the binary serves rather
/// than one it refuses.
pub fn load(path: &Path) -> Result<Option<Config>, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Unreadable {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    let config: Config = toml::from_str(&text).map_err(|error| ConfigError::Unparseable {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    config.check(path)?;
    Ok(Some(config))
}

impl Config {
    /// The value-level checks TOML cannot express.
    fn check(&self, path: &Path) -> Result<(), ConfigError> {
        let unusable = |field: &'static str, reason: &'static str| ConfigError::Unusable {
            path: path.to_path_buf(),
            field,
            reason,
        };

        if self.llm.base_url.trim().is_empty() {
            return Err(unusable(
                "llm.base_url",
                "is empty — name the endpoint, e.g. https://api.deepseek.com",
            ));
        }
        if !has_http_scheme(&self.llm.base_url) {
            // Anything else is not an OpenAI-compatible HTTP endpoint, and a request built on
            // it fails one layer down with a worse message.
            return Err(unusable(
                "llm.base_url",
                "must start with http:// or https://",
            ));
        }
        if self.llm.api_key.trim().is_empty() {
            return Err(unusable(
                "llm.api_key",
                "is empty — put the key of your model endpoint there",
            ));
        }
        if self.llm.model.trim().is_empty() {
            return Err(unusable(
                "llm.model",
                "is empty — name the model to ask, e.g. deepseek-flash",
            ));
        }
        Ok(())
    }

    /// The model name, for a page that reports what is configured.
    pub fn model(&self) -> &str {
        self.llm.model.trim()
    }
}

fn has_http_scheme(value: &str) -> bool {
    let value = value.trim();
    value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || value
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
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
                std::env::temp_dir().join(format!("deepnest-config-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn write(&self, text: &str) -> PathBuf {
            let path = self.base.join(FILE_NAME);
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    const GOOD: &str = r#"
[llm]
base_url = "https://api.deepseek.com"
api_key = "sk-test"
model = "deepseek-flash"
"#;

    #[test]
    fn a_complete_config_loads() {
        let scratch = Scratch::new("good");
        let path = scratch.write(GOOD);

        let config = load(&path).unwrap().unwrap();
        assert_eq!(config.llm.base_url, "https://api.deepseek.com");
        assert_eq!(config.llm.api_key, "sk-test");
        assert_eq!(config.model(), "deepseek-flash");
    }

    #[test]
    fn a_missing_file_is_first_run_not_a_failure() {
        let scratch = Scratch::new("absent");
        assert!(load(&scratch.base.join(FILE_NAME)).unwrap().is_none());
    }

    #[test]
    fn the_designs_spelling_of_the_key_is_accepted() {
        // design.md says `key`; EMRG's config says `api_key`. Both name the same thing, and a
        // user must not have to find that out from a parse error.
        let scratch = Scratch::new("key-alias");
        let path = scratch.write(
            r#"
[llm]
base_url = "https://api.deepseek.com"
key = "sk-alias"
model = "deepseek-flash"
"#,
        );

        assert_eq!(load(&path).unwrap().unwrap().llm.api_key, "sk-alias");
    }

    #[test]
    fn a_typo_in_a_key_name_is_reported_rather_than_read_as_absence() {
        let scratch = Scratch::new("typo");
        let path = scratch.write(
            r#"
[llm]
base_url = "https://api.deepseek.com"
api_keyy = "sk-test"
model = "deepseek-flash"
"#,
        );

        let error = load(&path).unwrap_err().to_string();
        assert!(
            error.contains("api_keyy"),
            "the message must name the field that is wrong: {error}"
        );
        assert!(error.contains(&path.display().to_string()));
    }

    #[test]
    fn an_empty_value_names_the_field_and_the_file() {
        let scratch = Scratch::new("empty");
        let path = scratch.write(
            r#"
[llm]
base_url = "https://api.deepseek.com"
api_key = "   "
model = "deepseek-flash"
"#,
        );

        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains("llm.api_key"), "{error}");
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[test]
    fn a_base_url_that_is_not_http_is_refused() {
        let scratch = Scratch::new("scheme");
        let path = scratch.write(
            r#"
[llm]
base_url = "api.deepseek.com"
api_key = "sk-test"
model = "deepseek-flash"
"#,
        );

        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains("llm.base_url"), "{error}");
        assert!(error.contains("http://"), "{error}");
    }

    #[test]
    fn a_directory_in_place_of_the_file_is_reported_as_unreadable() {
        let scratch = Scratch::new("directory");
        let path = scratch.base.join(FILE_NAME);
        fs::create_dir(&path).unwrap();

        // Not `NotFound` (it exists) and not a parse error (there is nothing to parse): the
        // message has to say which path could not be read.
        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
    }
}
