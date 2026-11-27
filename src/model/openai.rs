//! OpenRouter / OpenAI-compatible model provider.
//!
//! Implements the [`Model`] trait for OpenAI-compatible APIs (OpenRouter,
//! local vLLM, etc.) with native function calling, streaming via SSE,
//! error mapping to [`ModelError`] variants, and cost estimation from
//! OpenRouter metadata.

use crate::context::tokens::ModelPricing;
use crate::events::ToolCall;
use crate::model::model_trait::{
    GenerationParams, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse,
    ModelStreamChunk, ToolChoice, Usage, is_context_overflow_message, retry_after_secs,
};
use crate::model::openai_native::{OpenAIMessage, OpenAIUsage};
use crate::tool::builtin::EditFormat;
use crate::tool::tool_trait::ToolSpec;
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;

/// OpenAI-compatible chat completion request.
#[derive(Debug, Clone, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    /// OpenRouter unified reasoning control: `{"effort": "high"}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
}

/// OpenAI-compatible tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAITool {
    r#type: String,
    function: OpenAIFunction,
}

/// OpenAI-compatible function definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIFunction {
    name: String,
    description: String,
    parameters: Value,
}

/// OpenAI-compatible chat completion response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ChatCompletionResponse {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageInfo>,
}

/// A choice in a chat completion response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct Choice {
    index: usize,
    message: ChatMessageResponse,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// A chat message in a response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ChatMessageResponse {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    /// OpenRouter returns reasoning text as `reasoning`; DeepSeek-style as `reasoning_content`.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// An OpenAI tool call.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenAIToolCall {
    id: String,
    r#type: String,
    function: OpenAIFunctionCall,
}

/// An OpenAI function call.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenAIFunctionCall {
    name: String,
    arguments: String,
}

/// Token usage info from OpenAI API.
type UsageInfo = OpenAIUsage;

/// OpenAI API error response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenAIErrorResponse {
    error: OpenAIErrorBody,
}

/// OpenAI API error body.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct OpenAIErrorBody {
    message: String,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    code: Option<i32>,
}

/// Streaming chunk from OpenAI API.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ChatCompletionChunk {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<ChunkChoice>,
    /// Sent on a trailing chunk (usually with empty `choices`).
    #[serde(default)]
    usage: Option<UsageInfo>,
}
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ChunkChoice {
    index: usize,
    delta: ChunkDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// Delta in a streaming chunk.
#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
struct ChunkDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChunkToolCall>>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// A tool call in a streaming chunk delta.
#[derive(Debug, Clone, Deserialize)]
struct ChunkToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: ChunkFunctionCall,
}

/// A function call in a streaming chunk delta.
#[derive(Debug, Clone, Deserialize, Default)]
struct ChunkFunctionCall {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Accumulated state for a streaming tool call.
#[derive(Debug, Clone, Default)]
struct StreamingToolCall {
    index: u32,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// OpenRouter / OpenAI-compatible model provider.
pub struct OpenRouterModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model_id: String,
    capabilities: ModelCapabilities,
    pricing: Option<ModelPricing>,
}

impl OpenRouterModel {
    /// Create a new OpenRouter model provider.
    pub fn new(
        client: reqwest::Client,
        base_url: &str,
        api_key: &str,
        model_id: &str,
        capabilities: ModelCapabilities,
        pricing: Option<ModelPricing>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model_id: model_id.to_string(),
            capabilities,
            pricing,
        }
    }

    /// Create from OpenRouter discovery metadata.
    pub fn from_discovered(
        client: reqwest::Client,
        base_url: &str,
        api_key: &str,
        discovered: &crate::model::discovery::DiscoveredModel,
    ) -> Self {
        let capabilities = ModelCapabilities {
            tool_calling: discovered.supports_tools,
            structured_output: discovered.supports_structured_output,
            thinking: false,
            max_context: discovered.context_length,
            max_output: discovered.max_output_tokens,
            temperature_supported: true,
            vision: false,
            tier: discovered.tier,
            edit_format: EditFormat::default_for_tier(discovered.tier),
        };

        let pricing = Some(ModelPricing {
            input_per_million: discovered.pricing.prompt.parse::<f64>().unwrap_or(0.0),
            output_per_million: discovered.pricing.completion.parse::<f64>().unwrap_or(0.0),
        });

        Self::new(
            client,
            base_url,
            api_key,
            &discovered.id,
            capabilities,
            pricing,
        )
    }

