//! Where a widening request waits for the person in front of the page.
//!
//! `crates/sandbox` decides what a widening *is*, and `crates/agent` asks the question and turns
//! the answer into a policy for one call. Neither of them knows how a question reaches a person:
//! the loop must not know about the page, and the page must not be inside the loop. This module is
//! the ground between them, and it is one blocking wait.
//!
//! - **The turn waits; the page polls.** A turn that wants more room registers a request and waits.
//!   The page asks "anything?" on its own connection, shows the request, and posts the answer. That
//!   shape and not a return-then-resend one, because a grant is bound to the call that asked: if
//!   the turn ended and the answer arrived later, the call would be gone and the approval would
//!   belong to whatever came next — which is exactly the "session default moved" failure
//!   `crates/sandbox` refuses. Servers here serve one connection per thread, so a waiting turn
//!   holds its own thread and nobody else's.
//! - **A wait that ends.** [`ANSWER_TIMEOUT`] bounds it, and the answer at the far end is
//!   [`Answer::Unanswered`]: not a refusal, because nobody said no. A turn that could wait forever
//!   is a turn that holds a connection after the user closed the tab.
//! - **One request per app at a time.** A page cannot show two questions and a person cannot answer
//!   them at once, so a second request from the same app replaces the first — and the first is told
//!   so, rather than waiting out its whole lifetime for an answer that will never come.
//! - **The answer carries the request back.** [`Requests::answer`] returns what it answered, so the
//!   route can say what was approved. The audit line is the pair of them, and a page that reported
//!   "approved" without naming what would be a record of nothing.
//!
//! What is deliberately absent: any storage. A request lives in memory for the length of one call,
//! because an approval that outlived the call it was given for would be a wider policy nobody asked
//! for any more. Nothing here is written down, and a restart loses nothing that should have lasted.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use deepnest_agent::{Answer, Approver};
use deepnest_sandbox::EscalationRequest;

/// The verb this module answers, under `POST /api/apps/<slug>/`.
pub const ACTION: &str = "escalation";

/// How long a request waits for an answer.
///
/// Two minutes: long enough for a person to read a sentence and decide, short enough that a turn
/// nobody is watching gives its thread back before the browser has given up on the page. The far
/// end is named rather than silent — the model is told nobody answered, and says so to the user.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

/// The requests waiting for an answer, and the wait itself.
///
/// `Clone` shares the queue: the router holds one and each turn's approver holds a clone, and a
/// request registered by one has to be the one the other answers.
#[derive(Debug, Clone)]
pub struct Requests {
    timeout: Duration,
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
    /// Signalled when a request is registered or answered. A condition variable rather than a
    /// channel, because a waiter is waiting on *its* entry changing and a channel would have to be
    /// routed back to it by hand.
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// One entry per app. The key is the slug, which is what a page's route carries.
    waiting: HashMap<String, Waiting>,
    /// Serial numbers of waiters a newer request pushed out.
    ///
    /// A set rather than a flag on the entry, because the entry is what was replaced: by the time
    /// the older waiter wakes, the app's single slot holds its successor, and a flag written there
    /// would have been overwritten by the very insert that set it.
    replaced: HashSet<u64>,
    /// Handed out per registration, so a waiter can tell its own entry from a successor's.
    serial: u64,
}

#[derive(Debug)]
struct Waiting {
    serial: u64,
    request: EscalationRequest,
    resolution: Option<Resolution>,
}

/// What the person in front of the page said.
///
/// Public because it is the answer a route carries: the router reads `{"approve": true}` out of a
/// body and this is what it becomes, so the vocabulary is this module's rather than the route's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Approved,
    Denied(String),
}

impl Default for Requests {
    fn default() -> Self {
        Self::new()
    }
}

impl Requests {
    /// Requests that honour [`ANSWER_TIMEOUT`].
    pub fn new() -> Self {
        Self::with_timeout(ANSWER_TIMEOUT)
    }

