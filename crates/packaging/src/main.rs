//! `deepnest-pack` — turns a toolchain tree into the archive a release embeds.
//!
//! This is the release pipeline's first step and the only program that writes a payload: the
//! toolchain is assembled by whatever the machine has (a `rustup` toolchain, the JavaScript
//! runtime's own installer), `deepnest-pack` compresses the result, and the build then embeds it by
//! being handed the archive through `DEEPNEST_PAYLOAD`.
//!
//! It is a separate program from `deepnest-bundle` because they work on different things at
//! different moments: packing happens **before** the binary exists, and the bundle is laid out
//! around a binary that already does. A single program would have had to be told which half of its
//! life it was in.
//!
//! Nothing here decides what a toolchain contains, and nothing here signs anything. The archive is
//! the tree it was given, entry for entry, through `packaging::pack`'s own contract — sorted, with
//! times and ownership zeroed, so two builds of one tree produce one archive and a signature over
//! it means the payload did not change.
//!
//! Usage:
//!
//! ```text
//! deepnest-pack --from <dir> --into <file>
//! ```
//!
//! `--from` is the **contents** of the toolchain, not a directory containing one: what is under it
//! is what a first run finds under `~/.deepnest/toolchain/<version>/`, so the programs belong in
//! `<dir>/bin/`.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::process::ExitCode;

use deepnest_packaging::pack;

const USAGE: &str = "\
deepnest-pack — pack a toolchain tree into the archive a release embeds

Usage:
  deepnest-pack --from <dir> --into <file>

  --from   the contents of the toolchain; its programs belong in <dir>/bin
  --into   the archive to write (a tar inside a zstd stream)

The archive is what DEEPNEST_PAYLOAD names when the binary is built, and what the binary unpacks
into ~/.deepnest/toolchain/<version>/ on its first build. Signing it and making a disk image are
later steps, and neither is this program's.
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("deepnest-pack: {message}");
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
        Action::Pack { from, into } => {
            if !from.is_dir() {
                return Err(format!("`{}` is not a directory", from.display()));
            }
            // Refused rather than overwritten: a pack that silently replaced an existing archive
            // would make "the payload did not change" a claim about a file, not about a tree.
            if into.exists() {
                return Err(format!(
                    "`{}` already exists — remove it, or pack somewhere else",
                    into.display()
                ));
            }
            let file = File::create(&into)
                .map_err(|error| format!("cannot write `{}`: {error}", into.display()))?;
            let packed = pack(&from, BufWriter::new(file))
                .map_err(|error| format!("cannot pack `{}`: {error}", from.display()))?;
            // What the pack itself can say about its output: the number an unpack will read back.
            // It is printed rather than kept, because the workflow that calls this quotes it in its
            // own log and that is where a release's evidence lives.
            println!(
                "packed {} entries from {} into {}",
                packed.entries,
                from.display(),
                into.display()
            );
            Ok(())
        }
    }
}

#[derive(Debug)]
enum Action {
    Help,
    Pack { from: PathBuf, into: PathBuf },
}

fn arguments(args: impl Iterator<Item = String>) -> Result<Action, String> {
    let mut from: Option<PathBuf> = None;
    let mut into: Option<PathBuf> = None;
    let mut rest = args;
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--help" | "-h" => return Ok(Action::Help),
            "--from" => {
                from = Some(PathBuf::from(
                    rest.next()
                        .ok_or_else(|| "`--from` needs a directory".to_string())?,
                ));
            }
            "--into" => {
                into = Some(PathBuf::from(
                    rest.next()
                        .ok_or_else(|| "`--into` needs a path".to_string())?,
                ));
            }
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    match (from, into) {
        (Some(from), Some(into)) => Ok(Action::Pack { from, into }),
        _ => Err(format!("both `--from` and `--into` are needed\n\n{USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(values: &[&str]) -> Result<Action, String> {
        arguments(values.iter().map(|value| value.to_string()))
    }

    #[test]
    fn the_two_paths_are_what_a_pack_is() {
        match named(&["--from", "tree", "--into", "payload.zst"]) {
            Ok(Action::Pack { from, into }) => {
                assert_eq!(from, PathBuf::from("tree"));
                assert_eq!(into, PathBuf::from("payload.zst"));
            }
            _ => panic!("two paths did not make a pack"),
        }
    }

    #[test]
    fn an_incomplete_pack_is_refused_rather_than_guessed() {
        // A packer with a default for either path would write an archive nobody asked for, in a
        // directory nobody named. The workflow passes both, so both are required.
        assert!(named(&["--from", "tree"]).is_err());
        assert!(named(&["--into", "payload.zst"]).is_err());
        assert!(named(&[]).is_err());
    }

    #[test]
    fn an_unknown_argument_is_refused_and_the_usage_comes_with_it() {
        // The caller here is a workflow file, and a typo in one is otherwise a step that packs
        // nothing and reports success. The message carries the usage so the fix is in the error.
        let message = named(&["--fromm", "tree"]).unwrap_err();
        assert!(message.contains("--fromm"), "{message}");
        assert!(message.contains("Usage:"), "{message}");
    }

    #[test]
    fn help_is_an_answer_rather_than_an_error() {
        assert!(matches!(named(&["--help"]), Ok(Action::Help)));
        assert!(matches!(named(&["-h"]), Ok(Action::Help)));
    }
}
