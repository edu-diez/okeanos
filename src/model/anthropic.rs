//! Anthropic/Claude model provider.
//!
//! Implements the [`Model`] trait for the Anthropic Messages API
//! with native tool calling, streaming via SSE, error mapping to
//! [`ModelError`] variants, and cost estimation.

use crate::context::tokens::ModelPricing;
use crate::events::ToolCall;
use crate::model::model_trait::{
    GenerationParams, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse, ModelRole,
    ModelStreamChunk, Usage,
};
#[cfg(test)]
use crate::tool::builtin::EditFormat;
use crate::tool::tool_trait::ToolSpec;
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;

/// Anthropic Messages API request.
#[derive(Debug, Clone, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

/// Anthropic message format.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContentBlock>,
}

/// Content block for Anthropic messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

/// Anthropic tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
}

/// Anthropic tool choice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

/// Anthropic Messages API response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicResponse {
    id: String,
    r#type: String,
    role: String,
    content: Vec<AnthropicContentBlock>,
    model: String,
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
    usage: AnthropicUsage,
}

/// Anthropic token usage.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicUsage {
    input_tokens: usize,
    output_tokens: usize,
    cache_creation_input_tokens: Option<usize>,
    cache_read_input_tokens: Option<usize>,
}

/// Anthropic API error response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicErrorResponse {
    error: AnthropicErrorBody,
}

/// Anthropic API error body.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicErrorBody {
    message: String,
    r#type: String,
}

/// Streaming chunk from Anthropic API.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicStreamChunk {
    r#type: String,
    message: Option<AnthropicStreamMessage>,
    delta: Option<AnthropicStreamDelta>,
    usage: Option<AnthropicUsage>,
}

/// Streaming message (initial message_start event).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicStreamMessage {
    id: String,
    r#type: String,
    role: String,
    content: Vec<AnthropicContentBlock>,
    model: String,
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
    usage: AnthropicUsage,
}

/// Streaming delta (content_block_delta, content_block_start, content_block_stop).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AnthropicStreamDelta {
    #[serde(rename = "type")]
    delta_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    index: Option<usize>,
}

/// Accumulated state for a streaming tool call.
#[derive(Debug, Clone, Default)]
struct StreamingToolCall {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    input: String,
}

/// Accumulated thinking state.
#[derive(Debug, Clone, Default)]
struct StreamingThinking {
    thinking: String,
    signature: Option<String>,
}

/// Anthropic/Claude model provider.
pub struct AnthropicModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model_id: String,
    capabilities: ModelCapabilities,
    pricing: Option<ModelPricing>,
}

impl AnthropicModel {
    /// Create a new Anthropic model provider.
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

    /// Returns the per-model pricing, if available.
    pub fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    fn bearer_auth(&self) -> header::HeaderValue {
        header::HeaderValue::from_str(&format!("Bearer {}", self.api_key)).unwrap()
    }

    fn anthropic_version(&self) -> header::HeaderValue {
        header::HeaderValue::from_static("2023-06-01")
    }

    /// Map an HTTP error response to a [`ModelError`].
    fn map_http_error(status: StatusCode, body: &str) -> ModelError {
        let error_message = serde_json::from_str::<AnthropicErrorResponse>(body)
            .ok()
            .map(|e| e.error.message)
            .unwrap_or_else(|| body.to_string());

        match status {
            StatusCode::UNAUTHORIZED => ModelError::AuthFailed,
            StatusCode::NOT_FOUND => ModelError::ModelNotFound(error_message),
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

    /// Convert a [`ModelMessage`] to an Anthropic message format.
    fn to_anthropic_messages(messages: &[ModelMessage]) -> (Option<String>, Vec<AnthropicMessage>) {
        let mut system_prompt = None;
        let mut anthropic_messages = Vec::new();

        for msg in messages {
            match msg.role {
                ModelRole::System => {
                    system_prompt = Some(msg.content.clone());
                }
                ModelRole::User => {
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: vec![AnthropicContentBlock::Text {
                            text: msg.content.clone(),
                        }],
                    });
                }
                ModelRole::Assistant => {
                    let mut content = Vec::new();
                    if !msg.content.is_empty() {
                        content.push(AnthropicContentBlock::Text {
                            text: msg.content.clone(),
                        });
                    }
                    if let Some(tool_calls) = &msg.tool_calls {
                        for tc in tool_calls {
                            content.push(AnthropicContentBlock::ToolUse {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                input: tc.args.clone(),
                            });
                        }
                    }
                    if let Some(thinking) = &msg.thinking {
                        // Anthropic uses a separate thinking block with signature
                        // For replay, we just include the thinking as text for now
                        content.push(AnthropicContentBlock::Text {
                            text: format!("<thinking>{}</thinking>", thinking),
                        });
                    }
                    if !content.is_empty() {
                        anthropic_messages.push(AnthropicMessage {
                            role: "assistant".to_string(),
                            content,
                        });
                    }
                }
                ModelRole::Tool => {
                    // Tool result message
                    if let Some(tool_call_id) = &msg.tool_call_id {
                        anthropic_messages.push(AnthropicMessage {
                            role: "user".to_string(),
                            content: vec![AnthropicContentBlock::ToolResult {
                                tool_use_id: tool_call_id.clone(),
                                content: Value::String(msg.content.clone()),
                                is_error: None,
                            }],
                        });
                    }
                }
            }
        }

        (system_prompt, anthropic_messages)
    }

