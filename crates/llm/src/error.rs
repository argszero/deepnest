//! What kind of failure an LLM request hit, and what that permits.
//!
//! The priority inside [`LlmError::classify`] is the whole point of this module: a
//! content-filter refusal also arrives as a 400, so the refusal class is checked **before**
//! the length class. Without that order the fallback below swallows every refusal and the
//! caller shrinks a request that was never too big — forever.

use std::fmt;

/// The three distinguishable ways a request can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The provider refused the text itself (its content filter). Resending the same text
    /// cannot help; the context has to change.
    ContentRisk,
    /// The request did not fit: the model's context, or a proxy's body limit.
    ContextTooLong,
    /// Anything else — auth, a bad parameter, a network fault, a 5xx.
    Other,
}

/// A failed request, carrying what the classifier read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmError {
    /// The HTTP status, when the failure came with one. `None` for a transport fault.
    pub status: Option<u16>,
    /// The response body (already redacted by the adapter), or the transport message.
    pub body: String,
    pub kind: FailureKind,
}

impl LlmError {
    /// The wording a refusals-capable provider uses when its content filter rejects input.
    pub const CONTENT_RISK_MARKER: &'static str = "content exists risk";

    /// Statuses that mean "the request body did not fit".
    const OVERLONG_STATUSES: [u16; 2] = [400, 413];

    /// Bodies that mean "the request body did not fit". Providers word this differently per
    /// gateway: a model context overflow, a proxy body limit, or a plain "too long".
    const OVERLONG_MARKERS: [&'static str; 9] = [
        "context length",
        "context window",
        "context_length",
        "maximum context",
        "prompt is too long",
        "reduce the length",
        "too long",
        "length limit",
        "length exceeded",
    ];

    /// Statuses worth retrying unchanged: rate limits and server-side faults.
    const TRANSIENT_STATUSES: [u16; 5] = [429, 500, 502, 503, 504];

    /// Classify a failed request from its status and body.
    ///
    /// **Order matters.** The discriminating signals, in priority:
    ///   1. the provider refused the text itself — the body names the refusal;
    ///   2. the request did not fit — an overlong marker in the body, or a bare 400/413 with
    ///      nothing more specific to go on;
    ///   3. anything else.
    ///
    /// Step 2's bare-status fallback is deliberately kept: a 400 whose body carries no
    /// discriminating wording is far more often a context overflow than a malformed request
    /// in practice, and the caller can only act on "shrink and retry". A refusal is what
    /// must never fall through to it, which is why step 1 is checked first.
    pub fn classify(status: Option<u16>, body: impl Into<String>) -> Self {
        let body = body.into();
        let lowered = body.to_lowercase();

        let kind = if lowered.contains(Self::CONTENT_RISK_MARKER) {
            FailureKind::ContentRisk
        } else if Self::OVERLONG_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
            || matches!(status, Some(s) if Self::OVERLONG_STATUSES.contains(&s))
        {
            FailureKind::ContextTooLong
        } else {
            FailureKind::Other
        };

        Self { status, body, kind }
    }

    /// A failure that never reached the provider — a refused connection, a truncated body.
    pub fn transport(body: impl Into<String>) -> Self {
        Self {
            status: None,
            body: body.into(),
            kind: FailureKind::Other,
        }
    }

    /// A provider refusal that carried no HTTP status of its own — a stream's
    /// `finish_reason: "content_filter"`. Built explicitly rather than classified, because that
    /// wording shares no marker with the 400 refusal, and the class is the one thing the caller
    /// reacts to.
    pub fn content_risk(body: impl Into<String>) -> Self {
        Self {
            status: None,
            body: body.into(),
            kind: FailureKind::ContentRisk,
        }
    }

    /// A capability the transport does not implement whose streaming never reached the wire.
    pub fn unsupported(what: impl Into<String>) -> Self {
        Self {
            status: None,
            body: format!("unsupported: {}", what.into()),
            kind: FailureKind::Other,
        }
    }

    /// Whether retrying the *same* request could plausibly succeed.
    ///
    /// A content refusal and an over-long request both answer `false`: neither is fixed by
    /// sending identical bytes again, which is exactly why the classifier is asked first.
    pub fn is_transient(&self) -> bool {
        matches!(self.status, Some(s) if Self::TRANSIENT_STATUSES.contains(&s))
    }

    pub fn is_content_risk(&self) -> bool {
        self.kind == FailureKind::ContentRisk
    }

    /// Whether the request did not fit, so the caller should shrink rather than resend.
    pub fn is_overlong(&self) -> bool {
        self.kind == FailureKind::ContextTooLong
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "model request failed ({status}): {}", self.body),
            None => write!(f, "model request failed: {}", self.body),
        }
    }
}

impl std::error::Error for LlmError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_risk_wins_over_the_bare_400_fallback() {
        // The failure this ordering exists to prevent: a refusal read as "too long".
        let err = LlmError::classify(
            Some(400),
            r#"{"error":{"message":"Content Exists Risk","type":"invalid_request_error"}}"#,
        );
        assert_eq!(err.kind, FailureKind::ContentRisk);
        assert!(!err.is_overlong());
    }

    #[test]
    fn context_overflow_is_recognised_from_the_body() {
        let err = LlmError::classify(
            Some(400),
            r#"{"error":{"message":"This model's maximum context length is 65536 tokens"}}"#,
        );
        assert_eq!(err.kind, FailureKind::ContextTooLong);
    }

    #[test]
    fn proxy_body_limit_413_is_also_overlong() {
        // A 413 whose wording arrived in the other order still classifies.
        let err = LlmError::classify(
            Some(413),
            "Failed to buffer the request body: length limit exceeded",
        );
        assert_eq!(err.kind, FailureKind::ContextTooLong);

        let reversed = LlmError::classify(Some(413), "length exceeded: buffering failed");
        assert_eq!(reversed.kind, FailureKind::ContextTooLong);
    }

    #[test]
    fn bare_400_falls_back_to_overlong() {
        let err = LlmError::classify(Some(400), "bad request");
        assert_eq!(err.kind, FailureKind::ContextTooLong);
    }

    #[test]
    fn auth_and_not_found_are_neither_refusal_nor_length() {
        for status in [401, 403, 404] {
            let err = LlmError::classify(Some(status), "nope");
            assert_eq!(err.kind, FailureKind::Other);
            assert!(!err.is_transient());
        }
    }

    #[test]
    fn server_faults_and_rate_limits_are_transient() {
        for status in [429, 500, 502, 503, 504] {
            let err = LlmError::classify(Some(status), "server said no");
            assert!(err.is_transient(), "status {status} should be retryable");
            assert_eq!(err.kind, FailureKind::Other);
        }
    }

    #[test]
    fn a_refusal_is_never_retryable_even_if_the_status_looks_transient() {
        // Same status, different failure: the classifier decides, not the status alone.
        let err = LlmError::classify(Some(503), "Content Exists Risk");
        assert!(err.is_content_risk());
        assert!(!err.is_overlong());
    }

    #[test]
    fn transport_faults_carry_no_status_and_are_not_retryable_by_policy() {
        let err = LlmError::transport("connection reset by peer");
        assert_eq!(err.status, None);
        assert_eq!(err.kind, FailureKind::Other);
        assert!(!err.is_transient());
        assert_eq!(
            err.to_string(),
            "model request failed: connection reset by peer"
        );
    }
}