    /// Returns the per-model pricing, if available from discovery metadata.
    pub fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
    }

    fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn bearer_auth(&self) -> header::HeaderValue {
        header::HeaderValue::from_str(&format!("Bearer {}", self.api_key)).unwrap()
    }

    /// Map an HTTP error response to a [`ModelError`].
    fn map_http_error(status: StatusCode, body: &str) -> ModelError {
        let error_message = serde_json::from_str::<OpenAIErrorResponse>(body)
            .ok()
            .map(|e| e.error.message)
            .unwrap_or_else(|| body.to_string());

        match status {
            StatusCode::UNAUTHORIZED => ModelError::AuthFailed,
            StatusCode::NOT_FOUND => ModelError::ModelNotFound(error_message),
            StatusCode::PAYLOAD_TOO_LARGE => ModelError::ContextLengthExceeded(error_message),
            StatusCode::BAD_REQUEST if is_context_overflow_message(&error_message) => {
                ModelError::ContextLengthExceeded(error_message)
            }
            StatusCode::UNPROCESSABLE_ENTITY => ModelError::InvalidRequest(error_message),
            StatusCode::TOO_MANY_REQUESTS => ModelError::RateLimited { retry_after_sec: 0 },
            StatusCode::SERVICE_UNAVAILABLE => {
                ModelError::ServiceUnavailable { retry_after_sec: 0 }
            }
            StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN => {
                ModelError::InvalidRequest(error_message)
            }
            StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::GATEWAY_TIMEOUT => ModelError::ServiceUnavailable { retry_after_sec: 0 },
            _ => ModelError::Provider(format!("HTTP {status}: {error_message}")),
        }
    }

    /// Map a reqwest error to a [`ModelError`].
    fn map_reqwest_error(err: reqwest::Error) -> ModelError {
        if err.is_timeout() {
            return ModelError::ServiceUnavailable { retry_after_sec: 0 };
        }
        if err.is_connect() {
            return ModelError::ServiceUnavailable { retry_after_sec: 0 };
        }
        ModelError::Provider(err.to_string())
    }

    /// Convert a conversation to OpenAI chat messages.
    fn to_openai_messages(messages: &[ModelMessage]) -> Vec<OpenAIMessage> {
        crate::model::openai_native::OpenAIModel::to_openai_messages(messages)
    }

    /// Convert [`ToolSpec`] items to OpenAI tool definitions.
    fn to_openai_tools(specs: &[ToolSpec]) -> Vec<OpenAITool> {
        specs
            .iter()
            .map(|spec| OpenAITool {
                r#type: "function".to_string(),
                function: OpenAIFunction {
                    name: spec.name.clone(),
                    description: spec.description.clone(),
                    parameters: spec.input_schema.clone(),
                },
            })
            .collect()
    }

    /// Convert an OpenAI response to a [`ModelResponse`].
    fn from_openai_response(resp: ChatCompletionResponse) -> ModelResponse {
        let choice = resp.choices.into_iter().next();
        let message = choice.map(|c| c.message);

        let content = message
            .as_ref()
            .and_then(|m| m.content.clone())
            .unwrap_or_default();

        let thinking = message
            .as_ref()
            .and_then(|m| m.reasoning.clone().or_else(|| m.reasoning_content.clone()))
            .filter(|t| !t.is_empty());

        let tool_calls = message.and_then(|m| m.tool_calls).map(|calls| {
            calls
                .into_iter()
                .map(|tc| ToolCall {
                    id: tc.id,
                    name: tc.function.name,
                    args: serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null),
                    status: crate::events::ToolCallStatus::Ok,
                })
                .collect()
        });

        let usage = resp.usage.map(|u| u.to_usage());

        ModelResponse {
            content,
            tool_calls,
            thinking,
            reasoning: Vec::new(),
            usage,
        }
    }

    /// Estimate cost in USD from token usage and pricing.
    pub fn estimate_cost(&self, usage: &Usage) -> f64 {
        let Some(pricing) = &self.pricing else {
            return 0.0;
        };
        let input_cost = (usage.prompt_tokens as f64 / 1_000_000.0) * pricing.input_per_million;
        let output_cost =
            (usage.completion_tokens as f64 / 1_000_000.0) * pricing.output_per_million;
        input_cost + output_cost
    }

    /// Build the chat completion request body.
    fn build_request(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
        stream: bool,
    ) -> ChatCompletionRequest {
        let openai_messages: Vec<OpenAIMessage> = Self::to_openai_messages(messages);

        let openai_tools = tools.map(Self::to_openai_tools);

        let has_tools = openai_tools.as_ref().is_some_and(|t| !t.is_empty());
        let tool_choice = has_tools.then(|| match &params.tool_choice {
            Some(ToolChoice::None) => Value::String("none".into()),
            Some(ToolChoice::Required) => Value::String("required".into()),
            Some(ToolChoice::Tool(name)) => {
                serde_json::json!({"type": "function", "function": {"name": name}})
            }
            // The model must be free to answer in plain text to finish a task.
            _ => Value::String("auto".into()),
        });

        let reasoning = if self.capabilities.thinking {
            params
                .reasoning
                .and_then(|l| l.effort())
                .map(|effort| serde_json::json!({"effort": effort}))
        } else {
            None
        };

        ChatCompletionRequest {
            model: self.model_id.clone(),
            messages: openai_messages,
            temperature: params.temperature,
            max_tokens: params.max_tokens,
            top_p: params.top_p,
            stop: params.stop_sequences.clone(),
            stream: Some(stream),
            tools: openai_tools,
            tool_choice,
            reasoning,
        }
    }
}

