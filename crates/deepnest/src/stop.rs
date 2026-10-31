//! Being asked to stop — and what a stop has to run down.
//!
//! A deepnest process does not end because someone clicked quit: it ends because the operating
//! system says so. Ctrl-C in the terminal it was started from, `kill` from a script, a session
//! manager's `SIGTERM`, and closing the console window on Windows are four spellings of one
//! request, and only the platform knows which of them arrived. This module is where they arrive,
//! and it is also where the order of the work a stop does is fixed ([`run_down`]).
//!
//! **The handler does one thing: store to a flag.** It runs on the platform's own thread — on unix
//! in the signal handler, on Windows on a thread the console starts — which may be any thread at
//! all, including one holding a lock this process needs. Anything beyond an atomic store is a
//! deadlock waiting for a reason to happen. What the request *means* is decided by the thread that
//! waited for it, which is ordinary code: it may take a lock, join a thread, and wait as long as
//! the work takes.
//!
//! **The order is the design: the listener first, then the apps.** Requests are what start apps, so
//! a stop that killed the apps first would race the request that had just started one. Closing the
//! door before emptying the room is the only order in which "nothing this process started is still
//! running" is true once the process is gone. The listener's close is bounded (`server::Server`'s
//! shutdown), so a stream that is saying nothing cannot hold the exit open.
//!
//! **A stop is a success, not a failure.** The process was asked to end and did; `main` exits `0`
//! rather than reporting a request it obeyed as an error.
//!
//! What no handler can hear is `SIGKILL`: a deepnest killed that way still leaves its apps running,
//! and it is stated here rather than left to be discovered.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// How often the waiting thread looks at the flag.
///
/// The cost of a stop is this delay plus the work itself. Fifty milliseconds is below what anyone
/// notices after pressing Ctrl-C, and an idle deepnest pays a load-and-compare twenty times a
/// second — less than the accept loop already spends looking for a connection.
const POLL: Duration = Duration::from_millis(50);

/// The one thing a signal handler touches.
static ASKED: AtomicBool = AtomicBool::new(false);

/// Record that this process was asked to stop.
///
/// Signal-safe by construction: one atomic store, no allocation and no lock, which is the whole
/// requirement on a handler. Public because it is also how a test asks without sending a real
/// signal to the process running it.
pub fn ask() {
    ASKED.store(true, Ordering::SeqCst);
}

/// Whether this process has been asked to stop.
pub fn asked() -> bool {
    ASKED.load(Ordering::SeqCst)
}

/// Wait until a request has arrived.
pub fn wait() {
    wait_until(asked);
}

/// The wait itself, as a function of the question.
///
/// Taking the predicate rather than reading the flag is what makes the waiting testable: what has to
/// be right is that it neither returns early nor spins hot, and a test of that must not depend on
/// sending a real signal to the process running the test.
pub fn wait_until(asked: impl Fn() -> bool) {
    while !asked() {
        thread::sleep(POLL);
    }
}

/// Why the platform's handlers could not be installed.
#[derive(Debug)]
pub struct ListenError {
    detail: String,
}

impl std::fmt::Display for ListenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot listen for a stop request: {}", self.detail)
    }
}

impl std::error::Error for ListenError {}

/// Ask the platform to deliver stop requests here instead of ending the process.
///
/// One call covers all of them: on unix it installs `SIGINT`, `SIGTERM` and `SIGHUP` — the
/// `termination` feature of `ctrlc` is what adds the last two, and without it a `kill` would still
/// end the process with its apps running — and on Windows it registers a console control handler,
/// which is what both Ctrl-C and a console close arrive as.
///
/// Those three handlers written by hand are platform FFI this crate gains nothing from: the
/// interesting part is the sequence ([`run_down`]), not the four lines that record the request.
/// `socket2` is the same trade made next door for socket options, and the raw `winsock` call that
/// would need hand-written code is deferred for the same reason.
pub fn listen() -> Result<(), ListenError> {
    ctrlc::set_handler(ask).map_err(|error| ListenError {
        detail: error.to_string(),
    })
}

/// What a stop runs down, in the order [`run_down`] runs it down in.
///
/// A trait, for the same reason `Opener` and `Spawner` are: the thing that has to be right is the
/// sequence, and a test of a sequence should not need a listening socket and a process to kill.
pub trait Rundown {
    /// Stop taking connections, and wait until the listener thread is finished.
    fn stop_serving(&mut self);

    /// Stop every app this process started.
    fn stop_apps(&self);
}

/// Run a stop: the door first, then what it let in.
pub fn run_down(target: &mut dyn Rundown) {
    target.stop_serving();
    target.stop_apps();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Mutex};

    #[test]
    fn the_wait_returns_only_once_the_request_has_arrived() {
        let polls = AtomicUsize::new(0);
        wait_until(|| polls.fetch_add(1, Ordering::SeqCst) + 1 >= 3);
        assert!(
            polls.load(Ordering::SeqCst) >= 3,
            "the wait returned before the request it was waiting for"
        );
    }

    #[test]
    fn a_request_from_another_thread_is_what_ends_the_wait() {
        let asked = Arc::new(AtomicBool::new(false));
        let (began_tx, began) = mpsc::channel();
        let (ended_tx, ended) = mpsc::channel();
        let flag = Arc::clone(&asked);
        let waiter = thread::spawn(move || {
            began_tx.send(()).expect("report that the wait began");
            wait_until(|| flag.load(Ordering::SeqCst));
            ended_tx.send(()).expect("report that the wait ended");
        });
        began.recv().expect("the waiting thread started");

        // Long enough for several polls: a wait that returned here returned before anything asked
        // it to, which is the bug this test exists for.
        assert!(
            matches!(
                ended.recv_timeout(Duration::from_millis(150)),
                Err(RecvTimeoutError::Timeout)
            ),
            "the wait ended before any request arrived"
        );

        asked.store(true, Ordering::SeqCst);
        ended
            .recv_timeout(Duration::from_secs(5))
            .expect("the wait should end once a request arrives");
        waiter.join().expect("the waiting thread");
    }

    #[test]
    fn asking_is_recorded_where_the_wait_looks() {
        // The flag is one-way and process-wide, which is all a signal can offer: asking twice is
        // asking once.
        ask();
        ask();
        assert!(
            asked(),
            "the request a handler records is the one the wait reads"
        );
    }

    /// Records the calls a stop makes, in the order it makes them.
    struct Recorder(Mutex<Vec<&'static str>>);

    impl Rundown for Recorder {
        fn stop_serving(&mut self) {
            self.0.lock().unwrap().push("serving");
        }

        fn stop_apps(&self) {
            self.0.lock().unwrap().push("apps");
        }
    }

    #[test]
    fn the_door_closes_before_the_apps_are_stopped() {
        // Requests are what start apps, so the other order races a request that has just started
        // one.
        let mut recorder = Recorder(Mutex::new(Vec::new()));
        run_down(&mut recorder);
        assert_eq!(*recorder.0.lock().unwrap(), vec!["serving", "apps"]);
    }
}
