//! Asking for more room: one hop, one call, approved by a person.
//!
//! The shape is the host's (`host 19:03`): the tool call carries the requested tier and a
//! justification, the host approves it on the page, and the **session default never moves**. A
//! model that could raise its own ceiling would have no ceiling, so the request is a value the
//! host answers, not an operation the agent performs.
//!
//! What "one hop" refuses, and why each refusal exists:
//!
//! - **A jump to the same mode**, or to a narrower one: it does not widen, so approving it
//!   would teach the agent that the host says yes to requests for nothing.
//! - **A jump from `danger-full-access`**: there is nowhere to go. The refusal says so rather
//!   than pretending something wider exists.
//! - **A second hop in one call.** The grant is bound to the call that asked; a call that
//!   wanted more than one widening asked the wrong question, and the host should hear the whole
//!   need rather than a staircase.

use crate::mode::Mode;
use crate::policy::{Policy, PolicyError};

/// A mode with the width it grants, so escalation can be written without knowing the order.
pub trait RankedMode {
    fn width(self) -> u8;
}

impl RankedMode for Mode {
    fn width(self) -> u8 {
        Mode::width(self)
    }
}

/// Why a request was refused before it ever reached the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationRefusal {
    /// The request does not widen: same mode, or narrower.
    DoesNotWiden { from: Mode, to: Mode },
    /// Nothing is wider than `danger-full-access`.
    NowhereToGo { from: Mode },
    /// A justification is what the host approves; an empty one is not a request.
    NoJustification { from: Mode, to: Mode },
    /// The app the policy names is not a usable directory name.
    InvalidApp(PolicyError),
}

impl std::fmt::Display for EscalationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EscalationRefusal::DoesNotWiden { from, to } => write!(
                f,
                "{to} does not widen {from}, so there is nothing for the host to approve"
            ),
            EscalationRefusal::NowhereToGo { from } => write!(
                f,
                "this call already runs at {from}, which is the widest mode there is"
            ),
            EscalationRefusal::NoJustification { from, to } => write!(
                f,
                "asking to widen {from} to {to} needs a justification the host can judge"
            ),
            EscalationRefusal::InvalidApp(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for EscalationRefusal {}

/// What the agent asks for: a wider mode for one call, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationRequest {
    from: Mode,
    to: Mode,
    justification: String,
}

impl EscalationRequest {
    pub fn new(from: Mode, to: Mode, justification: impl Into<String>) -> Self {
        Self {
            from,
            to,
            justification: justification.into(),
        }
    }

    /// Check the request against the rules, without asking anyone. A refusal here never
    /// reaches the host, which is the point: the host is asked only what a person can answer.
    pub fn validate(&self) -> Result<(), EscalationRefusal> {
        if self.from == Mode::DangerFullAccess {
            return Err(EscalationRefusal::NowhereToGo { from: self.from });
        }
        if self.to.width() <= self.from.width() {
            return Err(EscalationRefusal::DoesNotWiden {
                from: self.from,
                to: self.to,
            });
        }
        if self.justification.trim().is_empty() {
            return Err(EscalationRefusal::NoJustification {
                from: self.from,
                to: self.to,
            });
        }
        Ok(())
    }

    pub fn from(&self) -> Mode {
        self.from
    }

    pub fn to(&self) -> Mode {
        self.to
    }

    pub fn justification(&self) -> &str {
        self.justification.trim()
    }
}

/// An approved widening, valid for exactly one call.
///
/// The type is the guard: it can only be produced by [`Grant::approve`], it carries the policy
/// to use for that one call, and it is consumed by value. Nothing here can write back to the
/// session's own [`Policy`] — that one was moved into the call that resolved it, and a
/// `Grant` holds a *separate* value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    policy: Policy,
    requested: EscalationRequest,
}

impl Grant {
    /// The host said yes. The one constructor, so "approved" is a fact with a source.
    pub fn approve(
        request: EscalationRequest,
        session: &Policy,
    ) -> Result<Self, EscalationRefusal> {
        request.validate()?;
        Ok(Self {
            policy: session.with_mode(request.to()),
            requested: request,
        })
    }

