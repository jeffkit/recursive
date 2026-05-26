//! Deterministic LLM for tests and offline development.
//!
//! `MockProvider` is fed a queue of pre-baked completions. The agent treats
//! it identically to a real provider, so the agent loop is fully testable
//! without network access.

use std::sync::Mutex;

use async_trait::async_trait;

use super::StructuredRequest;
use super::{Completion, LlmProvider, StreamSender, ToolSpec};
use crate::error::{Error, Result};
use crate::message::Message;
use tracing::Instrument;

#[derive(Default)]
pub struct MockProvider {
    scripted: Mutex<Vec<Completion>>,
    /// Queue of pre-baked JSON responses for `complete_structured`.
    structured_responses: Mutex<Vec<Result<serde_json::Value>>>,
    calls: Mutex<Vec<Vec<Message>>>,
}

impl MockProvider {
    /// Alias for `new` — both accept completions that may have `usage` set.
    /// Provided for API symmetry when callers want to be explicit about usage.
    pub fn with_usage(scripted: Vec<Completion>) -> Self {
        Self::new(scripted)
    }

    /// Create a new MockProvider with the given scripted completions.
    pub fn new(scripted: Vec<Completion>) -> Self {
        Self {
            scripted: Mutex::new(scripted),
            calls: Mutex::new(Vec::new()),
            structured_responses: Mutex::new(Vec::new()),
        }
    }

    /// Snapshot of the transcripts the agent has sent to this provider.
    pub fn calls(&self) -> Vec<Vec<Message>> {
        self.calls.lock().unwrap().clone()
    }

    /// Set the queue of structured responses for `complete_structured`.
    /// Each call to `complete_structured` pops the next response.
    /// If the queue is empty, it returns an error (fallback path).
    pub fn with_structured_responses(self, responses: Vec<Result<serde_json::Value>>) -> Self {
        *self.structured_responses.lock().unwrap() = responses;
        self
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn complete(&self, messages: &[Message], _tools: &[ToolSpec]) -> Result<Completion> {
        let span = tracing::info_span!("llm.complete", provider = "mock", model = "mock");
        async move {
            // Emit info log so tracing-test can capture the span
            tracing::info!("mock llm call");
            self.calls.lock().unwrap().push(messages.to_vec());
            let mut queue = self.scripted.lock().unwrap();
            if queue.is_empty() {
                return Err(Error::Llm {
                    provider: "mock".into(),
                    message: "MockProvider: no scripted completions left".into(),
                });
            }
            Ok(queue.remove(0))
        }
        .instrument(span)
        .await
    }

    async fn complete_structured(&self, _req: StructuredRequest) -> Result<serde_json::Value> {
        let mut queue = self.structured_responses.lock().unwrap();
        if queue.is_empty() {
            // Default: return error to trigger fallback
            return Err(Error::Config {
                message: "MockProvider: no structured responses configured".into(),
            });
        }
        queue.remove(0)
    }

    async fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        stream_tx: Option<StreamSender>,
    ) -> Result<Completion> {
        // MockProvider: just delegate to complete and emit the full content
        let completion = self.complete(messages, tools).await?;
        if let Some(tx) = stream_tx {
            if !completion.content.is_empty() {
                let _ = tx.send(completion.content.clone());
            }
        }
        Ok(completion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_scripted_in_order_and_records_calls() {
        let provider = MockProvider::new(vec![
            Completion {
                content: "one".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "two".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]);

        let m1 = vec![Message::user("hi")];
        let r1 = provider.complete(&m1, &[]).await.unwrap();
        assert_eq!(r1.content, "one");

        let m2 = vec![Message::user("bye")];
        let r2 = provider.complete(&m2, &[]).await.unwrap();
        assert_eq!(r2.content, "two");

        assert_eq!(provider.calls().len(), 2);
        assert_eq!(provider.calls()[0][0].content, "hi");
    }

    #[tokio::test]
    async fn errors_when_queue_drained() {
        let provider = MockProvider::new(vec![]);
        let err = provider.complete(&[], &[]).await.unwrap_err();
        assert!(matches!(err, Error::Llm { .. }));
    }
}

#[cfg(test)]
mod tracing_tests {
    use super::*;
    use crate::llm::TokenUsage;
    use tracing_test::traced_test;

    #[traced_test]
    #[tokio::test]
    async fn llm_complete_records_token_fields() {
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let provider = MockProvider::new(vec![Completion {
            content: "response".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(usage),
            reasoning_content: None,
        }]);

        provider.complete(&[], &[]).await.unwrap();

        // Should have created an llm.complete span - check for span name in output
        assert!(logs_contain("llm.complete"));
    }
}
