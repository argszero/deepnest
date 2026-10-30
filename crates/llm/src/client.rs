//! The client: one endpoint, a user-supplied key, a retry budget and no vendor logic.
//!
//! **There is no HTTP client in this crate.** [`Transport`] is the single seam an adapter
//! implements, which is what makes every rule below testable with no network: the retry budget,
//! the classification of a failure, the assembly of a stream, and the promise that the key never
//! reaches a log line. `crates/llm` decides *what* a failure means; the adapter only moves bytes.
//!
//! Two guarantees the tests pin:
//!
//! - **A refusal is returned as a refusal, never retried**, and a stream's
//!   `finish_reason: "content_filter"` becomes the same failure as a 400 refusal, so a caller
//!   has one place to react to it.
//! - **A retry never resumes a stream that already delivered events.** Restarting there would
//!   replay half a turn, so once bytes have arrived a failure is final.

use std::sync::Arc;
use std::time::Duration;

use crate::error::LlmError;
use crate::model::{ChatRequest, ChatResponse, Message};
use crate::retry::RetryPolicy;
use crate::stream::{AssistantTurn, FinishReason, SseEvent, StreamChunk, TurnAccumulator};

/// What the transport is asked to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    /// Sent as given. The key travels in `Authorization` and is never logged by this crate.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// What came back. A non-2xx status is a value, not an error: classification is this crate's
/// job, and a status alone does not say what kind of failure it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// The HTTP seam. An adapter built on any client library implements these two methods.
pub trait Transport: Send + Sync {
    /// A single request/response exchange. A refused connection is an [`LlmError::transport`].
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, LlmError>;

    /// A streaming exchange. Call `on_event` once per complete server-sent event; returning
    /// `Err` from it aborts the read and that error becomes this method's result.
    ///
    /// A non-2xx response must be read to its body and returned as
    /// `Err(LlmError::classify(Some(status), body))`, so that a refusal is classified exactly as
    /// it is on the non-streaming path.
    fn post_stream(
        &self,
        request: &HttpRequest,
        on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
    ) -> Result<(), LlmError>;
}

/// Where the wait between attempts happens. Injected so a test can prove the schedule without
/// spending it.
pub trait Sleeper: Send + Sync {
    fn sleep(&self, duration: Duration);
}

/// The real one.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// An OpenAI-compatible chat client bound to one endpoint, key and model.
pub struct Client {
    endpoint: String,
    api_key: String,
    model: String,
    transport: Arc<dyn Transport>,
    retry: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
}