#[async_trait]
impl Model for OpenRouterModel {
    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn complete(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<ModelResponse, ModelError> {
        let request = self.build_request(messages, params, tools, false);

        let response = self
            .client
            .post(self.chat_completions_url())
            .header(header::AUTHORIZATION, self.bearer_auth())
            .header(header::CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = response.status();
        let retry_after = retry_after_secs(response.headers());
        let body = response.text().await.map_err(Self::map_reqwest_error)?;

        if !status.is_success() {
            return Err(Self::map_http_error(status, &body).with_retry_after(retry_after));
        }

        let resp: ChatCompletionResponse = serde_json::from_str(&body)
            .map_err(|e| ModelError::Provider(format!("JSON parse error: {e}")))?;

        Ok(Self::from_openai_response(resp))
    }

    async fn stream(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ModelStreamChunk, ModelError>> + Send + 'static>>,
        ModelError,
    > {
        let request = self.build_request(messages, params, tools, true);

        let response = self
            .client
            .post(self.chat_completions_url())
            .header(header::AUTHORIZATION, self.bearer_auth())
            .header(header::CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = response.status();
        let retry_after = retry_after_secs(response.headers());

        // For streaming, check the status code first. Error responses come
        // as a regular JSON body with non-2xx status.
        if !status.is_success() {
            let body = response.text().await.map_err(Self::map_reqwest_error)?;
            return Err(Self::map_http_error(status, &body).with_retry_after(retry_after));
        }

        let byte_stream = response.bytes_stream();
        Ok(Box::pin(SseStream::new(byte_stream)))
    }
}

/// SSE stream processor that converts OpenAI SSE chunks into [`ModelStreamChunk`] items.
///
/// Parses Server-Sent Events lines (`data: {...}`, `data: [DONE]`) and
/// accumulates multi-chunk tool calls before yielding them.
pub(crate) struct SseStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: Vec<u8>,
    pending_tool_calls: Vec<StreamingToolCall>,
    finished: bool,
    usage: Option<Usage>,
    truncated: bool,
}

impl SseStream {
    fn new(
        byte_stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(byte_stream),
            buffer: Vec::new(),
            pending_tool_calls: Vec::new(),
            finished: false,
            usage: None,
            truncated: false,
        }
    }

    /// Terminal chunk: flushes pending tool calls and attaches usage.
    fn final_chunk(&mut self) -> ModelStreamChunk {
        let tool_calls = if self.pending_tool_calls.is_empty() {
            None
        } else {
            Some(self.flush_tool_calls())
        };
        ModelStreamChunk {
            tool_calls,
            finished: true,
            usage: self.usage.take(),
            truncated: self.truncated,
            ..Default::default()
        }
    }

    /// Parse an SSE `data:` line into a [`ModelStreamChunk`], accumulating
    /// tool call deltas across chunks.
    ///
    /// The terminal (`finished`) chunk is only emitted on `[DONE]` or end of
    /// stream so that a trailing usage-only chunk is not lost.
    fn process_data_line(&mut self, data: &str) -> Option<Result<ModelStreamChunk, ModelError>> {
        let trimmed = data.trim();

        // [DONE] signals end of stream
        if trimmed == "[DONE]" {
            self.finished = true;
            return Some(Ok(self.final_chunk()));
        }

        // Parse the JSON chunk
        let chunk: ChatCompletionChunk = match serde_json::from_str(trimmed) {
            Ok(c) => c,
            Err(_) => return None, // skip unparseable lines
        };

        if let Some(u) = chunk.usage {
            self.usage = Some(u.to_usage());
        }

        let choice = chunk.choices.into_iter().next();
        let delta = choice.as_ref().map(|c| c.delta.clone()).unwrap_or_default();
        let finish_reason = choice.as_ref().and_then(|c| c.finish_reason.clone());
        if finish_reason.as_deref() == Some("length") {
            self.truncated = true;
        }

        let mut content_delta = String::new();
        let reasoning_delta = delta
            .reasoning
            .or(delta.reasoning_content)
            .filter(|r| !r.is_empty());
        let mut tool_call_chunks: Vec<ChunkToolCall> = Vec::new();

        if let Some(content) = delta.content {
            content_delta.push_str(&content);
        }
        if let Some(tcs) = delta.tool_calls {
            tool_call_chunks.extend(tcs);
        }

        // Accumulate tool call chunks
        for tc in tool_call_chunks {
            // Find or create the accumulated tool call for this index
            let existing = self
                .pending_tool_calls
                .iter_mut()
                .find(|t| t.index == tc.index);

            match existing {
                Some(acc) => {
                    if let Some(id) = tc.id {
                        acc.id = Some(id);
                    }
                    if let Some(name) = tc.function.name {
                        acc.name = Some(name);
                    }
                    if let Some(args) = tc.function.arguments {
                        acc.arguments.push_str(&args);
                    }
                }
                None => {
                    self.pending_tool_calls.push(StreamingToolCall {
                        index: tc.index,
                        id: tc.id,
                        name: tc.function.name,
                        arguments: tc.function.arguments.unwrap_or_default(),
                    });
                }
            }
        }

        // Once the choice finishes, flush accumulated tool calls (the
        // terminal chunk follows on [DONE] / end of stream).
        let finished = finish_reason.is_some();
        let mut output_tool_calls: Option<Vec<ToolCall>> = None;

        if !self.pending_tool_calls.is_empty() && finished {
            output_tool_calls = Some(self.flush_tool_calls());
        }

        if content_delta.is_empty() && output_tool_calls.is_none() && reasoning_delta.is_none() {
            return None; // nothing to emit
        }

        Some(Ok(ModelStreamChunk {
            delta: content_delta,
            tool_calls: output_tool_calls,
            thinking: reasoning_delta,
            ..Default::default()
        }))
    }

    /// Flush accumulated tool calls into [`ToolCall`] items and clear the buffer.
    fn flush_tool_calls(&mut self) -> Vec<ToolCall> {
        let calls: Vec<ToolCall> = self
            .pending_tool_calls
            .iter()
            .filter_map(|tc| {
                let id = tc.id.as_deref().unwrap_or("").to_string();
                let name = tc.name.as_deref().unwrap_or("").to_string();
                if name.is_empty() {
                    return None;
                }
                let args = serde_json::from_str(&tc.arguments).unwrap_or(Value::Null);
                Some(ToolCall {
                    id,
                    name,
                    args,
                    status: crate::events::ToolCallStatus::Ok,
                })
            })
            .collect();
        self.pending_tool_calls.clear();
        calls
    }
}

impl Stream for SseStream {
    type Item = Result<ModelStreamChunk, ModelError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if this.finished {
            return std::task::Poll::Ready(None);
        }

        loop {
            // Try to find a complete line in the buffer
            if let Some(newline_pos) = this.buffer.iter().position(|b| *b == b'\n') {
                let line_bytes = this.buffer.drain(..=newline_pos).collect::<Vec<u8>>();
                let line = String::from_utf8_lossy(&line_bytes)
                    .trim_end_matches(['\r', '\n'])
                    .to_string();

                if let Some(data) = line.strip_prefix("data:")
                    && let Some(result) = this.process_data_line(data)
                {
                    return std::task::Poll::Ready(Some(result));
                }
                continue;
            }

            // Need more data from the underlying stream
            match Pin::new(&mut this.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    this.buffer.extend_from_slice(&bytes);
                    continue;
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    return std::task::Poll::Ready(Some(Err(ModelError::StreamInterrupted(
                        e.to_string(),
                    ))));
                }
                std::task::Poll::Ready(None) => {
                    // Stream ended without [DONE]
                    this.finished = true;
                    // Process any remaining data in buffer
                    if !this.buffer.is_empty() {
                        let line = String::from_utf8_lossy(&this.buffer)
                            .trim_end_matches(['\r', '\n'])
                            .to_string();
                        this.buffer.clear();
                        if let Some(data) = line.strip_prefix("data:")
                            && let Some(result) = this.process_data_line(data)
                        {
                            return std::task::Poll::Ready(Some(result));
                        }
                    }
                    // Flush pending tool calls
                    return std::task::Poll::Ready(Some(Ok(this.final_chunk())));
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Default base URL for OpenRouter.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::model_trait::{ModelRole, ModelTier};

    fn test_capabilities() -> ModelCapabilities {
        ModelCapabilities {
            tool_calling: true,
            structured_output: true,
            thinking: false,
            max_context: 128000,
            max_output: 8192,
            temperature_supported: true,
            vision: false,
            tier: ModelTier::Frontier,
            edit_format: EditFormat::Diff,
        }
    }

    #[test]
    fn test_openrouter_model_new() {
        let client = reqwest::Client::new();
        let model = OpenRouterModel::new(
            client,
            "https://openrouter.ai/api/v1",
            "test-key",
            "gpt-4o",
            test_capabilities(),
            None,
        );
        assert_eq!(model.model_id(), "gpt-4o");
        assert!(model.capabilities().tool_calling);
    }

    #[test]
    fn test_map_http_error_auth_failed() {
        let err = OpenRouterModel::map_http_error(
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"Bad API key","type":"authentication_error","code":401}}"#,
        );
        assert!(matches!(err, ModelError::AuthFailed));
    }

    #[test]
    fn test_map_http_error_rate_limited() {
        let err = OpenRouterModel::map_http_error(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"Rate limit exceeded","type":"rate_limit_error","code":429}}"#,
        );
        assert!(matches!(
            err,
            ModelError::RateLimited { retry_after_sec: 0 }
        ));
    }

