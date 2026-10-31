//! Asking a person for more room, from inside the call that needs it.
//!
//! `crates/sandbox` owns the whole of *what* a widening is: [`EscalationRequest`] is the value, its
//! [`validate`](EscalationRequest::validate) is the jump table, and [`Grant`](deepnest_sandbox::Grant)
//! is the thing an approval produces. None of that is re-decided here. What is missing from that
//! crate is a person: a `Grant` says "the host said yes", and the host is not inside the sandbox.
//!
//! So this module is the seam and nothing else — the shape of the question, and the shape of the
//! answer:
//!
//! - **The request rides in the call that needs the room.** `request_mode` and `justification` are
//!   two argument keys, read and removed by [`take_request`] before the tool sees anything. The
//!   alternatives were worse: a tool of its own would be a call that asks for room without naming
//!   what for, and a separate round trip would spend a model call to say something the vault of a
//!   call already said.
//! - **The loop asks, and the answer is a value.** [`Approver`] is a trait for the same reason
//!   [`Model`](crate::turn::Model) is: the loop is then exercised with no page, no socket and no
//!   person — a test answers in the same shape a browser does.
//! - **A refusal never reaches a person.** [`take_request`] only parses; the jump table is
//!   `validate`, and the loop calls it *before* asking. A host asked "widen workspace-write to
//!   read-only?" would be asked to approve a narrowing, and a host asked that often enough stops
//!   reading the questions.
//!
//! What this deliberately does **not** do: hold the widened policy, remember an approval, or let one
//! call's answer reach the next. The [`Grant`](deepnest_sandbox::Grant) is built for one call, moved
//! into that call's [`ToolContext`](deepnest_tools::ToolContext), and dropped with it. The session's
//! own policy is borrowed for the comparison and never written to — `Grant::approve` takes it by
//! reference and returns a *separate* [`Policy`](deepnest_sandbox::Policy), which is what makes
//! "the session default never moves" a property of the types rather than of the call sites.

use deepnest_sandbox::{EscalationRequest, Mode};
use serde_json::Value;

/// The argument key that asks for a wider mode, for this call only.
pub const MODE_KEY: &str = "request_mode";

/// The argument key that carries why. A request without one is refused by the jump table.
pub const JUSTIFICATION_KEY: &str = "justification";

/// What a person said.
///
/// Two ways to say no, kept apart because the model's next move differs: a `Denied` host has
/// considered this and does not want it, so asking again is worse than changing the approach;
/// an `Unanswered` request never reached anyone, so the answer is to tell the user rather than to
/// give up on the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The host approved the widening.
    Approved,
    /// The host read the request and refused it.
    Denied {
        /// Their words, if they gave any — shown to the model rather than summarised.
        reason: String,
    },
    /// Nobody answered: the request expired, was replaced, or there was no one to ask.
    Unanswered {
        /// Why there is no answer, in terms the model can act on.
        reason: String,
    },
}

/// Somewhere to send a request for a wider mode.
///
/// `&self` and object-safe, because the loop holds one per agent and a test holds one that answers
/// from a script. An implementation blocks, or answers at once — the loop does not care, and the
/// page is what decides which.
pub trait Approver: Send + Sync {
    /// Ask about `request`. Must return rather than wait forever: a turn that cannot be answered
    /// holds the connection it was sent on.
    fn ask(&self, request: &EscalationRequest) -> Answer;
}

