//! The model the binary uses: the user's own endpoint, in the two shapes a turn can arrive in.
//!
//! `crates/agent` reaches the model through its own `Model` trait so the loop is testable with no
//! network; this is the one line of wiring that connects it to [`deepnest_llm::Client`].
//!
//! A turn asked the plain way — [`Model::turn`], which is one [`Client::turn`] — is one whole answer
//! read from one response. That is all a caller that only wants the result needs, and it is what
//! `POST …/chat` still asks for.
//!
//! A turn asked with an observer — [`Model::turn_with`] — is the same turn streamed: the adapter in
//! `crates/gateway::transport` delivers the provider's events as it reads them, and
//! [`Client::chat_stream_with`] hands each fragment of the answer's text over while the model is
//! still writing. The loop asks that way only when somebody attached an observer, so what a caller
//! who did not ask to watch receives is unchanged.
//!
//! What is still missing for a *person* to watch one is above this file rather than in it: a `chat`
//! route that writes the turn as it arrives, and the page that renders it.
//!
//! The prompt is passed on every request because the loop owns it; nothing here decides what the
//! agent is told.

use std::sync::Arc;

use deepnest_agent::Model;
use deepnest_llm::{
    AssistantTurn, ChatRequest, Client, LlmError, Message, ToolDefinition, Transport,
};

/// A model client bound to one endpoint, key and model name.
pub struct Endpoint {
    client: Client,
}

impl Endpoint {
    /// One endpoint from the config file's `[llm]` table.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            client: Client::new(base_url, api_key, model, transport),
        }
    }

    /// The endpoint this was built for, for a message that has to say where it sent the request.
    pub fn endpoint(&self) -> &str {
        self.client.endpoint()
    }
}

impl Model for Endpoint {
    fn turn(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<AssistantTurn, LlmError> {
        // No `.streaming()`: this path asks for one response, and the loop still gets an
        // `AssistantTurn`. The trade is arrival, not content.
        let request = ChatRequest::new(self.client.model().to_string(), messages.to_vec())
            .with_tools(tools.to_vec());
        self.client.turn(&request)
    }

    fn turn_with(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        on_text: &mut dyn FnMut(&str),
    ) -> Result<AssistantTurn, LlmError> {
        // `.streaming()` and a streamed read, which is the whole difference: the same request, the
        // same assembled `AssistantTurn` out of it, and each fragment of the text reported on the
        // way rather than at the end.
        let request = ChatRequest::new(self.client.model().to_string(), messages.to_vec())
            .streaming()
            .with_tools(tools.to_vec());
        self.client.chat_stream_with(&request, on_text)
    }
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key lives inside the client and must not reach a log line.
        f.debug_struct("Endpoint")
            .field("endpoint", &self.client.endpoint())
            .field("model", &self.client.model())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use deepnest_gateway::transport::HttpTransport;

    /// A provider that writes its answer in pieces: the head, then one event at a time, waiting
    /// between them.
    ///
    /// Pieces rather than one body because the property under test is *when* a fragment arrives. An
    /// endpoint that read the answer whole and then replayed it into the observer would produce the
    /// same text and pass every assertion about content; the pause is what tells the two apart.
    fn serve_in_pieces(pieces: Vec<(String, Duration)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut seen = Vec::new();
                let mut buf = [0u8; 1024];
                // The blank line ends the head. A `POST` body is promised by its `Content-Length`,
                // so this is a complete request as soon as that line has arrived.
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            seen.extend_from_slice(&buf[..read]);
                            if seen.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\r\n",
                );
                for (body, pause) in pieces {
                    let _ = stream.write_all(format!("{:x}\r\n{body}\r\n", body.len()).as_bytes());
                    let _ = stream.flush();
                    std::thread::sleep(pause);
                }
                // The zero-length chunk is how a chunked body says it has ended, and a stream that
                // closed without it would be a truncated answer rather than a finished one.
                let _ = stream.write_all(b"0\r\n\r\n");
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{port}/v1/chat/completions")
    }

    /// One event as a provider writes it.
    fn event(data: &str) -> String {
        format!("data: {data}\n\n")
    }

    fn endpoint_at(url: String) -> Endpoint {
        Endpoint::new(
            url,
            "sk-test".to_string(),
            "m".to_string(),
            Arc::new(HttpTransport::new()),
        )
    }

    /// The one property a watched turn exists for: a fragment reaches the caller while the model is
    /// still writing the answer.
    ///
    /// The assertion is on *when* rather than on *what*, which is why it survives an implementation
    /// that gets the text right by collecting it first.
    #[test]
    fn the_text_of_a_watched_turn_arrives_before_the_turn_is_finished() {
        let url = serve_in_pieces(vec![
            (
                event(r#"{"choices":[{"index":0,"delta":{"content":"Hel"}}]}"#),
                Duration::from_millis(400),
            ),
            (
                event(r#"{"choices":[{"index":0,"delta":{"content":"lo"}}]}"#),
                Duration::from_millis(400),
            ),
            (
                event(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
                Duration::from_millis(400),
            ),
        ]);
        let endpoint = endpoint_at(url);

        let started = Instant::now();
        let mut arrivals: Vec<(String, Duration)> = Vec::new();
        let turn = endpoint
            .turn_with(&[Message::user("hi")], &[], &mut |text| {
                arrivals.push((text.to_string(), started.elapsed()))
            })
            .expect("a watched turn");
        let ended = started.elapsed();

        assert_eq!(turn.content.as_deref(), Some("Hello"));
        assert_eq!(
            arrivals
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>(),
            vec!["Hel", "lo"],
            "each fragment is reported once, as the model wrote it"
        );
        assert!(
            arrivals[0].1 + Duration::from_millis(300) < ended,
            "the first fragment was held until the answer ended (first {:?}, end {ended:?})",
            arrivals[0].1
        );
    }
}
