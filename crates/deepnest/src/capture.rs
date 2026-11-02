//! A picture of the screen, taken by this process rather than by the page.
//!
//! The dialog already takes pictures — a paste of one, or a file chosen with the attach button — and
//! both come from somewhere else. What was missing is the person who has nothing to paste from: no
//! `⇧⌘4`, no Snipaste, no Windows Snipping Tool. They can see the thing they mean on their own
//! screen and had no way to point at it.
//!
//! Three decisions are designs rather than implementation detail:
//!
//! - **The grab happens in this process, not in the browser.** A page can ask for the screen with
//!   `getDisplayMedia`, and paying for it means a browser picker for every single capture and a
//!   system prompt that no page can answer for the person in front of it. deepnest is a native
//!   process: the picture is taken by the same program that serves the page, and the page is handed
//!   the result. That is what turns a two-dialog ceremony into one press.
//! - **The platform tool is the recipe, and the recipes are data.** A rasteriser or an image crate
//!   would be a dependency and a C build in a binary that already carries its own toolchain; every
//!   desktop platform ships a program that writes a PNG of the whole screen (`screencapture`,
//!   `gnome-screenshot` and friends, `System.Drawing` through PowerShell). Each recipe is a program
//!   and the words it is given, so what a platform runs is pinned by a test that never runs it.
//! - **A missing program is the next recipe, not a failure.** Linux has no single answer — GNOME,
//!   KDE, ImageMagick and wlroots each ship a different one — so the list is tried in order and
//!   `NotFound` moves on. What ends the search is a recipe that ran and failed, because that one
//!   exists and has something to say.
//!
//! What this deliberately is **not**: a tool the agent may call. `crates/tools` states that the
//! browser is absent from the tool surface because driving one and taking screenshots are a skill —
//! that is about what an *agent* may do to the machine it is changing. This is a button for the
//! *person*, in the box where they type, and it hands the result to the same attachment path a
//! pasted picture already travels. Two decisions, two places, and neither contradicts the other.
//!
//! What it also cannot do, stated where it is implemented so a later reader does not infer it from
//! an absence: **it takes the whole screen, not a window the person picked**, and the file it writes
//! goes into the deepnest home and is deleted as soon as it has been read.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// The verb this module answers, under `POST /api/apps/<slug>/`.
pub const ACTION: &str = "capture";

/// Where the file's path goes in a recipe's words.
///
/// A placeholder rather than a position, because the platforms disagree about where the output
/// belongs: macOS and GNOME put it in an option's value, ImageMagick takes it last, and PowerShell
/// needs it inside the script it is given.
pub const PATH: &str = "{path}";

/// One program, and the words it is given to write a PNG of the whole screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    pub program: &'static str,
    pub args: &'static [&'static str],
    /// What to add when this program is installed and refuses anyway.
    ///
    /// The one thing the program's own message cannot say: where the platform keeps the
    /// permission. `screencapture` answers a refusal on macOS with a sentence about creating an
    /// image, which is true and tells nobody that the answer is a switch in System Settings — and
    /// a person whose first press fails with no way forward is a person who concludes the button
    /// does not work.
    pub refused_hint: Option<&'static str>,
}

impl Recipe {
    /// The words with `path` in place of [`PATH`].
    ///
    /// An argument that names the placeholder twice keeps both: nothing here counts occurrences, so
    /// a program that wants the path in two places gets it in both.
    pub fn argv(&self, path: &Path) -> Vec<String> {
        let path = path.display().to_string();
        self.args
            .iter()
            .map(|arg| arg.replace(PATH, &path))
            .collect()
    }

    /// Whether this recipe's words still name a file to write.
    ///
    /// Test-only on purpose: nothing on the capture path asks this, and the test that pins "every
    /// recipe says where to write" is the whole of its use. A method kept alive for a reader is a
    /// method that compiles into a binary nothing calls.
    #[cfg(test)]
    fn writes_a_file(&self) -> bool {
        self.args.iter().any(|arg| arg.contains(PATH))
    }
}

/// macOS: one program, no picker, no sound.
///
/// `-x` is silence — a camera shutter on every press would be the machine answering a question
/// nobody asked. The permission this needs is the screen-recording grant, which is the same one
/// every screenshot program on the platform needs and is granted once, to deepnest, rather than per
/// capture.
const MACOS: &[Recipe] = &[Recipe {
    program: "screencapture",
    args: &["-x", "-t", "png", PATH],
    refused_hint: Some(
        " — if macOS is refusing the screen, grant deepnest the Screen Recording permission in \
         System Settings → Privacy & Security → Screen Recording, then press Screenshot again",
    ),
}];

