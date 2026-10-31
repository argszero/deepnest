//! `.state.json`: which installed skills deepnest may refresh, and which are the user's.
//!
//! A skill in `~/.deepnest/skills/` comes from one of two places, and the difference decides
//! whether an update may touch it. Deepnest **installed** it, so deepnest may replace it; or the user
//! put it there, and it is theirs — a copy they edited, a folder they were given, a skill that never
//! came from a catalog at all.
//!
//! The file records that difference as one field per skill: `managed`. Only `managed: true` may be
//! refreshed, and that is enforced by construction here rather than left to each caller to remember
//! — [`State::managed`] is the only list an updater is given, and there is no "all skills" iterator
//! to reach for by mistake.
//!
//! # Three decisions, each about not losing someone's file
//!
//! - **The flag defaults to `false`, and an unreadable file reads as empty.** A missing `managed`
//!   field is not an old file to be upgraded into manageability; it is a file this code does not
//!   know the provenance of. Refreshing on doubt overwrites work that exists nowhere else.
//! - **Unknown keys survive a round trip.** A future version may record something this one does not
//!   know; keeping it costs a map and losing it would make an older daemon silently delete what a
//!   newer one wrote.
//! - **A write is a temporary neighbour and a rename.** The update loop reads this file while a
//!   previous run may still be writing it, and a half-written state file that parses as empty means
//!   every managed skill silently stops being updatable.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic;

/// The file, inside the user's skills root.
pub const STATE_FILE: &str = ".state.json";

/// What is known about one installed skill.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    /// The version it was installed at, as the source spelled it. Absent when the source does not
    /// publish versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// When it was installed, as the caller's own stamp. This crate does not read the clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    /// When it was last refreshed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Whether deepnest put this file here and may replace it.
    ///
    /// `default` is load-bearing: a state file written before this field existed, or by hand without
    /// it, must not read as managed. See the module doc.
    #[serde(default)]
    pub managed: bool,
    /// Anything a future version recorded that this one does not know about.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Installed {
    /// A skill deepnest installed, so it may refresh it.
    pub fn managed_at(version: Option<String>, at: &str) -> Self {
        Self {
            version,
            installed_at: Some(at.to_string()),
            updated_at: None,
            managed: true,
            extra: BTreeMap::new(),
        }
    }

    /// A skill the user brought, which deepnest must never touch.
    ///
    /// Recorded at all so the skill shows up in the state file with its provenance rather than by
    /// being absent and indistinguishable from a name nobody ever wrote down.
    pub fn unmanaged() -> Self {
        Self::default()
    }
}

/// The state file, read or built up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct State {
    entries: BTreeMap<String, Installed>,
}

impl State {
    /// An empty state: nothing installed, nothing managed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the state file at `path`, treating anything unreadable as empty.
    ///
    /// Empty is the safe reading in both directions: no skill is managed, so the updater touches
    /// nothing, and the next write replaces the file with what is actually known.
    pub fn read(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Write the state file at `path`, whole or not at all.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let mut text = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        text.push('\n');
        atomic::write(path, &text)
    }

    /// The skills an updater may refresh — and the only list it is given.
    pub fn managed(&self) -> impl Iterator<Item = (&str, &Installed)> {
        self.entries
            .iter()
            .filter(|(_, installed)| installed.managed)
            .map(|(name, installed)| (name.as_str(), installed))
    }

    /// Whether this one skill may be refreshed.
    pub fn may_refresh(&self, name: &str) -> bool {
        self.entries.get(name).is_some_and(|i| i.managed)
    }

    /// What is known about one skill.
    pub fn get(&self, name: &str) -> Option<&Installed> {
        self.entries.get(name)
    }

    /// How many skills the file knows about, managed or not.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the file knows about no skills at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record that deepnest installed a skill.
    pub fn record_install(&mut self, name: &str, version: Option<String>, at: &str) {
        self.entries
            .insert(name.to_string(), Installed::managed_at(version, at));
    }

    /// Record a refresh of a skill that was already managed.
    ///
    /// Returns whether it happened: refreshing something not marked managed is refused rather than
    /// silently promoting it, because a refresh that overwrote the user's file and then recorded
    /// itself as legitimate would be the one bug this file exists to prevent.
    pub fn record_update(&mut self, name: &str, version: Option<String>, at: &str) -> bool {
        let Some(installed) = self.entries.get_mut(name) else {
            return false;
        };
        if !installed.managed {
            return false;
        }
        installed.version = version;
        installed.updated_at = Some(at.to_string());
        true
    }

    /// Record a skill the user brought, so its provenance is on file and it is never refreshed.
    pub fn record_user_skill(&mut self, name: &str) {
        self.entries
            .insert(name.to_string(), Installed::unmanaged());
    }

    /// Forget a skill entirely — used when an install fails and its half-written file is removed.
    pub fn forget(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }
}

