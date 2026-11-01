//! `deepnest-bundle` — lays out the macOS app bundle around a binary that already exists.
//!
//! The binary and the bundle are two steps because they are two machines' worth of work: a release
//! build compiles, and laying out a `.app` is a copy and a text file. Keeping them apart is what lets
//! this be run by hand against a build already on disk — which is how the layout gets checked without
//! waiting for a release — and it is why this program takes a path to the binary instead of building
//! one.
//!
//! What it prints is the two paths somebody signs: the executable inside the bundle, and the plist.
//! A pipeline that had to reconstruct those paths from a directory listing would be a pipeline that
//! breaks the first time a name changes, so the layout that decides them is also the thing that says
//! where they landed.
//!
//! The second action is the signing itself, in the same program because it is the next thing a
//! release does: `--sign <dir>` walks a tree, signs every Mach-O in it and verifies each signature,
//! taking the identity's *name* from `DEEPNEST_SIGNING_IDENTITY`. No password is read here and none
//! can be: the identity's key has to be in a keychain already, put there by the pipeline step that
//! owns that secret.
//!
//! Usage:
//!
//! ```text
//! deepnest-bundle --binary <path> --into <dir> [--version <version>]
//! deepnest-bundle --sign <dir>
//! ```
//!
//! `--version` defaults to this crate's own version, which is the workspace's. The release pipeline
//! passes the tag instead, because a bundle's version is what the tag said rather than what the
//! build happened to be at.

use std::path::PathBuf;
use std::process::ExitCode;

use deepnest_bundle::sign::{sign_all, StdSigner, IDENTITY_VAR};
use deepnest_bundle::{write, BUNDLE};

const USAGE: &str = "\
deepnest-bundle — lay out the macOS app bundle around a binary that already exists

Usage:
  deepnest-bundle --binary <path> --into <dir> [--version <version>]

  --binary    the `deepnest` executable to wrap
  --into      the directory to write Deepnest.app into; it must not hold one already
  --version   the version the bundle reports (default: this build's)

  --sign      sign every Mach-O inside <dir> and verify each signature; the identity is named by
              DEEPNEST_SIGNING_IDENTITY, whose key must already be in a keychain

The bundle is written unsigned; `--sign` is what signs one, and notarizing it and making a `.dmg`
are the release pipeline's.
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("deepnest-bundle: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    match arguments(std::env::args().skip(1))? {
        Action::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Action::Sign(root) => {
            let identity = std::env::var(IDENTITY_VAR).map_err(|_| {
                format!(
                    "{IDENTITY_VAR} is not set: it names the Developer ID Application identity to \
                     sign with, and that identity's key must already be in a keychain"
                )
            })?;
            let signer = StdSigner::default();
            let signed = sign_all(&root, &identity, &signer).map_err(|error| error.to_string())?;
            println!("signed {} files with {identity}", signed.files.len());
            for path in signed.files {
                println!("  {}", path.display());
            }
            Ok(())
        }
        Action::LayOut(asked) => {
            let bundle = write(&asked.into, &asked.binary, &asked.version).map_err(|error| {
                // `Display` on the error already names the path or the version, so the sentence is
                // the whole message rather than a category with the detail after it.
                error.to_string()
            })?;
            println!("wrote {}", bundle.app.display());
            println!("  executable {}", bundle.executable.display());
            println!("  plist      {}", bundle.plist.display());
            Ok(())
        }
    }
}

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Help,
    LayOut(Layout),
    Sign(PathBuf),
}

/// The three inputs, once parsed.
#[derive(Debug, PartialEq, Eq)]
struct Layout {
    binary: PathBuf,
    into: PathBuf,
    version: String,
}