/// Linux: four answers, tried in order.
///
/// There is no single program every distribution ships. GNOME's is
/// [`gnome-screenshot`](https://gitlab.gnome.org/GNOME/gnome-screenshot), KDE's is
/// [`spectacle`](https://apps.kde.org/spectacle/), ImageMagick's `import` is the one that is present
/// on a machine with nothing else, and `grim` is the wlroots Wayland answer where X11 ones fail.
/// `-b -n` is spectacle told to write without showing itself; `-fullscreen` is the region it should
/// take, because a rectangle picker of its own would be a second overlay beside ours.
const LINUX: &[Recipe] = &[
    Recipe {
        program: "gnome-screenshot",
        args: &["-f", PATH],
        // Lineage other than the module doc's: on Linux a screenshot needs no per-program grant,
        // so a refusal there is the display server's answer and the program already says it.
        refused_hint: None,
    },
    Recipe {
        program: "spectacle",
        args: &["-b", "-n", "-f", "-o", PATH],
        refused_hint: None,
    },
    Recipe {
        program: "import",
        args: &["-window", "root", PATH],
        refused_hint: None,
    },
    Recipe {
        program: "grim",
        args: &[PATH],
        refused_hint: None,
    },
];

/// Windows: the picture is taken by the framework the platform already has.
///
/// There is no command-line screenshot program in a stock Windows install, so the recipe is a
/// PowerShell script that draws the virtual screen — every monitor, in the coordinates Windows
/// reports — into a bitmap and saves it. `VirtualScreen` rather than the primary screen because a
/// person with two monitors may mean either, and the overlay crops in image pixels: a picture of
/// one monitor would put the other's contents out of reach behind the crop rectangle.
const WINDOWS_SCRIPT: &str = "Add-Type -AssemblyName System.Windows.Forms,System.Drawing;\
$b=[System.Windows.Forms.SystemInformation]::VirtualScreen;\
$bmp=New-Object System.Drawing.Bitmap $b.Width,$b.Height;\
$g=[System.Drawing.Graphics]::FromImage($bmp);\
$g.CopyFromScreen($b.X,$b.Y,0,0,$bmp.Size);\
$bmp.Save('{path}',[System.Drawing.Imaging.ImageFormat]::Png);\
$g.Dispose();$bmp.Dispose()";

const WINDOWS: &[Recipe] = &[Recipe {
    program: "powershell",
    args: &["-NoProfile", "-NonInteractive", "-Command", WINDOWS_SCRIPT],
    refused_hint: None,
}];

/// The recipes for `os`, named as [`std::env::consts::OS`] names it.
///
/// `None` for a platform deepnest has no answer for, which is a different report from every recipe
/// having failed: "this platform has no screenshot program" is not something trying again fixes.
pub fn recipes_for(os: &str) -> Option<&'static [Recipe]> {
    match os {
        "macos" => Some(MACOS),
        "linux" => Some(LINUX),
        "windows" => Some(WINDOWS),
        _ => None,
    }
}

/// Takes the picture, or says why it could not.
///
/// A trait rather than a function for the reason [`deepnest_packaging::Probe`] is one: the real
/// implementation runs a platform program, and a test of the route above it must not — on macOS that
/// program asks the person for the screen-recording grant the first time it runs, and a test suite
/// that raised that prompt would be a test suite that interrupts its own run.
pub trait Capturer: Send + Sync {
    /// The PNG bytes of the whole screen.
    fn capture(&self) -> Result<Vec<u8>, String>;
}

/// The real capturer: a platform program, writing into the deepnest home.
pub struct StdCapturer {
    /// Where the program writes. Inside the home rather than the system's temp directory, because
    /// what this program writes is deepnest's business and the home is the one tree it owns. The
    /// file is deleted as soon as it has been read, and a crash before that leaves it in a directory
    /// the next capture overwrites rather than in one nobody looks at.
    dir: PathBuf,
    /// The recipes for this platform, resolved once at construction rather than per capture.
    recipes: &'static [Recipe],
}

/// Names the file, so two captures that overlap cannot read each other's picture.
///
/// The process id separates two deepnest instances on one machine and the counter separates two
/// captures in this one; neither is a secret, and neither has to be — the file is deleted by the same
/// call that wrote it.
static NEXT: AtomicU64 = AtomicU64::new(0);

impl StdCapturer {
    /// A capturer that writes under `home`, on whatever platform this binary was built for.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            dir: home.into().join("run"),
            recipes: recipes_for(std::env::consts::OS).unwrap_or(&[]),
        }
    }

    /// The same, with recipes chosen by the caller — the seam a test on one platform uses to prove
    /// what another platform would run, without running it.
    pub fn with_recipes(home: impl Into<PathBuf>, recipes: &'static [Recipe]) -> Self {
        Self {
            dir: home.into().join("run"),
            recipes,
        }
    }
}

