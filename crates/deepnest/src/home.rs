//! Where deepnest keeps everything, and the fact that the directory is private.
//!
//! `~/.deepnest` holds the model key, the control token and every app the user generated, so it
//! is created `0700`: on a shared machine the other accounts are the threat model that a `0600`
//! token file alone does not cover, because they could otherwise read the app sources.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The environment variable that relocates the whole deepnest home.
///
/// It exists so a developer — and a test — can run a throwaway deepnest without touching the
/// real `~/.deepnest`, where a user's apps and their agents live.
pub const HOME_ENV: &str = "DEEPNEST_HOME";

/// The directory name appended to the user's home directory.
pub const DIR_NAME: &str = ".deepnest";

/// `$DEEPNEST_HOME`, or `<user home>/.deepnest`, or `None` when neither can be found.
pub fn home() -> Option<PathBuf> {
    resolve(
        std::env::var(HOME_ENV).ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("USERPROFILE").ok().as_deref(),
    )
}

/// [`home`] with the environment passed in, so the rule is testable without mutating it.
///
/// `USERPROFILE` is read as well as `HOME` because Windows sets the latter inconsistently:
/// Git Bash sets `HOME`, PowerShell and `cmd` set `USERPROFILE`, and a binary that only knows
/// one of them creates two different homes depending on how it was launched.
pub fn resolve(
    deepnest_home: Option<&str>,
    home: Option<&str>,
    userprofile: Option<&str>,
) -> Option<PathBuf> {
    fn set(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|value| !value.is_empty())
    }

    if let Some(dir) = set(deepnest_home) {
        return Some(PathBuf::from(dir));
    }
    set(home)
        .or_else(|| set(userprofile))
        .map(|dir| Path::new(dir).join(DIR_NAME))
}

/// Create the deepnest home if it is absent, and make it private.
///
/// Called on every start rather than only on first run: a directory that was `0755` — copied
/// from another machine, restored from a backup, created by an older deepnest — is narrowed
/// here instead of being trusted.
pub fn prepare_home(home: &Path) -> io::Result<()> {
    fs::create_dir_all(home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(home, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_variable_wins_over_the_user_home() {
        assert_eq!(
            resolve(Some("/tmp/throwaway"), Some("/Users/ada"), None),
            Some(PathBuf::from("/tmp/throwaway"))
        );
    }

    #[test]
    fn a_blank_variable_is_not_a_path() {
        // An exported-but-empty `DEEPNEST_HOME=` must not resolve to the process's cwd, which
        // is what `PathBuf::from("")` would quietly become.
        assert_eq!(
            resolve(Some("  "), Some("/Users/ada"), None),
            Some(PathBuf::from("/Users/ada/.deepnest"))
        );
    }

    #[test]
    fn userprofile_is_the_windows_fallback() {
        assert_eq!(
            resolve(None, None, Some(r"C:\Users\ada")),
            Some(PathBuf::from(r"C:\Users\ada").join(DIR_NAME))
        );
        assert_eq!(resolve(None, None, None), None);
    }

    #[test]
    fn the_home_is_created_private() {
        let base = std::env::temp_dir().join(format!("deepnest-home-{}", std::process::id()));
        let home = base.join("nested");
        let _ = fs::remove_dir_all(&base);

        prepare_home(&home).unwrap();
        assert!(home.is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&home).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "the home holds the model key and the token");
        }

        let _ = fs::remove_dir_all(&base);
    }
}