    /// Convert [`ToolSpec`] items to Anthropic tool definitions.
    fn to_anthropic_tools(specs: &[ToolSpec]) -> Vec<AnthropicTool> {
        specs
            .iter()
            .map(|spec| AnthropicTool {
                name: spec.name.clone(),
                description: spec.description.clone(),
                input_schema: spec.input_schema.clone(),
            })
            .collect()
    }

    /// Convert an Anthropic response to a [`ModelResponse`].
    fn from_anthropic_response(resp: AnthropicResponse) -> ModelResponse {
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut thinking = None;

        for block in resp.content {
            match block {
                AnthropicContentBlock::Text { text } => {
                    // Check if this is thinking content
                    if text.starts_with("<thinking>") && text.ends_with("</thinking>") {
                        thinking = Some(
                            text["<thinking>".len()..text.len() - "</thinking>".len()].to_string(),
                        );
                    } else {
                        content.push_str(&text);
                    }
                }
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        args: input,
                        status: crate::events::ToolCallStatus::Ok,
                    });
                }
                AnthropicContentBlock::Thinking {
                    thinking: t,
                    signature: _,
                } => {
                    thinking = Some(t);
                    // Signature could be stored for native compaction
                }
                _ => {}
            }
        }

        let usage = Some(Usage {
            prompt_tokens: resp.usage.input_tokens,
            completion_tokens: resp.usage.output_tokens,
            total_tokens: resp.usage.input_tokens + resp.usage.output_tokens,
        });

        ModelResponse {
            content,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            thinking,
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

    /// Build the Anthropic request body.
    fn build_request(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
        stream: bool,
    ) -> AnthropicRequest {
        let (system, anthropic_messages) = Self::to_anthropic_messages(messages);

        let anthropic_tools = tools.map(Self::to_anthropic_tools);

        let tool_choice = if anthropic_tools
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false)
        {
            Some(AnthropicToolChoice::Auto)
        } else {
            None
        };

        AnthropicRequest {
            model: self.model_id.clone(),
            messages: anthropic_messages,
            system,
            temperature: params.temperature,
            max_tokens: Some(
                params
                    .max_tokens
                    .unwrap_or(self.capabilities.max_output.min(8192)),
            ),
            top_p: params.top_p,
            stop_sequences: params.stop_sequences.clone(),
            stream: Some(stream),
            tools: anthropic_tools,
            tool_choice,
        }
    }
}

#[async_trait]
impl Model for AnthropicModel {
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
            .post(self.messages_url())
            .header(header::AUTHORIZATION, self.bearer_auth())
            .header(header::CONTENT_TYPE, "application/json")
            .header("anthropic-version", self.anthropic_version())
            .json(&request)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = response.status();
        let body = response.text().await.map_err(Self::map_reqwest_error)?;

        if !status.is_success() {
            return Err(Self::map_http_error(status, &body));
        }

        let resp: AnthropicResponse = serde_json::from_str(&body)
            .map_err(|e| ModelError::Provider(format!("JSON parse error: {e}")))?;

        Ok(Self::from_anthropic_response(resp))
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
            .post(self.messages_url())
            .header(header::AUTHORIZATION, self.bearer_auth())
            .header(header::CONTENT_TYPE, "application/json")
            .header("anthropic-version", self.anthropic_version())
            .json(&request)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = response.status();

        // For streaming, check the status code first.
        if !status.is_success() {
            let body = response.text().await.map_err(Self::map_reqwest_error)?;
            return Err(Self::map_http_error(status, &body));
        }

        let byte_stream = response.bytes_stream();
        Ok(Box::pin(SseStream::new(byte_stream)))
    }
}

