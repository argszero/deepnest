//! The macOS app bundle: the two files a `.app` is actually made of, and the system's reading of
//! one.
//!
//! A `.app` is not a format. It is a directory the Finder agreed to show as a single program, and of
//! the names inside it only two are load-bearing: `Contents/Info.plist`, which the system reads to
//! learn what to launch, and `Contents/MacOS/<CFBundleExecutable>`, the file it launches. Everything
//! else — icons, localizations, embedded frameworks — is optional. This module writes those two and
//! nothing else, because a name written without being needed is a name every later reader has to
//! keep in step for no reason.
//!
//! Four decisions:
//!
//! - **The keys are the machine's, and the name inside them is escaped.** The plist is written as
//!   text rather than through a plist library, because what this project needs from XML is one
//!   `<dict>` of eleven keys and nothing else — but text means escaping, so the version goes through
//!   it. A `1.0&<beta>` written raw is a plist that fails to parse, and the system's answer to an
//!   unparseable `Info.plist` is not an error anybody sees: it is a bundle that does not open.
//! - **A version the system cannot parse is refused here.** `CFBundleShortVersionString` may carry a
//!   suffix, but `CFBundleVersion` must be dotted integers, so a release tagged `0.1.0-rc1` is
//!   refused by name instead of producing a bundle that installs and then cannot be notarized. The
//!   refusal is the moment the fix is a line in the release script rather than a mystery on someone
//!   else's Mac.
//! - **An existing path is refused, never written into.** Half a bundle looks whole: a `Contents/`
//!   that lost its executable between two writes still doubles as something the Finder offers to
//!   open. Refusing an occupied path is what makes the bundle in the release directory either the
//!   one this run wrote or not there at all.
//! - **The executable bit travels.** A binary that arrives `0644` is a bundle that opens onto
//!   nothing, with no message, because the system's own refusal to `exec` a non-executable file
//!   never reaches a person. [`fs::copy`] happens to carry the source's mode on unix, and this does
//!   not rely on it: the mode is set to `0755` whatever arrived.
//!
//! What this module deliberately does **not** do: sign anything, notarize anything, or build a disk
//! image. A signature covers files that exist, so it comes after this; a `.dmg` is the product of
//! `hdiutil`, which exists only on the machine that makes it, and a Rust wrapper around a program
//! that only runs on one platform would be a second thing to keep correct for no reading it adds.
//! [`write`] leaves a bundle the release pipeline can sign in place, and says where it is.
//!
//! **Nothing here is macOS-only but the layout's meaning.** Laying out a bundle is making
//! directories and copying a file, so this compiles and its tests pass on the two platforms that do
//! not ship `.app`s at all — which is what lets the CI legs that cannot build one still say the
//! writer is correct.
//!
//! Which files a signature has to cover is the neighbouring question, and [`macho`] answers it: that
//! module decides what a Mach-O is and walks a tree for the ones inside it. It is separate because
//! the answer matters as much for the payload this bundle carries as for the bundle itself, and
//! because it has to be checkable on the two platforms that will never run `codesign`.
//!
//! [`sign`] is the step that uses that list: it signs each file and verifies each signature, taking
//! the identity's name from the environment and no secret from anywhere at all. It is a seam, so its
//! decisions are tested on the platforms that have no `codesign` to run.

pub mod macho;
pub mod sign;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The directory name a bundle has on disk, which is the name the Finder shows.
pub const BUNDLE: &str = "Deepnest.app";

/// The program inside the bundle, and the value of `CFBundleExecutable`.
///
/// The two are one constant on purpose: a plist naming an executable the bundle does not hold is
/// the other way to produce a bundle that opens onto nothing, and the system says nothing about it
/// either.
pub const EXECUTABLE: &str = "deepnest";

/// `CFBundleName` and `CFBundleDisplayName` — what the menu bar and the Finder call this.
pub const NAME: &str = "Deepnest";