impl Capturer for StdCapturer {
    fn capture(&self) -> Result<Vec<u8>, String> {
        if self.recipes.is_empty() {
            return Err(format!(
                "deepnest does not know how to take a picture of the screen on {}",
                std::env::consts::OS
            ));
        }
        std::fs::create_dir_all(&self.dir).map_err(|error| {
            format!(
                "the screen cannot be captured: {} could not be created: {error}",
                self.dir.display()
            )
        })?;
        let path = self.dir.join(format!(
            "capture-{}-{}.png",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        // What the last recipe that exists said. A `NotFound` is not a failure — it is this
        // platform not having that program — so it is not recorded here.
        let mut refused: Option<String> = None;
        for recipe in self.recipes {
            match Command::new(recipe.program)
                .args(recipe.argv(&path))
                .output()
            {
                Ok(output) if output.status.success() => {
                    let png = std::fs::read(&path).map_err(|error| {
                        format!(
                            "{} wrote no picture at {}: {error}",
                            recipe.program,
                            path.display()
                        )
                    });
                    // Read first, removed second, and the removal is what the caller never sees: a
                    // file left behind by a failed read is the next capture's to overwrite.
                    let _ = std::fs::remove_file(&path);
                    let png = png?;
                    if png.is_empty() {
                        return Err(format!(
                            "{} wrote an empty picture — the screen may be locked",
                            recipe.program
                        ));
                    }
                    return Ok(png);
                }
                Ok(output) => refused = Some(said(recipe, &output)),
                // This platform does not have that program. The next recipe is the answer, not a
                // report: on Linux the first three may all be absent.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    refused = Some(format!("{} could not be started: {error}", recipe.program))
                }
            }
        }
        let _ = std::fs::remove_file(&path);
        Err(refused.unwrap_or_else(|| {
            format!(
                "the screen cannot be captured: none of {} is installed",
                self.recipes
                    .iter()
                    .map(|recipe| recipe.program)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }))
    }
}

/// What a recipe that ran and refused said about it, in one line.
///
/// The last line of stderr rather than the whole of it: these programs print usage and a hint at
/// full length, and the one line that names the problem is at the end.
fn said(recipe: &Recipe, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut sentence = if last.is_empty() {
        format!("{} exited {}", recipe.program, output.status)
    } else {
        format!("{} exited {}: {last}", recipe.program, output.status)
    };
    if let Some(hint) = recipe.refused_hint {
        sentence.push_str(hint);
    }
    sentence
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every platform, so a test can walk them without naming them one at a time.
    const PLATFORMS: [&str; 3] = ["macos", "linux", "windows"];

    #[test]
    fn each_platform_names_the_program_that_takes_the_picture() {
        // Pinned rather than described: these are the programs the README's system requirements
        // promise, and a `None` here is the difference between a button that works and one that
        // reports the platform is unsupported.
        let named: Vec<&str> = PLATFORMS
            .iter()
            .map(|os| recipes_for(os).unwrap_or_else(|| panic!("{os} has no recipe"))[0].program)
            .collect();
        assert_eq!(named, ["screencapture", "gnome-screenshot", "powershell"]);
    }

    #[test]
    fn every_recipe_puts_the_path_where_its_program_expects_it() {
        let path = Path::new("/tmp/home/run/capture-1-0.png");
        for os in PLATFORMS {
            for recipe in recipes_for(os).unwrap() {
                assert!(
                    recipe.writes_a_file(),
                    "{} on {os} never says where to write",
                    recipe.program
                );
                let argv = recipe.argv(path);
                assert!(
                    argv.iter()
                        .any(|arg| arg.contains("/tmp/home/run/capture-1-0.png")),
                    "{} on {os} was given no path: {argv:?}",
                    recipe.program
                );
                // A placeholder left in the words is a program told to write a file named `{path}`.
                for arg in &argv {
                    assert!(
                        !arg.contains(PATH),
                        "{} on {os} kept the placeholder: {arg}",
                        recipe.program
                    );
                }
            }
        }
    }

    #[test]
    fn macos_takes_the_whole_screen_without_a_sound() {
        // `-x` and nothing that opens a picker: the region is chosen in the page's overlay, and a
        // second picker in front of it would be the ceremony this module exists to remove.
        let recipe = recipes_for("macos").unwrap()[0];
        assert_eq!(recipe.program, "screencapture");
        assert_eq!(
            recipe.argv(Path::new("/x.png")),
            ["-x", "-t", "png", "/x.png"]
        );
    }

    #[test]
    fn linux_is_tried_in_order_because_one_distribution_ships_one_of_them() {
        let programs: Vec<&str> = recipes_for("linux")
            .unwrap()
            .iter()
            .map(|recipe| recipe.program)
            .collect();
        assert_eq!(
            programs,
            ["gnome-screenshot", "spectacle", "import", "grim"],
            "the Wayland answer must be last: on a machine that has both, an X11 program that fails \
             is a failure, and `grim` is the one that works there"
        );
    }

    #[test]
    fn windows_asks_for_every_monitor_rather_than_the_primary_one() {
        let recipe = recipes_for("windows").unwrap()[0];
        assert_eq!(recipe.program, "powershell");
        let argv = recipe.argv(Path::new(r"C:\x.png"));
        let script = argv.last().unwrap();
        assert!(script.contains("VirtualScreen"), "{script}");
        assert!(script.contains(r"C:\x.png"), "{script}");
        assert!(script.contains("Png"), "{script}");
    }

    #[test]
    fn only_macos_carries_a_hint_because_only_macos_has_that_switch() {
        // The hint exists for one failure: a program that is installed, that runs, and that the
        // platform then refuses. On macOS the answer is a permission deepnest cannot grant itself,
        // and the program's own message does not name it. A hint anywhere else would be advice
        // about a settings screen that does not exist.
        let macos = recipes_for("macos").unwrap()[0];
        let hint = macos
            .refused_hint
            .expect("macOS says nothing about its grant");
        assert!(hint.contains("Screen Recording"), "{hint}");
        assert!(hint.contains("System Settings"), "{hint}");
        for os in ["linux", "windows"] {
            for recipe in recipes_for(os).unwrap() {
                assert!(
                    recipe.refused_hint.is_none(),
                    "{} on {os} points at a settings screen that does not exist",
                    recipe.program
                );
            }
        }
    }

    #[test]
    fn a_platform_deepnest_has_no_answer_for_is_not_guessed() {
        // "there is no screenshot program here" is a different report from "the one there failed",
        // and a fallback invented for an unknown platform would be a program nobody tested.
        for os in ["freebsd", "android", "ios", ""] {
            assert!(recipes_for(os).is_none(), "{os} was given a recipe");
        }
    }

    #[test]
    fn a_program_that_is_not_installed_moves_on_to_the_next_recipe() {
        // The Linux list is four programs because no distribution ships all four. A missing one is
        // the next one's turn, and only when every one of them is missing is there nothing to say
        // but which programs were looked for.
        static MISSING: &[Recipe] = &[
            Recipe {
                program: "deepnest-no-such-program",
                args: &[PATH],
                refused_hint: None,
            },
            Recipe {
                program: "deepnest-also-absent",
                args: &[PATH],
                refused_hint: None,
            },
        ];
        let scratch =
            std::env::temp_dir().join(format!("deepnest-capture-missing-{}", std::process::id()));
        let error = StdCapturer::with_recipes(&scratch, MISSING)
            .capture()
            .unwrap_err();
        for name in ["deepnest-no-such-program", "deepnest-also-absent"] {
            assert!(error.contains(name), "{error}");
        }
        // Nothing is left behind by a search that found nothing — the `run` directory itself is
        // expected, the file the recipes would have written into it is not.
        let leftovers = std::fs::read_dir(scratch.join("run"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0, "a failed capture left a file behind");
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_ran_and_failed_is_the_report_rather_than_the_next_recipe() {
        // `false` exists everywhere this test compiles, ignores its arguments and exits 1 — the
        // shape of a screenshot program that is installed but refused. Its report must survive: a
        // loop that kept looking would answer "none of them is installed" about a machine where
        // one plainly is, and the reason it gave would be lost.
        static FAILING: &[Recipe] = &[Recipe {
            program: "false",
            args: &[PATH],
            refused_hint: None,
        }];
        let scratch =
            std::env::temp_dir().join(format!("deepnest-capture-failed-{}", std::process::id()));
        let error = StdCapturer::with_recipes(&scratch, FAILING)
            .capture()
            .unwrap_err();
        assert!(error.contains("false exited"), "{error}");
        assert!(!error.contains("is installed"), "{error}");
    }

    #[test]
    fn a_capture_with_no_recipe_says_which_platform_it_cannot_do() {
        let capturer =
            StdCapturer::with_recipes(std::env::temp_dir().join("deepnest-capture-test"), &[]);
        let error = capturer.capture().unwrap_err();
        assert!(error.contains(std::env::consts::OS), "{error}");
    }
}
