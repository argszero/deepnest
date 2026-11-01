//! Stopping a turn that is already running.
//!
//! A turn is the longest thing this server does: it asks the model, runs the tools it asked for,
//! asks again, and the measured worst case so far is 112 seconds and 24 rounds. Until now the page
//! could only wait it out. `crates/agent` already had the flag — [`Cancellation`], checked before
//! every request and before every tool call in a batch — but [`crate::chat::answer`] created one
//! per request, handed it to the loop and dropped it, so nothing outside that request ever held a
//! handle to stop it, and `TurnOutcome::Cancelled` was a path no page could reach.
//!
//! This module is the holder, and what it holds is only flags. Three decisions are designs rather
//! than implementation detail, and each is stated where it is made:
//!
//! - **One registry, keyed by app, not one flag per app.** Two tabs of the same app can both send,
//!   and both turns already run. A single per-app flag would have the second send overwrite the
//!   first one's handle and leave a turn nothing could reach. What a person means by "stop" is
//!   "stop this app's work", so the registry keeps every turn of an app and the verb cancels all
//!   of them.
//! - **A ticket, retired on drop.** The turn registers with [`Running::begin`] and is removed by
//!   the `Drop` of what that returns, not by the route. The route cannot know when a turn ends, and
//!   a registry that outlives its turn reports a stop that stops nothing.
//! - **Cancelling is a request, not a kill.** The flag is read at the loop's own checkpoints, so a
//!   turn in the middle of a model call ends when that call returns — the reply from [`stop`]
//!   therefore counts the turns it *asked* to stop, and the dialog says so rather than promising
//!   that the work has already ended.
//!
//! [`Cancellation`]: deepnest_agent::Cancellation
//! [`stop`]: Running::stop

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use deepnest_agent::Cancellation;

/// The verb this module answers, under `POST /api/apps/<slug>/`.
pub const ACTION: &str = "cancel";

/// The turns this process is running, by app.
///
/// `Clone` shares the map rather than copying it, for the reason [`crate::grant::Grants`] is
/// `Clone`: a router is cloned per connection, and the turn a message started on one connection has
/// to be the one a cancel on another connection reaches.
#[derive(Debug, Clone, Default)]
pub struct Running {
    turns: Arc<Mutex<HashMap<String, Vec<Entry>>>>,
    /// Handed out once per turn so a turn can be retired by identity. Never reused, so a ticket
    /// that outlived its entry cannot retire somebody else's turn.
    next_id: Arc<AtomicU64>,
}

/// One running turn: its identity, and the flag that stops it.
#[derive(Debug, Clone)]
struct Entry {
    id: u64,
    cancel: Cancellation,
}

impl Running {
    /// A registry holding no turns.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a turn for `slug`, and get the ticket that retires it.
    ///
    /// The caller keeps the ticket for as long as the turn runs — that is the whole of the
    /// contract, and it is why the ticket is a value the turn owns rather than something the route
    /// has to remember to undo.
    pub fn begin(&self, slug: &str) -> Ticket {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let cancel = Cancellation::new();
        self.turns()
            .entry(slug.to_string())
            .or_default()
            .push(Entry {
                id,
                cancel: cancel.clone(),
            });
        Ticket {
            running: self.clone(),
            slug: slug.to_string(),
            id,
            cancel,
        }
    }

    /// Ask every turn of `slug` to stop, and answer how many were asked.
    ///
    /// Zero is not an error: a second press of the same button, or a stop that arrives after the
    /// answer did, finds nothing to stop and says so. The turns stay listed until they actually
    /// end, because that is when they stop being something to stop.
    pub fn stop(&self, slug: &str) -> usize {
        let turns = self.turns();
        let Some(entries) = turns.get(slug) else {
            return 0;
        };
        for entry in entries {
            entry.cancel.cancel();
        }
        entries.len()
    }

    /// How many turns of `slug` are running. Read by the tests that say so, and the reason a stop
    /// of an app nobody is using is distinguishable from a stop that failed.
    pub fn running(&self, slug: &str) -> usize {
        self.turns().get(slug).map_or(0, Vec::len)
    }