impl Client {
    /// `base_url` is the user's (`host 20:18`): `https://host/v1`, or the full
    /// `…/chat/completions` URL. Nothing vendor-specific is appended beyond that path.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            endpoint: endpoint_for(&base_url.into()),
            api_key: api_key.into(),
            model: model.into(),
            transport,
            retry: RetryPolicy::new(),
            sleeper: Arc::new(ThreadSleeper),
        }
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// One non-streamed request. Transient failures are retried within the budget; anything
    /// else is returned as classified.
    pub fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let http = self.build(request)?;
        let mut attempt = 1;
        loop {
            let failure = match self.transport.post(&http) {
                Ok(response) if (200..300).contains(&response.status) => {
                    let parsed: ChatResponse =
                        serde_json::from_str(&response.body).map_err(|e| {
                            LlmError::transport(format!("response body was not a completion: {e}"))
                        })?;
                    if parsed.choices.is_empty() {
                        return Err(LlmError::transport("response contained no choices"));
                    }
                    return Ok(parsed);
                }
                Ok(response) => LlmError::classify(Some(response.status), response.body),
                Err(error) => error,
            };
            let failure = self.scrub(failure);
            if !self.retry.may_retry(attempt, &failure) {
                return Err(failure);
            }
            self.sleeper.sleep(self.retry.delay_for(attempt));
            attempt += 1;
        }
    }

    /// One streamed request, assembled into a completed turn.
    ///
    /// A turn the provider refused arrives as `Err` whose class is
    /// [`crate::FailureKind::ContentRisk`] — the same failure a 400 refusal produces — because a
    /// caller must not be able to mistake it for an answer.
    pub fn chat_stream(&self, request: &ChatRequest) -> Result<AssistantTurn, LlmError> {
        let streaming = ChatRequest {
            stream: Some(true),
            ..request.clone()
        };
        let http = self.build(&streaming)?;
        let mut attempt = 1;
        loop {
            let mut turn = TurnAccumulator::new();
            let mut saw_done = false;
            let mut delivered = 0usize;

            let outcome = self.transport.post_stream(&http, &mut |event| {
                if event.is_done() {
                    saw_done = true;
                    return Ok(());
                }
                delivered += 1;
                let chunk: StreamChunk = serde_json::from_str(&event.data).map_err(|e| {
                    LlmError::transport(format!("stream carried a malformed chunk: {e}"))
                })?;
                turn.apply(&chunk);
                Ok(())
            });

            match outcome {
                Err(error) => {
                    let error = self.scrub(error);
                    // Restarting after events have been delivered would replay half a turn.
                    if delivered > 0 || !self.retry.may_retry(attempt, &error) {
                        return Err(error);
                    }
                    self.sleeper.sleep(self.retry.delay_for(attempt));
                    attempt += 1;
                }
                Ok(()) => {
                    let finish = turn.finish_reason().clone();
                    if finish.is_refusal() {
                        return Err(LlmError::content_risk(
                            "the provider refused this content (finish_reason: content_filter)",
                        ));
                    }
                    if finish == FinishReason::Absent && !saw_done {
                        return Err(LlmError::transport(
                            "stream ended without a finish_reason or an end-of-stream sentinel",
                        ));
                    }
                    return Ok(turn.into_turn());
                }
            }
        }
    }

    /// The common case: send messages, get the turn.
    pub fn complete(&self, messages: Vec<Message>) -> Result<AssistantTurn, LlmError> {
        let request = ChatRequest::new(self.model.clone(), messages);
        self.chat_stream(&request)
    }

    fn build(&self, request: &ChatRequest) -> Result<HttpRequest, LlmError> {
        let body = serde_json::to_string(request)
            .map_err(|e| LlmError::transport(format!("request could not be serialised: {e}")))?;
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ),
        ];
        if request.stream == Some(true) {
            headers.push(("Accept".to_string(), "text/event-stream".to_string()));
        }
        Ok(HttpRequest {
            url: self.endpoint.clone(),
            headers,
            body,
        })
    }

    /// Take the key out of anything leaving this crate for a log or a user.
    fn scrub(&self, error: LlmError) -> LlmError {
        LlmError {
            body: redact(&error.body, &self.api_key),
            ..error
        }
    }
}