    /// The policy for the approved call — wider, and still carrying the protected set.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// What was approved, for the audit line the page shows.
    pub fn requested(&self) -> &EscalationRequest {
        &self.requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(mode: Mode) -> Policy {
        Policy::from_roots(mode, "/tmp/home/apps/notes", "/tmp/home/toolchain", "/tmp")
            .with_protected("/tmp/home/deepnest.token")
    }

    #[test]
    fn read_only_may_jump_straight_to_full_access() {
        let request = EscalationRequest::new(
            Mode::ReadOnly,
            Mode::DangerFullAccess,
            "build the app needs to write outside its directory",
        );
        assert!(request.validate().is_ok());
        let grant = Grant::approve(request, &session(Mode::ReadOnly)).expect("approved");
        assert_eq!(grant.policy().mode(), Mode::DangerFullAccess);
    }

    #[test]
    fn full_access_has_nowhere_to_go() {
        let request =
            EscalationRequest::new(Mode::DangerFullAccess, Mode::DangerFullAccess, "more");
        assert_eq!(
            request.validate(),
            Err(EscalationRefusal::NowhereToGo {
                from: Mode::DangerFullAccess
            })
        );
    }

    #[test]
    fn a_request_that_does_not_widen_is_refused() {
        let same = EscalationRequest::new(Mode::WorkspaceWrite, Mode::WorkspaceWrite, "again");
        assert_eq!(
            same.validate(),
            Err(EscalationRefusal::DoesNotWiden {
                from: Mode::WorkspaceWrite,
                to: Mode::WorkspaceWrite
            })
        );
        let narrower = EscalationRequest::new(Mode::WorkspaceWrite, Mode::ReadOnly, "be careful");
        assert_eq!(
            narrower.validate(),
            Err(EscalationRefusal::DoesNotWiden {
                from: Mode::WorkspaceWrite,
                to: Mode::ReadOnly
            })
        );
        // From the widest mode the more useful answer is that there is nowhere to go, and it
        // is given before the width comparison for exactly that reason.
        let from_widest = EscalationRequest::new(Mode::DangerFullAccess, Mode::ReadOnly, "quiet");
        assert_eq!(
            from_widest.validate(),
            Err(EscalationRefusal::NowhereToGo {
                from: Mode::DangerFullAccess
            })
        );
    }

    #[test]
    fn a_blank_justification_is_not_a_request() {
        for blank in ["", "   ", "\n\t"] {
            let request = EscalationRequest::new(Mode::ReadOnly, Mode::WorkspaceWrite, blank);
            assert_eq!(
                request.validate(),
                Err(EscalationRefusal::NoJustification {
                    from: Mode::ReadOnly,
                    to: Mode::WorkspaceWrite
                })
            );
        }
    }

    #[test]
    fn the_session_policy_does_not_move_when_a_grant_is_issued() {
        let session_policy = session(Mode::ReadOnly);
        let grant = Grant::approve(
            EscalationRequest::new(Mode::ReadOnly, Mode::WorkspaceWrite, "write src/main.rs"),
            &session_policy,
        )
        .expect("approved");
        assert_eq!(
            session_policy.mode(),
            Mode::ReadOnly,
            "the session default never moves"
        );
        assert_eq!(grant.policy().mode(), Mode::WorkspaceWrite);
        // And the widening does not unlock the protected files.
        assert!(!grant.policy().is_writable("/tmp/home/deepnest.token"));
    }

    #[test]
    fn a_refused_request_produces_no_grant_at_all() {
        let refused = Grant::approve(
            EscalationRequest::new(Mode::ReadOnly, Mode::ReadOnly, "please"),
            &session(Mode::ReadOnly),
        );
        assert!(refused.is_err());
    }

    #[test]
    fn the_justification_is_trimmed_for_display() {
        let request =
            EscalationRequest::new(Mode::ReadOnly, Mode::WorkspaceWrite, "  needs to write  ");
        assert_eq!(request.justification(), "needs to write");
    }
}