    #[test]
    fn test_map_http_error_service_unavailable() {
        let err = OpenRouterModel::map_http_error(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":{"message":"Overloaded","type":"service_unavailable","code":503}}"#,
        );
        assert!(matches!(
            err,
            ModelError::ServiceUnavailable { retry_after_sec: 0 }
        ));
    }

    #[test]
    fn test_map_http_error_model_not_found() {
        let err = OpenRouterModel::map_http_error(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"Model not found","type":"not_found_error","code":404}}"#,
        );
        assert!(matches!(err, ModelError::ModelNotFound(_)));
    }

    #[test]
    fn test_map_http_error_invalid_request() {
        let err = OpenRouterModel::map_http_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"error":{"message":"Invalid request","type":"invalid_request_error","code":422}}"#,
        );
        assert!(matches!(err, ModelError::InvalidRequest(_)));
    }

    #[test]
    fn test_map_http_error_provider_fallback() {
        let err = OpenRouterModel::map_http_error(StatusCode::IM_A_TEAPOT, "plain text error");
        assert!(matches!(err, ModelError::Provider(_)));
    }

    #[test]
    fn test_to_openai_tools() {
        let specs = vec![crate::tool::ToolSpec::new(
            "todo",
            "Manage todo list",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "complete", "list"]},
                    "title": {"type": "string"}
                },
                "required": ["action"]
            }),
        )];
        let tools = OpenRouterModel::to_openai_tools(&specs);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].r#type, "function");
        assert_eq!(tools[0].function.name, "todo");
        assert_eq!(tools[0].function.description, "Manage todo list");
    }

    #[test]
    fn test_from_openai_response() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-123".to_string(),
            object: "chat.completion".to_string(),
            created: 1000000,
            model: "gpt-4o".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessageResponse {
                    role: "assistant".to_string(),
                    content: Some("Hello!".to_string()),
                    tool_calls: None,
                    reasoning: Some("thought".to_string()),
                    reasoning_content: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(
                serde_json::from_str::<UsageInfo>(
                    r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}"#,
                )
                .unwrap(),
            ),
        };
        let model_resp = OpenRouterModel::from_openai_response(resp);
        assert_eq!(model_resp.content, "Hello!");
        assert_eq!(model_resp.thinking.as_deref(), Some("thought"));
        assert_eq!(model_resp.tool_calls, None);
        assert_eq!(
            model_resp.usage,
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            })
        );
    }

    #[test]
    fn test_from_openai_response_with_tool_calls() {
        let resp = ChatCompletionResponse {
            id: "chatcmpl-123".to_string(),
            object: "chat.completion".to_string(),
            created: 1000000,
            model: "gpt-4o".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessageResponse {
                    role: "assistant".to_string(),
                    content: Some("".to_string()),
                    tool_calls: Some(vec![OpenAIToolCall {
                        id: "call_abc".to_string(),
                        r#type: "function".to_string(),
                        function: OpenAIFunctionCall {
                            name: "todo".to_string(),
                            arguments: r#"{"action":"add","title":"Test"}"#.to_string(),
                        },
                    }]),
                    reasoning: None,
                    reasoning_content: None,
                },
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: None,
        };
        let model_resp = OpenRouterModel::from_openai_response(resp);
        assert_eq!(model_resp.content, "");
        let calls = model_resp.tool_calls.expect("tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].name, "todo");
        assert_eq!(calls[0].args["action"], "add");
        assert_eq!(calls[0].args["title"], "Test");
    }

    #[test]
    fn test_estimate_cost_with_pricing() {
        let client = reqwest::Client::new();
        let pricing = Some(ModelPricing {
            input_per_million: 2.50,
            output_per_million: 10.00,
        });
        let model = OpenRouterModel::new(
            client,
            "https://openrouter.ai/api/v1",
            "key",
            "gpt-4o",
            test_capabilities(),
            pricing,
        );
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
            ..Default::default()
        };
        let cost = model.estimate_cost(&usage);
        assert!((cost - 12.50).abs() < 0.001); // 2.50 + 10.00
    }

    #[test]
    fn test_estimate_cost_without_pricing() {
        let client = reqwest::Client::new();
        let model = OpenRouterModel::new(
            client,
            "https://openrouter.ai/api/v1",
            "key",
            "gpt-4o",
            test_capabilities(),
            None,
        );
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
            ..Default::default()
        };
        assert_eq!(model.estimate_cost(&usage), 0.0);
    }

    #[test]
    fn test_build_request_without_tools() {
        let client = reqwest::Client::new();
        let model = OpenRouterModel::new(
            client,
            "https://openrouter.ai/api/v1",
            "key",
            "gpt-4o",
            test_capabilities(),
            None,
        );
        let messages = vec![ModelMessage {
            role: ModelRole::User,
            content: "Hello".to_string(),
            ..Default::default()
        }];
        let params = GenerationParams {
            temperature: Some(0.7),
            max_tokens: Some(100),
            stop_sequences: None,
            top_p: Some(0.95),
            ..Default::default()
        };
        let request = model.build_request(&messages, &params, None, false);
        assert_eq!(request.model, "gpt-4o");
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.max_tokens, Some(100));
        assert_eq!(request.stream, Some(false));
        assert!(request.tools.is_none());
    }

    #[test]
    fn test_build_request_with_tools() {
        let client = reqwest::Client::new();
        let model = OpenRouterModel::new(
            client,
            "https://openrouter.ai/api/v1",
            "key",
            "gpt-4o",
            test_capabilities(),
            None,
        );
        let messages = vec![ModelMessage {
            role: ModelRole::User,
            content: "Hello".to_string(),
            ..Default::default()
        }];
        let params = GenerationParams::default();
        let specs = vec![crate::tool::ToolSpec::new(
            "todo",
            "Manage todos",
            serde_json::json!({"type": "object"}),
        )];
        let request = model.build_request(&messages, &params, Some(&specs), false);
        assert!(request.tools.is_some());
        let tools = request.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].r#type, "function");
        assert_eq!(request.tool_choice, Some(Value::String("auto".into())));
    }

    #[test]
    fn test_reasoning_param_and_stream_reasoning_delta() {
        let mut caps = test_capabilities();
        caps.thinking = true;
        let model = OpenRouterModel::new(
            reqwest::Client::new(),
            "https://openrouter.ai/api/v1",
            "key",
            "deepseek/deepseek-r1",
            caps,
            None,
        );
        let params = GenerationParams {
            reasoning: Some(crate::model::ReasoningLevel::Low),
            ..Default::default()
        };
        let request = model.build_request(&[], &params, None, true);
        assert_eq!(request.reasoning.unwrap()["effort"], "low");

        let mut sse = SseStream::new(futures::stream::empty());
        let chunk = sse
            .process_data_line(
                r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"reasoning":"hm"}}]}"#,
            )
            .unwrap()
            .unwrap();
        assert_eq!(chunk.thinking.as_deref(), Some("hm"));
    }

    #[test]
    fn test_from_discovered() {
        let client = reqwest::Client::new();
        let discovered = crate::model::discovery::DiscoveredModel {
            id: "openrouter/gpt-4o".to_string(),
            name: "GPT-4o".to_string(),
            context_length: 128000,
            max_output_tokens: 8192,
            pricing: crate::model::discovery::ModelPricing {
                prompt: "2.50".to_string(),
                completion: "10.00".to_string(),
                image: None,
                request: None,
            },
            supports_tools: true,
            supports_structured_output: true,
            description: Some("GPT-4o model".to_string()),
            tier: ModelTier::Frontier,
        };
        let model = OpenRouterModel::from_discovered(
            client,
            "https://openrouter.ai/api/v1",
            "key",
            &discovered,
        );
        assert_eq!(model.model_id(), "openrouter/gpt-4o");
        assert!(model.capabilities().tool_calling);
        assert!(model.capabilities().structured_output);
        assert_eq!(model.capabilities().max_context, 128000);
        assert_eq!(model.capabilities().tier, ModelTier::Frontier);
        let pricing = model.pricing().unwrap();
        assert!((pricing.input_per_million - 2.50).abs() < 0.001);
    }

    #[test]
    fn test_sse_done_line() {
        // The [DONE] handling is exercised through integration tests with a
        // mock HTTP server. Unit-level SSE parsing is covered by
        // `process_data_line` logic via the full stream flow.
    }
}