/// SSE stream processor that converts Anthropic SSE chunks into [`ModelStreamChunk`] items.
pub(crate) struct SseStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: Vec<u8>,
    pending_tool_calls: Vec<StreamingToolCall>,
    pending_thinking: StreamingThinking,
    finished: bool,
    message_started: bool,
}

impl SseStream {
    fn new(
        byte_stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(byte_stream),
            buffer: Vec::new(),
            pending_tool_calls: Vec::new(),
            pending_thinking: StreamingThinking::default(),
            finished: false,
            message_started: false,
        }
    }

    /// Parse an SSE `data:` line into a [`ModelStreamChunk`].
    fn process_data_line(&mut self, data: &str) -> Option<Result<ModelStreamChunk, ModelError>> {
        let trimmed = data.trim();

        if trimmed == "[DONE]" {
            self.finished = true;
            // Flush any accumulated tool calls
            if !self.pending_tool_calls.is_empty() {
                let tool_calls = self.flush_tool_calls();
                return Some(Ok(ModelStreamChunk {
                    delta: String::new(),
                    tool_calls: Some(tool_calls),
                    thinking: self.flush_thinking(),
                    finished: true,
                }));
            }
            return Some(Ok(ModelStreamChunk {
                delta: String::new(),
                tool_calls: None,
                thinking: self.flush_thinking(),
                finished: true,
            }));
        }

        // Parse the JSON chunk
        let chunk: AnthropicStreamChunk = match serde_json::from_str(trimmed) {
            Ok(c) => c,
            Err(_) => return None, // skip unparseable lines
        };

        match chunk.r#type.as_str() {
            "message_start" => {
                self.message_started = true;
                None // Don't emit chunk for message_start
            }
            "content_block_start" => {
                if let Some(delta) = chunk.delta {
                    if delta.delta_type == "tool_use" {
                        // Start accumulating a new tool call
                        let index = delta.index.unwrap_or(self.pending_tool_calls.len());
                        self.pending_tool_calls.push(StreamingToolCall {
                            index,
                            id: None,
                            name: delta.name,
                            input: String::new(),
                        });
                    } else if delta.delta_type == "thinking" {
                        self.pending_thinking = StreamingThinking::default();
                    }
                }
                None
            }
            "content_block_delta" => {
                if let Some(delta) = chunk.delta {
                    match delta.delta_type.as_str() {
                        "text_delta" => {
                            if let Some(text) = delta.text {
                                return Some(Ok(ModelStreamChunk {
                                    delta: text,
                                    tool_calls: None,
                                    thinking: None,
                                    finished: false,
                                }));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(thinking) = delta.thinking {
                                self.pending_thinking.thinking.push_str(&thinking);
                            }
                            if let Some(signature) = delta.signature {
                                self.pending_thinking.signature = Some(signature);
                            }
                        }
                        "input_json_delta" => {
                            if let Some(input) = delta.input {
                                let idx = delta.index.unwrap_or(0);
                                if let Some(tc) =
                                    self.pending_tool_calls.iter_mut().find(|t| t.index == idx)
                                {
                                    tc.input.push_str(&input.to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                None
            }
            "content_block_stop" => {
                // Tool call or thinking block finished
                if let Some(idx) = chunk.delta.and_then(|d| d.index) {
                    // Check if this was a tool call
                    if let Some(pos) = self.pending_tool_calls.iter().position(|t| t.index == idx) {
                        let tc = self.pending_tool_calls.remove(pos);
                        if let Some(name) = tc.name {
                            let args = serde_json::from_str(&tc.input).unwrap_or(Value::Null);
                            return Some(Ok(ModelStreamChunk {
                                delta: String::new(),
                                tool_calls: Some(vec![ToolCall {
                                    id: tc.id.unwrap_or_default(),
                                    name,
                                    args,
                                    status: crate::events::ToolCallStatus::Ok,
                                }]),
                                thinking: None,
                                finished: false,
                            }));
                        }
                    }
                }
                None
            }
            "message_delta" => {
                if let Some(_usage) = chunk.usage {
                    // Update usage - but we don't emit a chunk for this
                }
                None
            }
            "message_stop" => {
                self.finished = true;
                // Flush any pending tool calls
                if !self.pending_tool_calls.is_empty() {
                    let tool_calls = self.flush_tool_calls();
                    return Some(Ok(ModelStreamChunk {
                        delta: String::new(),
                        tool_calls: Some(tool_calls),
                        thinking: self.flush_thinking(),
                        finished: true,
                    }));
                }
                Some(Ok(ModelStreamChunk {
                    delta: String::new(),
                    tool_calls: None,
                    thinking: self.flush_thinking(),
                    finished: true,
                }))
            }
            _ => None,
        }
    }

    /// Flush accumulated tool calls into [`ToolCall`] items and clear the buffer.
    fn flush_tool_calls(&mut self) -> Vec<ToolCall> {
        let calls: Vec<ToolCall> = self
            .pending_tool_calls
            .drain(..)
            .filter_map(|tc| {
                let name = tc.name?;
                let args = serde_json::from_str(&tc.input).unwrap_or(Value::Null);
                Some(ToolCall {
                    id: tc.id.unwrap_or_default(),
                    name,
                    args,
                    status: crate::events::ToolCallStatus::Ok,
                })
            })
            .collect();
        calls
    }

    fn flush_thinking(&mut self) -> Option<String> {
        if self.pending_thinking.thinking.is_empty() {
            None
        } else {
            let thinking = std::mem::take(&mut self.pending_thinking.thinking);
            Some(thinking)
        }
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

                if !line.is_empty() {
                    if let Some(data) = line.strip_prefix("data: ")
                        && let Some(result) = this.process_data_line(data)
                    {
                        return std::task::Poll::Ready(Some(result));
                    } else if let Some(data) = line.strip_prefix("data:")
                        && let Some(result) = this.process_data_line(data)
                    {
                        return std::task::Poll::Ready(Some(result));
                    }
                    // Skip empty lines, event: lines, etc.
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
                    return std::task::Poll::Ready(Some(Err(ModelError::Streaming(e.to_string()))));
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
                        if !line.is_empty() {
                            if let Some(data) = line.strip_prefix("data: ")
                                && let Some(result) = this.process_data_line(data)
                            {
                                return std::task::Poll::Ready(Some(result));
                            } else if let Some(data) = line.strip_prefix("data:")
                                && let Some(result) = this.process_data_line(data)
                            {
                                return std::task::Poll::Ready(Some(result));
                            }
                        }
                    }
                    // Flush pending tool calls
                    if !this.pending_tool_calls.is_empty() {
                        let tool_calls = this.flush_tool_calls();
                        return std::task::Poll::Ready(Some(Ok(ModelStreamChunk {
                            delta: String::new(),
                            tool_calls: Some(tool_calls),
                            thinking: this.flush_thinking(),
                            finished: true,
                        })));
                    }
                    return std::task::Poll::Ready(Some(Ok(ModelStreamChunk {
                        delta: String::new(),
                        tool_calls: None,
                        thinking: this.flush_thinking(),
                        finished: true,
                    })));
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Default base URL for Anthropic.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::model_trait::ModelTier;

    fn test_capabilities() -> ModelCapabilities {
        ModelCapabilities {
            tool_calling: true,
            structured_output: true,
            thinking: true,
            max_context: 200000,
            max_output: 8192,
            temperature_supported: true,
            tier: ModelTier::Frontier,
            edit_format: EditFormat::Diff,
        }
    }

    #[test]
    fn test_anthropic_model_new() {
        let client = reqwest::Client::new();
        let model = AnthropicModel::new(
            client,
            "https://api.anthropic.com",
            "test-key",
            "claude-3-5-sonnet",
            test_capabilities(),
            None,
        );
        assert_eq!(model.model_id(), "claude-3-5-sonnet");
        assert!(model.capabilities().tool_calling);
    }

    #[test]
    fn test_map_http_error_auth_failed() {
        let err = AnthropicModel::map_http_error(
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"Bad API key","type":"authentication_error"}}"#,
        );
        assert!(matches!(err, ModelError::AuthFailed));
    }

    #[test]
    fn test_map_http_error_rate_limited() {
        let err = AnthropicModel::map_http_error(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"Rate limit exceeded","type":"rate_limit_error"}}"#,
        );
        assert!(matches!(
            err,
            ModelError::RateLimited { retry_after_sec: 0 }
        ));
    }

    #[test]
    fn test_to_anthropic_tools() {
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
        let tools = AnthropicModel::to_anthropic_tools(&specs);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "todo");
        assert_eq!(tools[0].description, "Manage todo list");
    }

    #[test]
    fn test_from_anthropic_response() {
        let resp = AnthropicResponse {
            id: "msg_123".to_string(),
            r#type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![AnthropicContentBlock::Text {
                text: "Hello!".to_string(),
            }],
            model: "claude-3-5-sonnet".to_string(),
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        let model_resp = AnthropicModel::from_anthropic_response(resp);
        assert_eq!(model_resp.content, "Hello!");
        assert_eq!(model_resp.tool_calls, None);
        assert_eq!(
            model_resp.usage,
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15
            })
        );
    }

    #[test]
    fn test_from_anthropic_response_with_tool_calls() {
        let resp = AnthropicResponse {
            id: "msg_123".to_string(),
            r#type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![AnthropicContentBlock::ToolUse {
                id: "toolu_abc".to_string(),
                name: "todo".to_string(),
                input: serde_json::json!({"action":"add","title":"Test"}),
            }],
            model: "claude-3-5-sonnet".to_string(),
            stop_reason: Some("tool_use".to_string()),
            stop_sequence: None,
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        let model_resp = AnthropicModel::from_anthropic_response(resp);
        assert_eq!(model_resp.content, "");
        let calls = model_resp.tool_calls.expect("tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_abc");
        assert_eq!(calls[0].name, "todo");
        assert_eq!(calls[0].args["action"], "add");
        assert_eq!(calls[0].args["title"], "Test");
    }

    #[test]
    fn test_from_anthropic_response_with_thinking() {
        let resp = AnthropicResponse {
            id: "msg_123".to_string(),
            r#type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![
                AnthropicContentBlock::Thinking {
                    thinking: "Let me think about this...".to_string(),
                    signature: "sig_123".to_string(),
                },
                AnthropicContentBlock::Text {
                    text: "Here's my answer.".to_string(),
                },
            ],
            model: "claude-3-5-sonnet".to_string(),
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        let model_resp = AnthropicModel::from_anthropic_response(resp);
        assert_eq!(model_resp.content, "Here's my answer.");
        assert_eq!(
            model_resp.thinking,
            Some("Let me think about this...".to_string())
        );
    }

    #[test]
    fn test_estimate_cost_with_pricing() {
        let client = reqwest::Client::new();
        let pricing = Some(ModelPricing {
            input_per_million: 3.00,
            output_per_million: 15.00,
        });
        let model = AnthropicModel::new(
            client,
            "https://api.anthropic.com",
            "key",
            "claude-3-5-sonnet",
            test_capabilities(),
            pricing,
        );
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
        };
        let cost = model.estimate_cost(&usage);
        assert!((cost - 18.00).abs() < 0.001); // 3.00 + 15.00
    }

    #[test]
    fn test_estimate_cost_without_pricing() {
        let client = reqwest::Client::new();
        let model = AnthropicModel::new(
            client,
            "https://api.anthropic.com",
            "key",
            "claude-3-5-sonnet",
            test_capabilities(),
            None,
        );
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
        };
        assert_eq!(model.estimate_cost(&usage), 0.0);
    }

    #[test]
    fn test_to_anthropic_messages_user() {
        let messages = vec![ModelMessage {
            role: ModelRole::User,
            content: "Hello".to_string(),
            tool_calls: None,
            tool_call_id: None,
            thinking: None,
        }];
        let (system, anthropic_msgs) = AnthropicModel::to_anthropic_messages(&messages);
        assert!(system.is_none());
        assert_eq!(anthropic_msgs.len(), 1);
        assert_eq!(anthropic_msgs[0].role, "user");
        assert!(matches!(
            &anthropic_msgs[0].content[0],
            AnthropicContentBlock::Text { text } if text == "Hello"
        ));
    }

    #[test]
    fn test_to_anthropic_messages_system() {
        let messages = vec![ModelMessage {
            role: ModelRole::System,
            content: "You are a helpful assistant".to_string(),
            tool_calls: None,
            tool_call_id: None,
            thinking: None,
        }];
        let (system, anthropic_msgs) = AnthropicModel::to_anthropic_messages(&messages);
        assert_eq!(system, Some("You are a helpful assistant".to_string()));
        assert_eq!(anthropic_msgs.len(), 0);
    }

    #[test]
    fn test_to_anthropic_messages_tool_result() {
        let messages = vec![ModelMessage {
            role: ModelRole::Tool,
            content: "File contents here".to_string(),
            tool_calls: None,
            tool_call_id: Some("toolu_123".to_string()),
            thinking: None,
        }];
        let (system, anthropic_msgs) = AnthropicModel::to_anthropic_messages(&messages);
        assert!(system.is_none());
        assert_eq!(anthropic_msgs.len(), 1);
        assert_eq!(anthropic_msgs[0].role, "user");
        assert!(matches!(
            &anthropic_msgs[0].content[0],
            AnthropicContentBlock::ToolResult { tool_use_id, content, .. }
            if tool_use_id == "toolu_123" && content == "File contents here"
        ));
    }
}
