//! The three modes, and the one question they answer.

use std::fmt;

/// What a call may do to the file system.
///
/// The vocabulary is the host's (`host 19:03`), spelled as the configuration spells it, so
/// a mode read from a config file and a mode written in code cannot diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// Every write is refused, inside the writable set and outside it alike.
    ReadOnly,
    /// Writes land inside the writable set and nowhere else.
    WorkspaceWrite,
    /// No confinement. The one mode with no enforcer.
    DangerFullAccess,
}

impl Mode {
    /// The configuration spelling. One home, so a `match` on a config string and a `match`
    /// on this enum cannot disagree.
    pub const ALL: [Mode; 3] = [Mode::ReadOnly, Mode::WorkspaceWrite, Mode::DangerFullAccess];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::ReadOnly => "read-only",
            Mode::WorkspaceWrite => "workspace-write",
            Mode::DangerFullAccess => "danger-full-access",
        }
    }

    /// Parse the configuration spelling. `None` for anything else — a typo is refused, never
    /// rounded to a neighbouring mode, because the nearest neighbour of a typo may be the
    /// mode with no fence at all.
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        Self::ALL.into_iter().find(|mode| mode.as_str() == trimmed)
    }

    /// Whether a write anywhere may ever succeed under this mode. Not "may this path be
    /// written" — that is [`crate::Policy::is_writable`], which also asks about the roots and
    /// the protected set.
    pub fn allows_any_write(self) -> bool {
        !matches!(self, Mode::ReadOnly)
    }

    /// Whether this mode confines at all. `danger-full-access` is the only one that does not,
    /// and it is checked in exactly one place ([`crate::backend::SandboxBackend::confine`])
    /// rather than at each call site.
    pub fn is_confined(self) -> bool {
        !matches!(self, Mode::DangerFullAccess)
    }

    /// How wide the mode is. Used by escalation to answer "does this request widen?", and it
    /// is a total order over the three modes rather than a set of pairs, so a fourth mode
    /// would have to be placed rather than silently compared as equal.
    pub fn width(self) -> u8 {
        match self {
            Mode::ReadOnly => 0,
            Mode::WorkspaceWrite => 1,
            Mode::DangerFullAccess => 2,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_round_trips_through_its_configuration_spelling() {
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.as_str()), Some(mode), "{mode}");
        }
        assert_eq!(Mode::parse("  read-only "), Some(Mode::ReadOnly));
    }

    #[test]
    fn a_typo_is_refused_rather_than_rounded() {
        // "readonly" is one keystroke from a fence and one from none; guessing is the bug.
        assert_eq!(Mode::parse("readonly"), None);
        assert_eq!(Mode::parse("danger"), None);
        assert_eq!(Mode::parse(""), None);
    }

    #[test]
    fn only_danger_full_access_skips_confinement() {
        assert!(Mode::ReadOnly.is_confined());
        assert!(Mode::WorkspaceWrite.is_confined());
        assert!(!Mode::DangerFullAccess.is_confined());
        assert!(!Mode::ReadOnly.allows_any_write());
        assert!(Mode::WorkspaceWrite.allows_any_write());
    }

    #[test]
    fn width_orders_the_modes_by_how_much_they_permit() {
        assert!(Mode::ReadOnly.width() < Mode::WorkspaceWrite.width());
        assert!(Mode::WorkspaceWrite.width() < Mode::DangerFullAccess.width());
    }
}
