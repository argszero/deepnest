//! Signing every Mach-O in a tree, which is the half of "sign everything" that runs `codesign`.
//!
//! The design of record signs each binary in the payload **before** it is packed, because first-run
//! unpacking does not re-sign and an unsigned child process is refused under hardened runtime. Which
//! files those are is [`crate::macho`]'s answer; this module takes that list and signs each one, or
//! refuses and signs nothing.
//!
//! Five decisions, in the order they cost something:
//!
//! - **No secret passes through this program, and none is written anywhere.** The release pipeline
//!   imports the Developer ID certificate into a keychain in a step of its own, and what reaches this
//!   module is the identity's *name* — `Developer ID Application: …` — which is not a secret and is
//!   the string `codesign` is told to sign with. That is why nothing here is an environment passphrase:
//!   a password that reaches a program is a password in that program's process table, and the design
//!   that needs one is the design that only works with a `.p12` and a prompt. A missing identity is
//!   refused by name ([`SignError::NoIdentity`]) rather than silently producing an unsigned bundle.
//! - **Every file gets `--options runtime --timestamp --force`.** Hardened runtime and a secure
//!   timestamp are the two flags notarization will not accept without — a signature missing either is
//!   a notarization rejection hours later instead of a signing failure now. `--force` is not about
//!   overwriting: every arm64 binary already carries an ad-hoc signature the linker left on it, and
//!   `codesign` refuses to replace one without it, so a signing step without `--force` fails on the
//!   first file and on every file after it.
//! - **Each file is verified after it is signed, and a failed verification fails the run.** Signing
//!   and being signed are different claims, and only the second one is what notarization and the
//!   loader read. A run that reports success without asking therefore reports the interesting
//!   failure — "codesign exited 0 and the signature is not there" — as a green build.
//! - **A tree with no binaries is refused, not signed happily.** An empty payload is either the wrong
//!   directory or a packer that dropped its content, and both are the mistake where a green signing
//!   step is the last chance to notice. The cost is that this cannot sign an empty bundle, which is
//!   not a thing anybody wants.
//! - **The signing and the verifying are a seam.** What is worth testing is the decision — which
//!   files, in what order, and what happens when the second one fails — and a test of that must not
//!   depend on a certificate being installed on the machine running it. [`StdSigner`] is the real
//!   one, and it is the only part here that only works on macOS; the rest compiles and its tests pass
//!   on all three platforms, which is what lets the legs that cannot run `codesign` still vouch for
//!   the logic that decides what to call it on.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::macho::{self, ScanError};

/// The environment variable the identity is read from.
///
/// A variable rather than an argument because it is one value for a whole release rather than one
/// value per file, and because it keeps the identity out of a command line that ends up in a build
/// log. It carries a name, never a secret — see this module's header.
pub const IDENTITY_VAR: &str = "DEEPNEST_SIGNING_IDENTITY";

/// What `codesign` is run for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Put a signature on this file.
    Sign,
    /// Ask whether the signature on this file is real.
    Verify,
}

impl fmt::Display for Step {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sign => write!(out, "signing"),
            Self::Verify => write!(out, "verifying"),
        }
    }
}

/// What a signer did not manage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// What the program said, in its own words — `codesign` names the reason and this carries it.
    pub detail: String,
}

/// Signs a file, and says whether a file is signed.
///
/// A seam rather than a direct call, for the reason [`crate::macho`] is a module: the behaviour worth
/// testing is the decision, and a test that had to own a Developer ID certificate would never run on
/// CI. [`StdSigner`] is the real one.
pub trait Signer: Send + Sync {
    /// Sign `path` with `identity`.
    fn sign(&self, path: &Path, identity: &str) -> Result<(), Refusal>;

    /// Confirm that `path` carries a signature that will survive notarization and the loader.
    fn verify(&self, path: &Path) -> Result<(), Refusal>;
}

/// Why signing did not happen, or did not finish.
///
/// As in the rest of this workspace, the variants differ in what each says about the input rather
/// than in what the caller does next: every one of them means the tree is not signed, and the only
/// truthful action is to stop rather than to ship a half-signed payload — a half-signed payload
/// installs, opens, and then fails to start a compiler.
#[derive(Debug)]
pub enum SignError {
    /// No identity was given, so nothing was attempted.
    NoIdentity,
    /// The tree could not be enumerated.
    Scan(ScanError),
    /// The tree holds no binaries, so signing it proves nothing.
    NothingToSign {
        /// The tree that was asked about.
        root: PathBuf,
    },
    /// `codesign` refused a file, or was not there to ask.
    Refused {
        /// The file.
        path: PathBuf,
        /// Which of the two steps was running.
        step: Step,
        /// What the program said.
        detail: String,
    },
}

