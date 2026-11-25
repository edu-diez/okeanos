use crate::events::ToolCall;
use crate::model::model_trait::{
    GenerationParams, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse,
    ModelStreamChunk, ModelTier, Usage,
};
use crate::tool::builtin::EditFormat;
use crate::tool::tool_trait::ToolSpec;
use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// A mock model for testing that returns pre-programmed responses.
pub struct MockModel {
    capabilities: ModelCapabilities,
    model_id: String,
    responses: Arc<Mutex<VecDeque<MockResponse>>>,
    streaming: bool,
}

/// A pre-programmed response for the mock model.
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub thinking: Option<String>,
    pub usage: Option<Usage>,
    pub should_error: Option<ModelError>,
}

impl MockModel {
    /// Create a new mock model with the given capabilities.
    pub fn new(capabilities: ModelCapabilities, model_id: impl Into<String>) -> Self {
        Self {
            capabilities,
            model_id: model_id.into(),
            responses: Arc::new(Mutex::new(VecDeque::new())),
            streaming: false,
        }
    }

    /// Create a mock model with small tier defaults.
    pub fn small(model_id: impl Into<String>) -> Self {
        Self::new(
            ModelCapabilities {
                tool_calling: false,
                structured_output: false,
                thinking: false,
                max_context: 4096,
                max_output: 2048,
                temperature_supported: true,
                tier: ModelTier::Small,
                edit_format: EditFormat::WholeFile,
            },
            model_id,
        )
    }

    /// Create a mock model with frontier tier defaults.
    pub fn frontier(model_id: impl Into<String>) -> Self {
        Self::new(
            ModelCapabilities {
                tool_calling: true,
                structured_output: true,
                thinking: true,
                max_context: 128000,
                max_output: 8192,
                temperature_supported: true,
                tier: ModelTier::Frontier,
                edit_format: EditFormat::Diff,
            },
            model_id,
        )
    }

    /// Push a response to the queue (returns in FIFO order).
    pub fn push_response(&self, response: MockResponse) {
        self.responses.lock().unwrap().push_back(response);
    }

    /// Push multiple responses at once.
    pub fn push_responses(&self, responses: Vec<MockResponse>) {
        let mut queue = self.responses.lock().unwrap();
        for r in responses {
            queue.push_back(r);
        }
    }

    /// Enable or disable streaming mode.
    pub fn set_streaming(&mut self, streaming: bool) {
        self.streaming = streaming;
    }

    /// Get the number of remaining responses.
    pub fn remaining_responses(&self) -> usize {
        self.responses.lock().unwrap().len()
    }

    /// Create a mock response with just text content.
    pub fn text_response(content: impl Into<String>) -> MockResponse {
        let content_str: String = content.into();
        MockResponse {
            content: content_str.clone(),
            tool_calls: None,
            thinking: None,
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: content_str.len() / 4,
                total_tokens: 100 + content_str.len() / 4,
            }),
            should_error: None,
        }
    }

    /// Create a mock response with tool calls.
    pub fn tool_call_response(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> MockResponse {
        let content_str: String = content.into();
        MockResponse {
            content: content_str,
            tool_calls: Some(tool_calls),
            thinking: None,
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
            }),
            should_error: None,
        }
    }

    /// Create a mock response that returns an error.
    pub fn error_response(error: ModelError) -> MockResponse {
        MockResponse {
            content: String::new(),
            tool_calls: None,
            thinking: None,
            usage: None,
            should_error: Some(error),
        }
    }

    /// Create a mock response with rate limit error.
    pub fn rate_limit_error(retry_after_sec: u64) -> MockResponse {
        Self::error_response(ModelError::RateLimited { retry_after_sec })
    }

    /// Create a mock response with context length exceeded error.
    pub fn context_length_error(msg: impl Into<String>) -> MockResponse {
        Self::error_response(ModelError::ContextLengthExceeded(msg.into()))
    }
}

#[async_trait]
impl Model for MockModel {
    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn complete(
        &self,
        _messages: &[ModelMessage],
        _params: &GenerationParams,
        _tools: Option<&[ToolSpec]>,
    ) -> Result<ModelResponse, ModelError> {
        let mut queue = self.responses.lock().unwrap();
        let response = queue.pop_front().unwrap_or(MockResponse {
            content: "Mock response (queue empty)".into(),
            tool_calls: None,
            thinking: None,
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 10,
                total_tokens: 10,
            }),
            should_error: None,
        });

        if let Some(err) = response.should_error {
            return Err(err);
        }

        Ok(ModelResponse {
            content: response.content,
            tool_calls: response.tool_calls,
            thinking: response.thinking,
            usage: response.usage,
        })
    }

    async fn stream(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        _tools: Option<&[ToolSpec]>,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ModelStreamChunk, ModelError>> + Send + 'static>>,
        ModelError,
    > {
        // For streaming, we get the complete response and chunk it
        let response = self.complete(messages, params, None).await?;

        let content = response.content;
        let chunk_size = if self.streaming {
            10
        } else {
            content.len().max(1)
        };

        let total_chunks = content.len().div_ceil(chunk_size);
        let streaming = self.streaming;

        // Create a stream that yields control between chunks when streaming is enabled
        let chunk_stream = stream! {
            if content.is_empty() {
                // Tool-call-only responses have no text; still emit the terminal chunk.
                yield Ok(ModelStreamChunk {
                    tool_calls: response.tool_calls.clone(),
                    thinking: response.thinking.clone(),
                    finished: true,
                    usage: response.usage.clone(),
                    ..Default::default()
                });
            }
            for (i, chunk) in content.as_bytes().chunks(chunk_size).enumerate() {
                let delta = String::from_utf8_lossy(chunk).to_string();
                let finished = i == total_chunks - 1;
                yield Ok(ModelStreamChunk {
                    delta,
                    tool_calls: if finished {
                        response.tool_calls.clone()
                    } else {
                        None
                    },
                    thinking: if finished {
                        response.thinking.clone()
                    } else {
                        None
                    },
                    finished,
                    usage: if finished { response.usage.clone() } else { None },
                    truncated: false,
                });

                // Yield control between chunks when streaming to allow interruption
                if streaming {
                    tokio::task::yield_now().await;
                }
            }
        };

        Ok(Box::pin(chunk_stream))
    }
}