    /// Requests with a wait the caller chooses.
    ///
    /// A constructor rather than a test hook, for the reason `Grants::with_ttl` is one: a clock is
    /// not something a test can wind forward, so the only way to exercise the far end of a wait is
    /// to give it a short one.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
            }),
        }
    }

    /// The approver one app's agent asks. `Arc<dyn Approver>` because the loop holds a trait
    /// object, and because a test can attach one of its own without a page in sight.
    pub fn approver(&self, slug: &str) -> Arc<dyn Approver> {
        Arc::new(Asks {
            requests: self.clone(),
            slug: slug.to_string(),
        })
    }

    /// The request waiting for `slug`, if one is.
    pub fn pending(&self, slug: &str) -> Option<EscalationRequest> {
        let state = self.inner.state.lock().ok()?;
        state
            .waiting
            .get(slug)
            .map(|waiting| waiting.request.clone())
    }

    /// Answer `slug`'s waiting request. `Err` when nothing is waiting, which is a different reading
    /// from a `404` about the app and a different one again from a refusal.
    pub fn answer(&self, slug: &str, resolution: Resolution) -> Result<EscalationRequest, String> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| "the queue of requests is in an unknown state".to_string())?;
        let Some(waiting) = state.waiting.get_mut(slug) else {
            return Err(format!(
                "no one is asking `{slug}` for anything right now, so there is nothing to answer"
            ));
        };
        // A second answer to the same request is a page that sent twice, not a second question:
        // the first answer wins and the second is told there was nothing left to answer.
        if waiting.resolution.is_some() {
            return Err(format!("`{slug}`'s request has already been answered"));
        }
        waiting.resolution = Some(resolution);
        let answered = waiting.request.clone();
        // Woken while still holding the lock. `notify_all` on a `Condvar` does not need the lock
        // released first, and releasing it here would let a second answer through between the
        // check above and this line.
        self.inner.changed.notify_all();
        drop(state);
        Ok(answered)
    }

    /// Register `request` for `slug` and wait for its answer.
    ///
    /// Blocking, deliberately: this *is* the wait, and the caller is a turn that has nothing else to
    /// do until somebody says yes or no.
    fn block(&self, slug: &str, request: &EscalationRequest) -> Answer {
        let Ok(mut state) = self.inner.state.lock() else {
            return Answer::Unanswered {
                reason: "the queue of requests is in an unknown state".to_string(),
            };
        };

        state.serial += 1;
        let serial = state.serial;
        // Replacing an earlier waiter rather than refusing the newcomer: the newer request is about
        // the call that is running now, and the older one's page is stale by definition.
        let superseded = state
            .waiting
            .get(slug)
            .filter(|previous| previous.resolution.is_none())
            .map(|previous| previous.serial);
        if let Some(superseded) = superseded {
            state.replaced.insert(superseded);
        }
        state.waiting.insert(
            slug.to_string(),
            Waiting {
                serial,
                request: request.clone(),
                resolution: None,
            },
        );
        self.inner.changed.notify_all();

        let deadline = Instant::now() + self.timeout;
        loop {
            // Superseded, and told so rather than left to wait out a lifetime for an answer that
            // will never come. Checked before the entry is read, because the entry is no longer
            // this waiter's.
            if state.replaced.remove(&serial) {
                return Answer::Unanswered {
                    reason: "a newer request from this app took its place before anyone answered"
                        .to_string(),
                };
            }
            // Read the resolution out from under the borrow before removing the entry: a replaced
            // waiter must not remove the entry that replaced it, and the serial is what tells the
            // two apart.
            let mine = state
                .waiting
                .get(slug)
                .filter(|waiting| waiting.serial == serial)
                .and_then(|waiting| waiting.resolution.clone());
            if let Some(resolution) = mine {
                state.waiting.remove(slug);
                return match resolution {
                    Resolution::Approved => Answer::Approved,
                    Resolution::Denied(reason) => Answer::Denied { reason },
                };
            }
            // The entry is gone without an answer — only possible if something removed it, and the
            // only remover is the answer path above. Reported rather than looped on.
            if !state.waiting.contains_key(slug) {
                return Answer::Unanswered {
                    reason: "the request left the queue without an answer".to_string(),
                };
            }

            let now = Instant::now();
            if now >= deadline {
                // Only this waiter's entry, and only if it is still this waiter's.
                let still_mine = state
                    .waiting
                    .get(slug)
                    .is_some_and(|waiting| waiting.serial == serial);
                if still_mine {
                    state.waiting.remove(slug);
                }
                return Answer::Unanswered {
                    reason: format!("nobody answered within {} seconds", self.timeout.as_secs()),
                };
            }
            let waited = self.inner.changed.wait_timeout(state, deadline - now);
            match waited {
                Ok((next, _)) => state = next,
                // A poisoned lock leaves no answer anybody can trust.
                Err(_) => {
                    return Answer::Unanswered {
                        reason: "the queue of requests is in an unknown state".to_string(),
                    }
                }
            }
        }
    }
}

/// One app's approver: the agent's question, put to whichever page is open on that app.
struct Asks {
    requests: Requests,
    slug: String,
}