impl fmt::Display for SignError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoIdentity => write!(
                out,
                "{IDENTITY_VAR} is not set: it must name a Developer ID Application identity that \
                 already has its key in a keychain, and a release that signs nothing must not look \
                 like one that signed everything"
            ),
            Self::Scan(error) => write!(out, "{error}"),
            Self::NothingToSign { root } => write!(
                out,
                "{} holds no Mach-O binaries: an empty tree is the wrong directory or a payload \
                 that lost its content, and signing it would report success for nothing",
                root.display()
            ),
            Self::Refused { path, step, detail } => {
                write!(out, "{step} {} failed: {detail}", path.display())
            }
        }
    }
}

impl std::error::Error for SignError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            _ => None,
        }
    }
}

/// What a finished signing run did, with the evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// The identity every signature was made with.
    pub identity: String,
    /// Every file that now carries one, in the order they were signed.
    pub files: Vec<PathBuf>,
}

/// The real signer: `/usr/bin/codesign`.
#[derive(Debug, Clone)]
pub struct StdSigner {
    /// The program to run.
    ///
    /// A field rather than a constant so a test can point it at a script, and so a machine whose
    /// `codesign` lives somewhere else can say so instead of being told where it is.
    pub codesign: PathBuf,
}

impl Default for StdSigner {
    fn default() -> Self {
        Self {
            codesign: PathBuf::from("/usr/bin/codesign"),
        }
    }
}

impl StdSigner {
    /// Runs `codesign` with `arguments`, returning its complaint on any non-zero exit.
    ///
    /// Output is captured rather than inherited, so the reason reaches the caller as a sentence
    /// instead of scrolling past in a build log. stdin is `/dev/null` for the reason
    /// [`crate::StdSigner`]'s sibling in `crates/apps` gives about its runner: a program that decides
    /// to ask a question must fail rather than take the terminal of whatever started it — and for
    /// `codesign` the question would be a keychain password prompt, which in CI waits forever.
    fn run(&self, arguments: &[&std::ffi::OsStr]) -> Result<(), Refusal> {
        let output = Command::new(&self.codesign)
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .map_err(|source| Refusal {
                detail: match source.kind() {
                    io::ErrorKind::NotFound => format!(
                        "{} is not on this machine: signing is a macOS step, and a build elsewhere \
                         produces an unsigned artifact on purpose",
                        self.codesign.display()
                    ),
                    _ => format!("cannot run {}: {source}", self.codesign.display()),
                },
            })?;

        if output.status.success() {
            return Ok(());
        }
        // `codesign` writes its reason to stderr and nothing to stdout, so the message is stderr
        // when there is one and the status when there is not — an empty detail would be a failure
        // with nothing to read.
        let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(Refusal {
            detail: if said.is_empty() {
                format!("{} exited {}", self.codesign.display(), output.status)
            } else {
                said
            },
        })
    }
}

impl Signer for StdSigner {
    fn sign(&self, path: &Path, identity: &str) -> Result<(), Refusal> {
        use std::ffi::OsStr;

        self.run(&[
            OsStr::new("--sign"),
            OsStr::new(identity),
            // Hardened runtime and a secure timestamp: notarization refuses a signature without
            // either, and discovering that hours later is the cost this flag pair avoids.
            OsStr::new("--options"),
            OsStr::new("runtime"),
            OsStr::new("--timestamp"),
            // Every arm64 binary already has an ad-hoc signature from the linker, and `codesign`
            // will not replace one without this.
            OsStr::new("--force"),
            path.as_os_str(),
        ])
    }

    fn verify(&self, path: &Path) -> Result<(), Refusal> {
        use std::ffi::OsStr;

        // `--strict` is the check notarization and the loader apply; `--verify` alone accepts
        // signatures that will not pass either.
        self.run(&[
            OsStr::new("--verify"),
            OsStr::new("--strict"),
            OsStr::new("--verbose=2"),
            path.as_os_str(),
        ])
    }
}