/// Take the escalation out of a call's arguments.
///
/// Returns the arguments the tool should see — the two keys removed, so a tool that later reads a
/// key it did not expect cannot read this one — and the request, when there was one.
///
/// `from` is the session's own mode, which is what the request is measured against. It is read
/// here rather than inside the request because the request must not be able to claim a starting
/// point: a call that could say `from` would say `read-only` and widen twice.
pub fn take_request(
    arguments: &Value,
    from: Mode,
) -> Result<(Value, Option<EscalationRequest>), String> {
    let Some(object) = arguments.as_object() else {
        return Ok((arguments.clone(), None));
    };
    let Some(spelled) = object.get(MODE_KEY) else {
        return Ok((arguments.clone(), None));
    };

    let text = match spelled.as_str() {
        Some(text) => text,
        None => {
            return Err(format!(
                "`{MODE_KEY}` must be a string naming a mode, and it was {spelled}. The modes are \
                 read-only, workspace-write and danger-full-access. Nothing was run."
            ))
        }
    };
    let Some(to) = Mode::parse(text) else {
        return Err(format!(
            "`{text}` is not a mode, so `{MODE_KEY}` names nothing to widen to. The modes are \
             read-only, workspace-write and danger-full-access. Nothing was run."
        ));
    };

    // Absent and empty are the same request: the jump table refuses both, by name, and this is
    // where a missing key becomes the empty string rather than a second refusal path.
    let justification = object
        .get(JUSTIFICATION_KEY)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut stripped = object.clone();
    stripped.remove(MODE_KEY);
    stripped.remove(JUSTIFICATION_KEY);
    Ok((
        Value::Object(stripped),
        Some(EscalationRequest::new(from, to, justification)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_call_that_asks_for_nothing_is_untouched() {
        let arguments = json!({ "file_path": "a.txt", "content": "x" });
        let (kept, request) = take_request(&arguments, Mode::WorkspaceWrite).unwrap();
        assert_eq!(kept, arguments, "a call without a request is not rewritten");
        assert!(request.is_none());
    }

    #[test]
    fn the_request_is_read_and_the_tool_never_sees_it() {
        let (kept, request) = take_request(
            &json!({
                "file_path": "/etc/hosts",
                "content": "x",
                MODE_KEY: "danger-full-access",
                JUSTIFICATION_KEY: "  the app's build writes to ~/.cache  ",
            }),
            Mode::WorkspaceWrite,
        )
        .unwrap();
        let request = request.expect("there is a request");
        assert_eq!(request.from(), Mode::WorkspaceWrite, "the session's mode");
        assert_eq!(request.to(), Mode::DangerFullAccess);
        assert_eq!(
            request.justification(),
            "the app's build writes to ~/.cache"
        );
        assert_eq!(kept, json!({ "file_path": "/etc/hosts", "content": "x" }));
    }

    #[test]
    fn the_starting_mode_cannot_be_claimed_by_the_call() {
        // A request that could name its own `from` would say `read-only` and widen twice.
        let (_, request) = take_request(
            &json!({ MODE_KEY: "workspace-write", "from": "read-only" }),
            Mode::WorkspaceWrite,
        )
        .unwrap();
        let request = request.unwrap();
        assert_eq!(request.from(), Mode::WorkspaceWrite);
        assert_eq!(
            request.validate(),
            Err(deepnest_sandbox::EscalationRefusal::DoesNotWiden {
                from: Mode::WorkspaceWrite,
                to: Mode::WorkspaceWrite
            })
        );
    }

    #[test]
    fn a_mode_that_is_not_a_mode_is_refused_by_name() {
        let error = take_request(
            &json!({ MODE_KEY: "danger", JUSTIFICATION_KEY: "please" }),
            Mode::ReadOnly,
        )
        .unwrap_err();
        assert!(error.contains("`danger` is not a mode"), "{error}");
        assert!(
            error.contains("workspace-write"),
            "the refusal lists what would have worked: {error}"
        );
    }

    #[test]
    fn a_mode_that_is_not_a_string_is_refused_rather_than_read_as_one() {
        let error = take_request(&json!({ MODE_KEY: true }), Mode::ReadOnly).unwrap_err();
        assert!(error.contains("must be a string"), "{error}");
    }

    #[test]
    fn a_justification_that_is_not_a_string_becomes_the_empty_one_the_table_refuses() {
        let (_, request) = take_request(
            &json!({ MODE_KEY: "danger-full-access", JUSTIFICATION_KEY: 7 }),
            Mode::ReadOnly,
        )
        .unwrap();
        assert_eq!(
            request.unwrap().validate(),
            Err(deepnest_sandbox::EscalationRefusal::NoJustification {
                from: Mode::ReadOnly,
                to: Mode::DangerFullAccess
            })
        );
    }

    #[test]
    fn arguments_that_are_not_an_object_are_left_for_the_call_that_will_refuse_them() {
        let (kept, request) = take_request(&json!("not an object"), Mode::ReadOnly).unwrap();
        assert_eq!(kept, json!("not an object"));
        assert!(request.is_none());
    }
}