/// `CFBundleIdentifier`.
///
/// Reverse DNS of the only namespace this project owns, the account the repository is published
/// under. It does not change between releases: the system's record of the app's permissions, its
/// saved state and its keychain items are all filed under this string, so a bump would be a new app.
pub const IDENTIFIER: &str = "io.github.argszero.deepnest";

/// `LSMinimumSystemVersion` — the oldest macOS a bundle may be opened on.
///
/// 11 is Big Sur, the first release that runs on Apple silicon at all, so this is the arm64 floor
/// rather than a choice. It is written down because a bundle that omits the key is treated as
/// openable anywhere, and the failure that produces is an illegal-instruction crash rather than a
/// sentence about the system being too old.
pub const MINIMUM_SYSTEM_VERSION: &str = "11.0";

/// What laying out a bundle left behind.
///
/// Named rather than returned as one directory, because the two files are what a caller signs and
/// what it checks: the release pipeline signs every Mach-O in the payload and this executable, and
/// a bundle whose executable it could not name would be a pipeline that has to rebuild the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// The `.app` directory.
    pub app: PathBuf,
    /// `Contents/MacOS/deepnest`.
    pub executable: PathBuf,
    /// `Contents/Info.plist`.
    pub plist: PathBuf,
}

/// Why a bundle could not be laid out.
///
/// As in the rest of this workspace, the variants differ in what each says about the input rather
/// than in what the caller does next: nothing is left behind on any of them.
#[derive(Debug)]
pub enum WriteError {
    /// The binary to wrap is not a file.
    Source {
        /// The path that was named.
        path: PathBuf,
        /// What the filesystem said, or why it was refused.
        source: io::Error,
    },
    /// The version cannot be written into `CFBundleVersion`.
    Version {
        /// The version as it was given.
        version: String,
    },
    /// A bundle is already at the destination.
    Occupied {
        /// The path that is in the way.
        app: PathBuf,
    },
    /// A directory or file could not be created or written.
    Write {
        /// What was being written, as a path inside the bundle.
        entry: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source { path, source } => {
                write!(out, "cannot wrap {}: {source}", path.display())
            }
            Self::Version { version } => write!(
                out,
                "{version:?} cannot be CFBundleVersion, which must be one or more dot-separated \
                 numbers — tag the release with a version the system can read"
            ),
            Self::Occupied { app } => write!(
                out,
                "{} already exists: a bundle is written into an empty path, so remove it first",
                app.display()
            ),
            Self::Write { entry, source } => {
                write!(out, "cannot write {}: {source}", entry.display())
            }
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Version { .. } | Self::Occupied { .. } => None,
        }
    }
}

/// Lays out `into/Deepnest.app` around an already-built binary.
///
/// `version` becomes both `CFBundleShortVersionString` and `CFBundleVersion`, which is what a
/// launcher with no release history behind it needs: the two keys are read by different parts of the
/// system, and a bundle that set only one would show a version the system did not agree with.
pub fn write(into: &Path, binary: &Path, version: &str) -> Result<Bundle, WriteError> {
    let metadata = fs::metadata(binary).map_err(|source| WriteError::Source {
        path: binary.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(WriteError::Source {
            path: binary.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "not a file"),
        });
    }
    if !is_a_version(version) {
        return Err(WriteError::Version {
            version: version.to_string(),
        });
    }

    let app = into.join(BUNDLE);
    if app.exists() {
        return Err(WriteError::Occupied { app });
    }

    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let executable = macos.join(EXECUTABLE);
    let plist = contents.join("Info.plist");

    fs::create_dir_all(&macos).map_err(|source| WriteError::Write {
        entry: macos.clone(),
        source,
    })?;
    fs::copy(binary, &executable).map_err(|source| WriteError::Write {
        entry: executable.clone(),
        source,
    })?;
    make_executable(&executable)?;
    fs::write(&plist, info_plist(version)).map_err(|source| WriteError::Write {
        entry: plist.clone(),
        source,
    })?;

    Ok(Bundle {
        app,
        executable,
        plist,
    })
}