/// Where the state file lives in a given skills root.
pub fn path(root: &Path) -> PathBuf {
    root.join(STATE_FILE)
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
                std::env::temp_dir().join(format!("deepnest-state-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn root(&self) -> PathBuf {
            self.base.join("skills")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_state_file_round_trips_through_disk() {
        let scratch = Scratch::new("roundtrip");
        let mut state = State::new();
        state.record_install(
            "browser-harness",
            Some("0.1.8".into()),
            "2026-10-31T09:00:00Z",
        );
        state.record_user_skill("notes");
        state.write(&path(&scratch.root())).unwrap();

        let read = State::read(&path(&scratch.root()));
        assert_eq!(read, state);
        assert!(read.may_refresh("browser-harness"));
        assert!(!read.may_refresh("notes"));
    }

    #[test]
    fn only_managed_skills_are_offered_to_the_updater() {
        // The list an updater is handed cannot contain a skill the user brought, which is the
        // whole point of having a list rather than a `may_refresh` call per name.
        let mut state = State::new();
        state.record_install("harness", None, "t");
        state.record_user_skill("mine");
        let offered: Vec<&str> = state.managed().map(|(name, _)| name).collect();
        assert_eq!(offered, ["harness"]);
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn a_state_file_without_the_managed_field_reads_as_unmanaged() {
        // Written before the field existed, or by hand. Doubt is not permission.
        let scratch = Scratch::new("no-flag");
        fs::create_dir_all(scratch.root()).unwrap();
        fs::write(
            path(&scratch.root()),
            r#"{"notes": {"version": "1.0", "installed_at": "2026-01-01T00:00:00Z"}}"#,
        )
        .unwrap();

        let state = State::read(&path(&scratch.root()));
        assert!(!state.may_refresh("notes"));
        assert_eq!(state.managed().count(), 0);
        // And what it did say is kept, not discarded along with the flag.
        assert_eq!(state.get("notes").unwrap().version.as_deref(), Some("1.0"));
    }

    #[test]
    fn refreshing_a_skill_that_is_not_managed_is_refused() {
        let mut state = State::new();
        state.record_user_skill("mine");
        assert!(!state.record_update("mine", Some("2.0".into()), "t"));
        assert_eq!(state.get("mine").unwrap().version, None);
        assert_eq!(state.get("mine").unwrap().updated_at, None);
    }

    #[test]
    fn refreshing_a_skill_nobody_recorded_is_refused_rather_than_creating_it() {
        let mut state = State::new();
        assert!(!state.record_update("ghost", Some("1".into()), "t"));
        assert!(state.get("ghost").is_none());
    }

    #[test]
    fn a_managed_skill_records_its_refresh() {
        let mut state = State::new();
        state.record_install("harness", Some("0.1.8".into()), "t0");
        assert!(state.record_update("harness", Some("0.2.0".into()), "t1"));
        let installed = state.get("harness").unwrap();
        assert_eq!(installed.version.as_deref(), Some("0.2.0"));
        assert_eq!(installed.installed_at.as_deref(), Some("t0"));
        assert_eq!(installed.updated_at.as_deref(), Some("t1"));
    }

    #[test]
    fn a_key_this_version_does_not_know_survives_a_round_trip() {
        // A newer deepnest records something extra; an older one must not delete it on its next
        // write, or downgrading once would lose the record for good.
        let scratch = Scratch::new("extra");
        fs::create_dir_all(scratch.root()).unwrap();
        fs::write(
            path(&scratch.root()),
            r#"{"harness": {"managed": true, "version": "1", "pinned_by_user": true}}"#,
        )
        .unwrap();

        let state = State::read(&path(&scratch.root()));
        assert_eq!(
            state.get("harness").unwrap().extra.get("pinned_by_user"),
            Some(&serde_json::Value::Bool(true))
        );
        state.write(&path(&scratch.root())).unwrap();
        let text = fs::read_to_string(path(&scratch.root())).unwrap();
        assert!(text.contains("pinned_by_user"), "{text}");
    }

    #[test]
    fn an_unreadable_state_file_reads_as_empty_and_touches_nothing() {
        let scratch = Scratch::new("corrupt");
        fs::create_dir_all(scratch.root()).unwrap();
        fs::write(path(&scratch.root()), "{ this is not json").unwrap();

        let state = State::read(&path(&scratch.root()));
        assert!(state.is_empty());
        assert_eq!(state.managed().count(), 0);
        // The damaged file is still there: this reads, it does not repair behind the user's back.
        assert!(path(&scratch.root()).exists());
    }

    #[test]
    fn a_missing_state_file_is_empty_rather_than_absent() {
        let scratch = Scratch::new("missing");
        let state = State::read(&path(&scratch.root()));
        assert!(state.is_empty());
    }

    #[test]
    fn forgetting_a_skill_removes_it_from_both_lists() {
        let mut state = State::new();
        state.record_install("harness", None, "t");
        assert!(state.forget("harness"));
        assert!(!state.forget("harness"));
        assert!(state.is_empty());
        assert_eq!(state.managed().count(), 0);
    }

    #[test]
    fn a_write_leaves_no_scratch_file_behind() {
        let scratch = Scratch::new("atomic");
        let mut state = State::new();
        state.record_install("harness", None, "t");
        state.write(&path(&scratch.root())).unwrap();

        let leftovers: Vec<String> = fs::read_dir(scratch.root())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn the_file_is_json_a_person_can_read_and_patch() {
        // `.state.json` is a file users do open. Compact JSON would be legal and useless.
        let scratch = Scratch::new("pretty");
        let mut state = State::new();
        state.record_install("harness", Some("1.0".into()), "t");
        state.write(&path(&scratch.root())).unwrap();
        let text = fs::read_to_string(path(&scratch.root())).unwrap();
        assert!(text.contains("\n  \"harness\""), "{text}");
        assert!(text.ends_with('\n'));
    }
}