impl Approver for Asks {
    fn ask(&self, request: &EscalationRequest) -> Answer {
        self.requests.block(&self.slug, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepnest_sandbox::Mode;
    use std::sync::mpsc;
    use std::thread;

    fn a_request() -> EscalationRequest {
        EscalationRequest::new(
            Mode::WorkspaceWrite,
            Mode::DangerFullAccess,
            "the build writes outside the app",
        )
    }

    /// Ask on another thread, so the test can answer the way a page does.
    fn asking(requests: &Requests, slug: &str) -> (mpsc::Receiver<Answer>, thread::JoinHandle<()>) {
        let approver = requests.approver(slug);
        let (sent, received) = mpsc::channel();
        let handle = thread::spawn(move || {
            let _ = sent.send(approver.ask(&a_request()));
        });
        (received, handle)
    }

    /// Wait for the request to appear, so the test does not race the thread it just started.
    fn until_pending(requests: &Requests, slug: &str) -> EscalationRequest {
        for _ in 0..2000 {
            if let Some(request) = requests.pending(slug) {
                return request;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("the request never appeared");
    }

    #[test]
    fn an_approved_request_wakes_the_turn_that_asked() {
        let requests = Requests::new();
        let (received, handle) = asking(&requests, "notes");
        let pending = until_pending(&requests, "notes");
        assert_eq!(pending.to(), Mode::DangerFullAccess);

        let answered = requests
            .answer("notes", Resolution::Approved)
            .expect("something was waiting");
        assert_eq!(answered.to(), Mode::DangerFullAccess);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            Answer::Approved
        );
        handle.join().unwrap();
        // And the queue is empty again: an answer is not a standing permission.
        assert!(requests.pending("notes").is_none());
    }

    #[test]
    fn a_refusal_carries_the_hosts_words_back_to_the_model() {
        let requests = Requests::new();
        let (received, handle) = asking(&requests, "notes");
        until_pending(&requests, "notes");
        requests
            .answer(
                "notes",
                Resolution::Denied("not for a note app".to_string()),
            )
            .unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            Answer::Denied {
                reason: "not for a note app".to_string()
            }
        );
        handle.join().unwrap();
    }

    #[test]
    fn nobody_answering_is_not_a_refusal_and_ends_the_wait() {
        // The far end of the wait, with a timeout short enough to exercise it: a turn that waited
        // forever would hold its connection after the user closed the tab.
        let requests = Requests::with_timeout(Duration::from_millis(20));
        let answer = requests.approver("notes").ask(&a_request());
        match answer {
            Answer::Unanswered { reason } => {
                assert!(reason.contains("nobody answered"), "{reason}")
            }
            other => panic!("expected an unanswered request, got {other:?}"),
        }
        assert!(requests.pending("notes").is_none(), "a wait ends cleanly");
    }

    #[test]
    fn answering_nothing_is_reported_rather_than_pretended() {
        let requests = Requests::new();
        let error = requests
            .answer("notes", Resolution::Approved)
            .expect_err("nothing was waiting");
        assert!(error.contains("nothing to answer"), "{error}");
    }

    #[test]
    fn one_request_is_answered_once() {
        let requests = Requests::new();
        let (received, handle) = asking(&requests, "notes");
        until_pending(&requests, "notes");
        requests.answer("notes", Resolution::Approved).unwrap();
        // A page that posted twice — a double click — must not read as a second question.
        let second = requests.answer("notes", Resolution::Denied("again".to_string()));
        assert!(second.is_err(), "{second:?}");
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            Answer::Approved
        );
        handle.join().unwrap();
    }

    #[test]
    fn a_newer_request_replaces_an_older_one_instead_of_leaving_it_to_expire() {
        let requests = Requests::with_timeout(Duration::from_secs(30));
        let (first_received, first) = asking(&requests, "notes");
        until_pending(&requests, "notes");
        let (second_received, second) = asking(&requests, "notes");

        // The older waiter is told it was replaced, rather than waiting out thirty seconds.
        let replaced = first_received.recv_timeout(Duration::from_secs(5)).unwrap();
        match replaced {
            Answer::Unanswered { reason } => assert!(reason.contains("took its place"), "{reason}"),
            other => panic!("the older request should have been replaced, got {other:?}"),
        }
        first.join().unwrap();

        // And the newer one is the one on the page, and the one answered.
        until_pending(&requests, "notes");
        requests.answer("notes", Resolution::Approved).unwrap();
        assert_eq!(
            second_received
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            Answer::Approved
        );
        second.join().unwrap();
    }

    #[test]
    fn two_apps_wait_on_two_questions_at_once() {
        // One page per app: a question from one app must not hide or answer another's.
        let requests = Requests::new();
        let (notes, notes_handle) = asking(&requests, "notes");
        let (budget, budget_handle) = asking(&requests, "budget");
        until_pending(&requests, "notes");
        until_pending(&requests, "budget");
        requests.answer("budget", Resolution::Approved).unwrap();
        assert_eq!(
            budget.recv_timeout(Duration::from_secs(5)).unwrap(),
            Answer::Approved
        );
        assert!(requests.pending("notes").is_some(), "notes still waits");
        requests.answer("notes", Resolution::Approved).unwrap();
        assert_eq!(
            notes.recv_timeout(Duration::from_secs(5)).unwrap(),
            Answer::Approved
        );
        notes_handle.join().unwrap();
        budget_handle.join().unwrap();
    }
}