/// Reads the arguments, or says what is missing.
///
/// Hand-written rather than pulled from a crate: there are three flags, and an argument parser is a
/// dependency to add and keep current for a program whose whole vocabulary is below.
fn arguments(args: impl Iterator<Item = String>) -> Result<Action, String> {
    let mut binary: Option<PathBuf> = None;
    let mut into: Option<PathBuf> = None;
    let mut version: Option<String> = None;
    let mut signing: Option<PathBuf> = None;

    let mut args = args;
    while let Some(argument) = args.next() {
        // `--flag=value` and `--flag value` are both what people type.
        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (argument, None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            match inline.clone() {
                Some(value) => Ok(value),
                None => args.next().ok_or_else(|| format!("{name} needs a value")),
            }
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Action::Help),
            "--binary" => binary = Some(PathBuf::from(value("--binary")?)),
            "--into" => into = Some(PathBuf::from(value("--into")?)),
            "--version" => version = Some(value("--version")?),
            "--sign" => signing = Some(PathBuf::from(value("--sign")?)),
            other => {
                return Err(format!(
                    "{other} is not an argument this program takes\n\n{USAGE}"
                ))
            }
        }
    }

    // One action per run: a command line that names both is a command line whose author meant one
    // of them and is about to get the other.
    if let Some(root) = signing {
        if binary.is_some() || into.is_some() || version.is_some() {
            return Err(format!(
                "--sign is an action of its own, so it takes no --binary, --into or --version\n\n{USAGE}"
            ));
        }
        return Ok(Action::Sign(root));
    }

    let binary = binary
        .ok_or_else(|| format!("--binary is required: it is the build to wrap\n\n{USAGE}"))?;
    let into =
        into.ok_or_else(|| format!("--into is required: it is where {BUNDLE} goes\n\n{USAGE}"))?;
    Ok(Action::LayOut(Layout {
        binary,
        into,
        version: version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Action, String> {
        arguments(args.iter().map(|argument| argument.to_string()))
    }

    #[test]
    fn the_two_paths_are_all_a_bundle_needs() {
        let action = parsed(&["--binary", "target/release/deepnest", "--into", "dist"]).unwrap();
        assert_eq!(
            action,
            Action::LayOut(Layout {
                binary: PathBuf::from("target/release/deepnest"),
                into: PathBuf::from("dist"),
                version: env!("CARGO_PKG_VERSION").to_string(),
            })
        );
    }

    #[test]
    fn a_version_can_be_attached_to_the_flag_or_stand_after_it() {
        let joined =
            parsed(&["--binary=build/deepnest", "--into=dist", "--version=1.2.3"]).unwrap();
        let spaced = parsed(&[
            "--binary",
            "build/deepnest",
            "--into",
            "dist",
            "--version",
            "1.2.3",
        ])
        .unwrap();
        assert_eq!(joined, spaced);
        match spaced {
            Action::LayOut(layout) => assert_eq!(layout.version, "1.2.3"),
            other => panic!("--version asked for {other:?}"),
        }
    }

    #[test]
    fn a_missing_path_is_named_rather_than_defaulted() {
        let binary = parsed(&["--into", "dist"]).unwrap_err();
        assert!(binary.contains("--binary"), "{binary}");
        let into = parsed(&["--binary", "build/deepnest"]).unwrap_err();
        assert!(into.contains("--into"), "{into}");
    }

    #[test]
    fn a_flag_without_a_value_is_an_error_rather_than_an_empty_path() {
        let refused = parsed(&["--binary"]).unwrap_err();
        assert!(refused.contains("needs a value"), "{refused}");
    }

    #[test]
    fn signing_is_an_action_of_its_own() {
        let action = parsed(&["--sign", "dist"]).unwrap();
        assert_eq!(action, Action::Sign(PathBuf::from("dist")));
        let joined = parsed(&["--sign=dist"]).unwrap();
        assert_eq!(joined, Action::Sign(PathBuf::from("dist")));
    }

    #[test]
    fn signing_beside_a_layout_is_refused_rather_than_doing_one_of_them() {
        let refused = parsed(&["--sign", "dist", "--binary", "build/deepnest"]).unwrap_err();
        assert!(refused.contains("action of its own"), "{refused}");
        let refused = parsed(&["--sign", "dist", "--into", "dist"]).unwrap_err();
        assert!(refused.contains("action of its own"), "{refused}");
        // And `--sign` alone still needs its directory.
        let refused = parsed(&["--sign"]).unwrap_err();
        assert!(refused.contains("needs a value"), "{refused}");
    }

    #[test]
    fn the_usage_names_the_variable_the_signing_step_reads() {
        // `USAGE` is a plain literal rather than a format string, so the name is spelled there and
        // this is what stops it drifting from the constant the code actually reads.
        assert!(USAGE.contains(IDENTITY_VAR), "{USAGE}");
    }

    #[test]
    fn something_this_program_does_not_take_is_refused_with_the_usage() {
        let refused = parsed(&["--bundle", "x"]).unwrap_err();
        assert!(refused.contains("--bundle is not an argument"), "{refused}");
        assert!(refused.contains("Usage:"), "{refused}");
    }
}
