//! OpenAI-compatible model access for deepnest.
//!
//! The core problem this crate solves is not "call an endpoint" — it is that **a failed
//! request is not one thing**. A provider answers a content-filter refusal, an over-long
//! request, and an ordinary bad request with the same HTTP 400, and treating them alike is
//! how a model gets stuck: a refusal "compacted" forever, or a request that never fit
//! retried unchanged. So the crate classifies first and acts second
//! ([`error::LlmError::classify`]), and every gate that decides "retry" or "shrink" asks
//! that one classifier rather than spelling its own word list.
//!
//! Nothing here performs I/O. [`client::Transport`] is the seam the HTTP adapter
//! implements, so every retry, stream-assembly and classification rule in this crate is
//! exercised by tests with no network and no endpoint.
//!
//! ```
//! use deepnest_llm::{LlmError, FailureKind};
//!
//! // The same status, two different failures, told apart by the body.
//! let refused = LlmError::classify(Some(400), r#"{"error":{"message":"Content Exists Risk"}}"#);
//! assert_eq!(refused.kind, FailureKind::ContentRisk);
//!
//! let too_long = LlmError::classify(Some(400), "This model's maximum context length is 65536 tokens");
//! assert_eq!(too_long.kind, FailureKind::ContextTooLong);
//! ```

pub mod client;
pub mod error;
pub mod model;
pub mod retry;
pub mod stream;

pub use client::{
    endpoint_for, redact, Client, HttpRequest, HttpResponse, Sleeper, ThreadSleeper, Transport,
};
pub use error::{FailureKind, LlmError};
pub use model::{
    ChatRequest, ChatResponse, Choice, FunctionCall, Message, ToolCall, ToolDefinition,
};
pub use retry::RetryPolicy;
pub use stream::{AssistantTurn, FinishReason, SseDecoder, SseEvent, TurnAccumulator};