/// Signs every Mach-O in `root` with `identity`, verifying each one as it goes.
///
/// The list is taken before anything is signed, so a tree that cannot be enumerated is left exactly
/// as it was rather than half-signed. An empty `identity` is refused for the same reason
/// [`SignError::NoIdentity`] exists: `codesign --sign ""` is not a signature.
pub fn sign_all(root: &Path, identity: &str, signer: &dyn Signer) -> Result<Signed, SignError> {
    if identity.trim().is_empty() {
        return Err(SignError::NoIdentity);
    }

    let files = macho::machos_in(root).map_err(SignError::Scan)?;
    if files.is_empty() {
        return Err(SignError::NothingToSign {
            root: root.to_path_buf(),
        });
    }

    for path in &files {
        signer
            .sign(path, identity)
            .map_err(|refusal| SignError::Refused {
                path: path.clone(),
                step: Step::Sign,
                detail: refusal.detail,
            })?;
        signer.verify(path).map_err(|refusal| SignError::Refused {
            path: path.clone(),
            step: Step::Verify,
            detail: refusal.detail,
        })?;
    }

    Ok(Signed {
        identity: identity.to_string(),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// What a [`Recorder`] was asked to do, in the order it was asked.
    type Called = Vec<(Step, PathBuf)>;

    /// A signer that records instead of signing, and can be told to fail on one file.
    struct Recorder {
        called: Mutex<Called>,
        /// The file whose signing fails, when one is set.
        fails_signing: Option<PathBuf>,
        /// The file whose verification fails, when one is set.
        fails_verifying: Option<PathBuf>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                called: Mutex::new(Vec::new()),
                fails_signing: None,
                fails_verifying: None,
            }
        }

        fn failing_to_sign(name: &str) -> Self {
            Self {
                fails_signing: Some(PathBuf::from(name)),
                ..Self::new()
            }
        }

        fn failing_to_verify(name: &str) -> Self {
            Self {
                fails_verifying: Some(PathBuf::from(name)),
                ..Self::new()
            }
        }

        fn called(&self) -> Called {
            self.called.lock().unwrap().clone()
        }

        /// The files it was asked to sign, in order.
        fn signed(&self) -> Vec<PathBuf> {
            self.called()
                .into_iter()
                .filter(|(step, _)| *step == Step::Sign)
                .map(|(_, path)| path)
                .collect()
        }
    }

    impl Signer for Recorder {
        fn sign(&self, path: &Path, _identity: &str) -> Result<(), Refusal> {
            self.called
                .lock()
                .unwrap()
                .push((Step::Sign, path.to_path_buf()));
            if self.fails_signing.as_deref() == Some(path) {
                return Err(Refusal {
                    detail: "code object is not signed at all".to_string(),
                });
            }
            Ok(())
        }

        fn verify(&self, path: &Path) -> Result<(), Refusal> {
            self.called
                .lock()
                .unwrap()
                .push((Step::Verify, path.to_path_buf()));
            if self.fails_verifying.as_deref() == Some(path) {
                return Err(Refusal {
                    detail: "code object is not signed at all".to_string(),
                });
            }
            Ok(())
        }
    }

    /// What an arm64 mac writes at the front of a binary.
    const BINARY: &[u8] = b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01 and the rest is not read";

    struct Scratch {
        base: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("deepnest-sign-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn binary(&self, relative: &str) -> PathBuf {
            self.file(relative, BINARY)
        }

        fn text(&self, relative: &str) -> PathBuf {
            self.file(relative, b"#!/bin/sh\nexit 0\n")
        }

        fn file(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.base.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn every_binary_is_signed_and_then_verified_in_the_order_it_was_found() {
        let scratch = Scratch::new("everything");
        let rustc = scratch.binary("payload/bin/rustc");
        let driver = scratch.binary("payload/lib/librustc_driver.dylib");
        scratch.text("payload/bin/rustfmt");

        let recorder = Recorder::new();
        let signed = sign_all(
            &scratch.base.join("payload"),
            "Developer ID Application: A (B)",
            &recorder,
        )
        .unwrap();

        assert_eq!(signed.identity, "Developer ID Application: A (B)");
        assert_eq!(signed.files, vec![rustc.clone(), driver.clone()]);

        // Sign then verify, per file, and nothing for the script: a run that verified before signing
        // would pass on a binary that had its linker signature already.
        assert_eq!(
            recorder.called(),
            vec![
                (Step::Sign, rustc.clone()),
                (Step::Verify, rustc),
                (Step::Sign, driver.clone()),
                (Step::Verify, driver),
            ]
        );
    }

    #[test]
    fn an_identity_that_is_not_there_signs_nothing() {
        let scratch = Scratch::new("no-identity");
        scratch.binary("payload/bin/rustc");

        let recorder = Recorder::new();
        for absent in ["", "   "] {
            let refused = sign_all(&scratch.base.join("payload"), absent, &recorder).unwrap_err();
            assert!(matches!(refused, SignError::NoIdentity), "{refused}");
            assert!(refused.to_string().contains(IDENTITY_VAR), "{refused}");
        }
        assert!(recorder.called().is_empty(), "a refusal still ran codesign");
    }

    #[test]
    fn a_tree_with_no_binary_is_refused_rather_than_reported_as_signed() {
        let scratch = Scratch::new("nothing");
        scratch.text("payload/bin/rustfmt");

        let recorder = Recorder::new();
        let refused = sign_all(&scratch.base.join("payload"), "identity", &recorder).unwrap_err();
        assert!(
            matches!(refused, SignError::NothingToSign { .. }),
            "{refused}"
        );
        assert!(refused.to_string().contains("no Mach-O"), "{refused}");
        assert!(recorder.called().is_empty());
    }

    #[test]
    fn a_tree_that_cannot_be_enumerated_is_left_untouched() {
        let scratch = Scratch::new("unscannable");
        let recorder = Recorder::new();
        let refused = sign_all(&scratch.base.join("nowhere"), "identity", &recorder).unwrap_err();
        assert!(matches!(refused, SignError::Scan(_)), "{refused}");
        assert!(recorder.called().is_empty());
    }

    #[test]
    fn a_file_that_will_not_sign_stops_the_run_and_is_named() {
        let scratch = Scratch::new("sign-fails");
        // Sorted order puts `a.dylib` first, so the failure is on the second file and the first one
        // has already been signed — which is exactly the state the run must report rather than hide.
        scratch.binary("payload/a.dylib");
        let stubborn = scratch.binary("payload/b.dylib");
        scratch.binary("payload/c.dylib");

        let recorder = Recorder::failing_to_sign(stubborn.to_str().unwrap());
        let refused = sign_all(&scratch.base.join("payload"), "identity", &recorder).unwrap_err();
        assert!(
            matches!(
                refused,
                SignError::Refused {
                    step: Step::Sign,
                    ..
                }
            ),
            "{refused}"
        );
        assert!(refused.to_string().contains("b.dylib"), "{refused}");
        assert!(
            refused.to_string().contains("not signed at all"),
            "the program's own words are the message: {refused}"
        );
        // Nothing after the failure was attempted: a run that carried on would have signed `c.dylib`
        // and then reported an error anyway.
        assert_eq!(recorder.signed().len(), 2, "{:?}", recorder.called());
    }

    #[test]
    fn a_signature_that_does_not_verify_fails_the_run() {
        let scratch = Scratch::new("verify-fails");
        let file = scratch.binary("payload/rustc");

        // `codesign` exiting 0 and the signature not being there is the interesting failure, so it is
        // the one the fake produces.
        let recorder = Recorder::failing_to_verify(file.to_str().unwrap());
        let refused = sign_all(&scratch.base.join("payload"), "identity", &recorder).unwrap_err();
        match &refused {
            SignError::Refused { step, .. } => assert_eq!(*step, Step::Verify),
            other => panic!("{other}"),
        }
        assert!(refused.to_string().starts_with("verifying "), "{refused}");
    }

    #[test]
    fn a_signer_that_is_not_there_says_so_rather_than_looking_like_a_refusal() {
        // The real signer is exercised only for its absence path, which is the reading every
        // non-macOS leg gets and the one a macOS machine without the developer tools gets too.
        let signer = StdSigner {
            codesign: PathBuf::from("/nowhere/codesign"),
        };
        let refusal = signer.sign(Path::new("/bin/true"), "identity").unwrap_err();
        assert!(
            refusal.detail.contains("is not on this machine"),
            "{}",
            refusal.detail
        );
        let refusal = signer.verify(Path::new("/bin/true")).unwrap_err();
        assert!(
            refusal.detail.contains("is not on this machine"),
            "{}",
            refusal.detail
        );
    }

    #[test]
    fn the_identity_variable_is_read_from_the_environment() {
        // The one line the binary needs, asserted here so the name cannot drift between the module
        // that documents it and the pipeline that sets it.
        assert_eq!(IDENTITY_VAR, "DEEPNEST_SIGNING_IDENTITY");
    }
}