    /// Drop one turn by identity, and forget the app entirely when it had no others.
    fn retire(&self, slug: &str, id: u64) {
        let mut turns = self.turns();
        let now_empty = match turns.get_mut(slug) {
            Some(entries) => {
                entries.retain(|entry| entry.id != id);
                entries.is_empty()
            }
            None => false,
        };
        if now_empty {
            turns.remove(slug);
        }
    }

    /// The map, recovered from a poisoned lock rather than refused.
    ///
    /// What a panic inside the critical sections above could leave behind is a list of flags, not
    /// an invariant this code depends on — so taking the guard back is better than making one
    /// panicking turn take the capability away from every app for the rest of the process's life.
    fn turns(&self) -> MutexGuard<'_, HashMap<String, Vec<Entry>>> {
        self.turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One running turn's registration. Dropping it retires the turn.
#[derive(Debug)]
pub struct Ticket {
    running: Running,
    slug: String,
    id: u64,
    cancel: Cancellation,
}

impl Ticket {
    /// The flag this turn's loop is watching.
    ///
    /// A clone, because the loop borrows it for the whole turn and the ticket still has to be alive
    /// — and dropped — for the turn to leave the registry.
    pub fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.running.retire(&self.slug, self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_that_was_stopped_reads_as_cancelled() {
        let running = Running::new();
        let ticket = running.begin("notes");
        assert!(!ticket.cancellation().is_cancelled());
        assert_eq!(running.stop("notes"), 1);
        assert!(ticket.cancellation().is_cancelled());
    }

    #[test]
    fn a_stop_reaches_only_the_app_it_names() {
        // The measured failure this prevents: one page's stop ending another app's work. Turns are
        // registered per app, so the map is what keeps them apart — nothing else does.
        let running = Running::new();
        let notes = running.begin("notes");
        let budget = running.begin("budget");
        assert_eq!(running.stop("notes"), 1);
        assert!(notes.cancellation().is_cancelled());
        assert!(!budget.cancellation().is_cancelled());
    }

    #[test]
    fn two_turns_of_one_app_are_both_stopped() {
        // Two tabs, two sends, two turns — and one person pressing stop once. A per-app flag would
        // have the second send take the first one's handle with it.
        let running = Running::new();
        let first = running.begin("notes");
        let second = running.begin("notes");
        assert_eq!(running.stop("notes"), 2);
        assert!(first.cancellation().is_cancelled());
        assert!(second.cancellation().is_cancelled());
    }

    #[test]
    fn a_finished_turn_leaves_the_registry() {
        let running = Running::new();
        let ticket = running.begin("notes");
        assert_eq!(running.running("notes"), 1);
        drop(ticket);
        assert_eq!(running.running("notes"), 0);
        // And the app is gone with it, rather than left as an empty list nobody prunes.
        assert_eq!(running.stop("notes"), 0);
    }

    #[test]
    fn retiring_one_turn_leaves_its_neighbour_running() {
        let running = Running::new();
        let first = running.begin("notes");
        let second = running.begin("notes");
        drop(first);
        assert_eq!(running.running("notes"), 1);
        assert_eq!(running.stop("notes"), 1);
        assert!(second.cancellation().is_cancelled());
    }

    #[test]
    fn stopping_an_app_nothing_is_running_for_is_zero_and_not_an_error() {
        let running = Running::new();
        assert_eq!(running.stop("notes"), 0);
        // Still nothing afterwards: a stop is not a registration.
        assert_eq!(running.running("notes"), 0);
    }

    #[test]
    fn a_second_stop_is_idempotent() {
        // The flag is idempotent by construction; what this pins is that the count stays honest
        // rather than climbing with every press.
        let running = Running::new();
        let _ticket = running.begin("notes");
        assert_eq!(running.stop("notes"), 1);
        assert_eq!(running.stop("notes"), 1);
    }
}
