//! An app is a directory, and this crate owns what that means.
//!
//! `~/.deepnest/apps/<slug>/` is one generated app: its source, its database, its build output and
//! its agent's conversation. The host fixed that shape (`host 18:55`, `host 20:58`) because it is
//! where users' data and users' URLs sit, so changing it later costs a migration. This crate is
//! the one place that reads it — [`Slug`] validates the name, [`Manifest`] marks a directory as an
//! app, [`list`] finds the ones that are there, and [`logs::tail`] reads what an app said when it
//! ran.
//!
//! Three decisions carry weight here and each is stated where it is made:
//!
//! - **The name is validated once, against both namespaces it lives in** ([`slug`]). It becomes a
//!   directory on three operating systems and a path segment in `/app/<slug>/`, and the union of
//!   those rules is stricter than either alone.
//! - **A directory is an app because a manifest says so**, not because of where it sits
//!   ([`manifest`]). A folder someone dropped into `apps/` is then distinguishable from an app
//!   whose manifest failed to parse — which is the difference between "nothing here" and "here is
//!   what is wrong with it".
//! - **Discovery reports what it could not use, by name** ([`list`]). Skipping silently would make
//!   an app that does not appear indistinguishable from an app that does not exist, and the user's
//!   only clue would be that the panel is emptier than they expected.
//!
//! [`process`] is the second: an app's port (allocated per start, never persisted, never visible),
//! the supervisor that starts it and waits until it **answers** rather than until it was spawned,
//! and stop/restart. It runs a binary that already exists and names the path it expected when one
//! does not.
//!
//! [`template`] and [`build`] are the two that produce that binary. Creating an app writes text and
//! touches no toolchain, so a creation that fails halfway leaves a directory that is not an app;
//! building it runs the steps in [`build::plan`] and reports which one said no. They are separate
//! because their failures are: one is "this directory is now wrong", the other is "this toolchain
//! said no".

pub mod build;
pub mod listing;
pub mod logs;
pub mod manifest;
pub mod process;
pub mod slug;
pub mod template;

use std::path::{Path, PathBuf};

pub use build::{build, plan, BuildError, Built, Outcome, Runner, StdRunner, Step, STEP_TIMEOUT};
pub use listing::{list, App, Listing, Skipped};
pub use logs::{build_log_path, log_path, tail, LOGS_DIR, MAX_TAIL_BYTES};
pub use manifest::{
    AppInfo, Manifest, ManifestError, CURRENT_TEMPLATE, FILE_NAME as MANIFEST_FILE,
};
pub use process::{
    allocate, run_command, Exit, Handle, Port, Prober, Spawner, Spec, StartError, State,
    StdSpawner, Supervisor, TcpProber, DEFAULT_READINESS, EXPECTED_BINARY, PORT_ENV,
};
pub use slug::{Slug, SlugError, MAX_CHARS as MAX_SLUG_CHARS};
pub use template::{create, Created, TemplateError, BINARY_NAME, FILES as TEMPLATE_FILES};

/// The directory under the deepnest home that holds every app.
pub const DIR_NAME: &str = "apps";

/// `~/.deepnest/apps` — where [`list`] is pointed.
pub fn apps_dir(home: &Path) -> PathBuf {
    home.join(DIR_NAME)
}