/// The endpoint a user's `base_url` means. A base that already names the path is used as given,
/// so a user who pastes the full URL is not given a doubled one.
pub fn endpoint_for(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

/// Replace every occurrence of `secret` with `***`.
pub fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "***")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::SseDecoder;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Serves scripted responses and records what it was asked to send.
    #[derive(Default)]
    struct ScriptedTransport {
        responses: Mutex<VecDeque<Result<HttpResponse, LlmError>>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<Result<HttpResponse, LlmError>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn last_request(&self) -> HttpRequest {
            self.requests.lock().unwrap().last().cloned().unwrap()
        }
    }

    impl Transport for ScriptedTransport {
        fn post(&self, request: &HttpRequest) -> Result<HttpResponse, LlmError> {
            self.requests.lock().unwrap().push(request.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(LlmError::transport("script exhausted")))
        }

        fn post_stream(
            &self,
            _request: &HttpRequest,
            _on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
        ) -> Result<(), LlmError> {
            Err(LlmError::unsupported("streaming not scripted"))
        }
    }

    /// Feeds raw SSE fragments, as the wire would deliver them.
    #[derive(Default)]
    struct StreamTransport {
        scripts: Mutex<VecDeque<Result<Vec<String>, LlmError>>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl StreamTransport {
        fn new(scripts: Vec<Result<Vec<String>, LlmError>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl Transport for StreamTransport {
        fn post(&self, _request: &HttpRequest) -> Result<HttpResponse, LlmError> {
            Err(LlmError::unsupported("non-streaming not scripted"))
        }

        fn post_stream(
            &self,
            request: &HttpRequest,
            on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
        ) -> Result<(), LlmError> {
            self.requests.lock().unwrap().push(request.clone());
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(LlmError::transport("script exhausted")))?;
            let mut decoder = SseDecoder::new();
            for fragment in script {
                for event in decoder.push(&fragment) {
                    on_event(event)?;
                }
            }
            for event in decoder.finish() {
                on_event(event)?;
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingSleeper {
        waited: Mutex<Vec<Duration>>,
    }

    impl Sleeper for RecordingSleeper {
        fn sleep(&self, duration: Duration) {
            self.waited.lock().unwrap().push(duration);
        }
    }

    fn client_with(transport: Arc<dyn Transport>, sleeper: Arc<RecordingSleeper>) -> Client {
        Client::new(
            "https://model.example/v1",
            "sk-secret",
            "test-model",
            transport,
        )
        .with_sleeper(sleeper)
    }

    fn request() -> ChatRequest {
        ChatRequest::new("test-model", vec![Message::user("hi")])
    }

    fn ok(body: &str) -> Result<HttpResponse, LlmError> {
        Ok(HttpResponse {
            status: 200,
            body: body.to_string(),
        })
    }

    #[test]
    fn a_transient_failure_is_retried_on_a_doubling_schedule() {
        let transport = ScriptedTransport::new(vec![
            Ok(HttpResponse {
                status: 429,
                body: "slow down".into(),
            }),
            Ok(HttpResponse {
                status: 503,
                body: "unavailable".into(),
            }),
            ok(
                r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"done"},
                 "finish_reason":"stop"}]}"#,
            ),
        ]);
        let sleeper = Arc::new(RecordingSleeper::default());
        let client = client_with(transport.clone(), sleeper.clone());

        let response = client.chat(&request()).expect("third attempt succeeds");
        assert_eq!(response.choices[0].message.content.as_deref(), Some("done"));
        assert_eq!(transport.request_count(), 3);
        assert_eq!(
            *sleeper.waited.lock().unwrap(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[test]
    fn a_refusal_is_returned_at_once_and_never_retried() {
        let transport = ScriptedTransport::new(vec![Ok(HttpResponse {
            status: 400,
            body: r#"{"error":{"message":"Content Exists Risk"}}"#.into(),
        })]);
        let sleeper = Arc::new(RecordingSleeper::default());
        let client = client_with(transport.clone(), sleeper.clone());

        let error = client.chat(&request()).expect_err("a refusal is a failure");
        assert!(error.is_content_risk(), "{error:?}");
        assert!(!error.is_overlong());
        assert_eq!(transport.request_count(), 1, "one attempt, no retry");
        assert!(sleeper.waited.lock().unwrap().is_empty());
    }

    #[test]
    fn the_retry_budget_is_finite() {
        let transport = ScriptedTransport::new(vec![
            Ok(HttpResponse {
                status: 500,
                body: "boom".into(),
            });
            4
        ]);
        let sleeper = Arc::new(RecordingSleeper::default());
        let client = client_with(transport.clone(), sleeper.clone());

        let error = client.chat(&request()).expect_err("exhausted budget");
        assert_eq!(error.status, Some(500));
        assert_eq!(transport.request_count(), 4);
        assert_eq!(sleeper.waited.lock().unwrap().len(), 3);
    }

    #[test]
    fn the_key_travels_in_a_header_and_never_in_an_error() {
        let transport = ScriptedTransport::new(vec![Ok(HttpResponse {
            status: 401,
            body: "invalid key sk-secret in request".into(),
        })]);
        let client = client_with(transport.clone(), Arc::new(RecordingSleeper::default()));

        let error = client.chat(&request()).expect_err("401");
        assert!(!error.body.contains("sk-secret"), "{}", error.body);
        assert!(error.body.contains("***"), "{}", error.body);

        let sent = transport.last_request();
        let auth = sent
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.clone())
            .expect("Authorization header present");
        assert_eq!(auth, "Bearer sk-secret");
    }

    #[test]
    fn a_user_pasted_full_url_is_not_doubled() {
        assert_eq!(
            endpoint_for("https://api.example/v1"),
            "https://api.example/v1/chat/completions"
        );
        assert_eq!(
            endpoint_for("https://api.example/v1/"),
            "https://api.example/v1/chat/completions"
        );
        assert_eq!(
            endpoint_for("https://api.example/v1/chat/completions"),
            "https://api.example/v1/chat/completions"
        );
    }

    #[test]
    fn a_streamed_turn_is_assembled_across_fragments() {
        let transport = StreamTransport::new(vec![Ok(vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n".into(),
            "\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n".into(),
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                .into(),
        ])]);
        let client = client_with(transport.clone(), Arc::new(RecordingSleeper::default()));

        let turn = client.chat_stream(&request()).expect("assembled");
        assert_eq!(turn.content.as_deref(), Some("Hello"));
        assert_eq!(turn.finish_reason, FinishReason::Stop);
        assert_eq!(transport.request_count(), 1);
    }

    #[test]
    fn a_content_filter_finish_becomes_the_same_failure_as_a_400_refusal() {
        let transport = StreamTransport::new(vec![Ok(vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\ndata: [DONE]\n\n".into(),
        ])]);
        let client = client_with(transport.clone(), Arc::new(RecordingSleeper::default()));

        let error = client
            .chat_stream(&request())
            .expect_err("a refusal is not a turn");
        assert!(error.is_content_risk(), "{error:?}");
        assert!(
            !error.is_overlong(),
            "a refusal must never be read as too long"
        );
        assert_eq!(transport.request_count(), 1);
    }

    #[test]
    fn a_stream_that_dies_before_any_event_is_retried() {
        let transport = StreamTransport::new(vec![
            Err(LlmError::classify(Some(503), "unavailable")),
            Ok(vec![
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into(),
            ]),
        ]);
        let sleeper = Arc::new(RecordingSleeper::default());
        let client = client_with(transport.clone(), sleeper.clone());

        let turn = client.chat_stream(&request()).expect("second attempt");
        assert_eq!(turn.content.as_deref(), Some("ok"));
        assert_eq!(transport.request_count(), 2);
        assert_eq!(
            *sleeper.waited.lock().unwrap(),
            vec![Duration::from_secs(1)]
        );
    }

    #[test]
    fn a_stream_that_dies_after_delivering_events_is_not_replayed() {
        struct DyingAfterFirst;
        impl Transport for DyingAfterFirst {
            fn post(&self, _request: &HttpRequest) -> Result<HttpResponse, LlmError> {
                Err(LlmError::unsupported("non-streaming"))
            }
            fn post_stream(
                &self,
                _request: &HttpRequest,
                on_event: &mut dyn FnMut(SseEvent) -> Result<(), LlmError>,
            ) -> Result<(), LlmError> {
                on_event(SseEvent {
                    event: None,
                    data: r#"{"choices":[{"index":0,"delta":{"content":"half a tur"}}]}"#.into(),
                })?;
                Err(LlmError::classify(
                    Some(503),
                    "connection closed mid-stream",
                ))
            }
        }
        let sleeper = Arc::new(RecordingSleeper::default());
        let client = client_with(Arc::new(DyingAfterFirst), sleeper.clone());

        let error = client
            .chat_stream(&request())
            .expect_err("truncated stream");
        assert_eq!(error.status, Some(503));
        assert!(
            sleeper.waited.lock().unwrap().is_empty(),
            "half a turn must not be replayed"
        );
    }

    #[test]
    fn a_stream_that_ends_without_any_finish_signal_is_a_failure() {
        let transport = StreamTransport::new(vec![Ok(vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"cut\"}}]}\n\n".into(),
        ])]);
        let client = client_with(transport, Arc::new(RecordingSleeper::default()));
        let error = client
            .chat_stream(&request())
            .expect_err("no finish signal");
        assert_eq!(error.kind, crate::FailureKind::Other);
        assert!(error.body.contains("finish_reason"), "{}", error.body);
    }

    #[test]
    fn a_truncated_turn_reports_length_rather_than_stop() {
        let transport = StreamTransport::new(vec![Ok(vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"cut\"},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n".into(),
        ])]);
        let client = client_with(transport, Arc::new(RecordingSleeper::default()));
        let turn = client.chat_stream(&request()).expect("a turn, truncated");
        assert!(turn.finish_reason.is_truncated());
    }

    #[test]
    fn a_non_streamed_response_with_no_choices_is_a_failure() {
        let transport = ScriptedTransport::new(vec![ok(r#"{"choices":[]}"#)]);
        let client = client_with(transport, Arc::new(RecordingSleeper::default()));
        let error = client.chat(&request()).expect_err("no choices");
        assert!(error.body.contains("no choices"), "{}", error.body);
    }

    #[test]
    fn redaction_leaves_nothing_of_the_secret() {
        assert_eq!(
            redact("key=sk-abc; again sk-abc", "sk-abc"),
            "key=***; again ***"
        );
        assert_eq!(redact("no secret here", ""), "no secret here");
    }
}