/// The `Info.plist` for a bundle holding [`EXECUTABLE`] at `version`.
///
/// Public because it is the half worth reading on its own: the release pipeline checks the bundle it
/// produced against this, and so does anyone asking what the system will be told about it.
pub fn info_plist(version: &str) -> String {
    let mut plist = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
"#,
    );
    for (key, value) in [
        ("CFBundleDevelopmentRegion", "en"),
        ("CFBundleExecutable", EXECUTABLE),
        ("CFBundleIdentifier", IDENTIFIER),
        ("CFBundleInfoDictionaryVersion", "6.0"),
        ("CFBundleName", NAME),
        ("CFBundleDisplayName", NAME),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleShortVersionString", version),
        ("CFBundleVersion", version),
        ("LSMinimumSystemVersion", MINIMUM_SYSTEM_VERSION),
    ] {
        plist.push_str(&format!(
            "\t<key>{key}</key>\n\t<string>{}</string>\n",
            escaped(value)
        ));
    }
    plist.push_str("</dict>\n</plist>\n");
    plist
}

/// `text` as the body of an XML element.
///
/// The four characters XML reserves, and nothing else: an app name or a version arrives from a
/// command line, and the cost of being wrong about one of them is a plist that does not parse —
/// which the system reports by doing nothing at all.
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Whether `version` is what `CFBundleVersion` accepts: one or more dot-separated numbers.
///
/// Not a semantic-version check — `1.10.0` is fine and so is `42`. What it refuses is everything the
/// system refuses, which in practice is any release tag carrying a word.
fn is_a_version(version: &str) -> bool {
    !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Sets the executable bit on `path`, which no non-unix has.
fn make_executable(path: &Path) -> Result<(), WriteError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|source| {
            WriteError::Write {
                entry: path.to_path_buf(),
                source,
            }
        })?;
    }
    #[cfg(not(unix))]
    {
        // Nothing to set: whether a file can be run is the name's business here, and `deepnest` has
        // no extension — so a bundle laid out on Windows is a directory of the right shape, which is
        // all the test on that leg claims.
        let _ = path;
    }
    Ok(())
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
                std::env::temp_dir().join(format!("deepnest-bundle-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        /// A file standing in for a built binary, with the bytes a caller could recognise.
        fn binary(&self) -> PathBuf {
            let path = self.base.join("deepnest");
            fs::write(&path, b"\x7fELF not really, but it is bytes").unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_bundle_is_the_two_files_the_system_looks_for() {
        let scratch = Scratch::new("two-files");
        let bundle = write(&scratch.base.join("dist"), &scratch.binary(), "0.1.0").unwrap();

        assert_eq!(bundle.app, scratch.base.join("dist").join(BUNDLE));
        assert_eq!(
            bundle.executable,
            bundle.app.join("Contents").join("MacOS").join(EXECUTABLE)
        );
        assert_eq!(bundle.plist, bundle.app.join("Contents").join("Info.plist"));
        assert!(bundle.executable.is_file());
        assert!(bundle.plist.is_file());

        // The executable is the binary it was given, byte for byte: a bundle that wrapped a stale
        // build is the mistake a copy leaves room for and a symlink would hide.
        assert_eq!(
            fs::read(&bundle.executable).unwrap(),
            fs::read(scratch.binary()).unwrap()
        );

        // And nothing else: `Contents` holds one directory and one file.
        let mut names: Vec<String> = fs::read_dir(bundle.app.join("Contents"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["Info.plist", "MacOS"]);
    }

    #[test]
    fn the_plist_names_the_executable_that_is_in_the_bundle() {
        let plist = info_plist("0.1.0");
        assert!(plist.contains(&format!(
            "<key>CFBundleExecutable</key>\n\t<string>{EXECUTABLE}</string>"
        )));
        assert!(plist.contains(&format!(
            "<key>CFBundleIdentifier</key>\n\t<string>{IDENTIFIER}</string>"
        )));
        assert!(plist.contains("<key>CFBundleShortVersionString</key>\n\t<string>0.1.0</string>"));
        assert!(plist.contains("<key>CFBundleVersion</key>\n\t<string>0.1.0</string>"));
        assert!(plist.contains("<key>CFBundlePackageType</key>\n\t<string>APPL</string>"));
        assert!(plist.contains(&format!(
            "<key>LSMinimumSystemVersion</key>\n\t<string>{MINIMUM_SYSTEM_VERSION}</string>"
        )));
        // A launcher that hid itself from the Dock would be a process with no way to quit it.
        assert!(!plist.contains("LSUIElement"));
        assert!(plist.starts_with("<?xml version=\"1.0\""));
        assert!(plist.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn a_version_is_escaped_rather_than_breaking_the_file() {
        assert_eq!(escaped("1.0&<beta>"), "1.0&amp;&lt;beta&gt;");
        assert_eq!(escaped("plain"), "plain");
    }

    #[test]
    fn a_version_the_system_cannot_read_is_refused() {
        let scratch = Scratch::new("bad-version");
        let into = scratch.base.join("dist");
        for bad in ["", "0.1.0-rc1", "v0.1.0", "0..1", "1.0 beta"] {
            let refused = write(&into, &scratch.binary(), bad).unwrap_err();
            assert!(
                matches!(refused, WriteError::Version { .. }),
                "{bad:?} was not refused by name: {refused}"
            );
        }
        assert!(
            !into.join(BUNDLE).exists(),
            "a refusal left a bundle behind"
        );

        // And the dotted integers a tag actually carries are accepted, including a two-digit part.
        for good in ["0.1.0", "1.10.0", "42"] {
            let scratch = Scratch::new(&format!("good-version-{}", good.replace('.', "_")));
            assert!(
                write(&scratch.base.join("dist"), &scratch.binary(), good).is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn a_binary_that_is_not_there_is_refused_and_nothing_is_written() {
        let scratch = Scratch::new("no-binary");
        let into = scratch.base.join("dist");
        let missing = scratch.base.join("nowhere");
        let refused = write(&into, &missing, "0.1.0").unwrap_err();
        assert!(matches!(refused, WriteError::Source { .. }), "{refused}");
        assert!(!into.exists(), "a refusal created the destination");
    }

    #[test]
    fn a_directory_named_like_the_binary_is_refused() {
        let scratch = Scratch::new("binary-is-a-directory");
        let directory = scratch.base.join("deepnest");
        fs::create_dir_all(&directory).unwrap();
        let refused = write(&scratch.base.join("dist"), &directory, "0.1.0").unwrap_err();
        assert!(matches!(refused, WriteError::Source { .. }), "{refused}");
    }

    #[test]
    fn a_bundle_that_is_already_there_is_refused_rather_than_written_into() {
        let scratch = Scratch::new("occupied");
        let into = scratch.base.join("dist");
        write(&into, &scratch.binary(), "0.1.0").unwrap();

        let refused = write(&into, &scratch.binary(), "0.2.0").unwrap_err();
        assert!(matches!(refused, WriteError::Occupied { .. }), "{refused}");

        // The bundle that was there is untouched: this is not a writer that replaces one.
        let plist =
            fs::read_to_string(into.join(BUNDLE).join("Contents").join("Info.plist")).unwrap();
        assert!(plist.contains("<string>0.1.0</string>"));
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_arrives_executable() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new("mode");
        // A source that is deliberately not executable, so the mode can only come from this writer.
        let binary = scratch.binary();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();

        let bundle = write(&scratch.base.join("dist"), &binary, "0.1.0").unwrap();
        let mode = fs::metadata(&bundle.executable)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o755, 0o755, "{mode:o}");
    }
}
