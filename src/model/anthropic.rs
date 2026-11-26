//! Anthropic/Claude model provider.
//!
//! Implements the [`Model`] trait for the Anthropic Messages API
//! with native tool calling, streaming via SSE, error mapping to
//! [`ModelError`] variants, and cost estimation.

use crate::context::compaction::{
    CompactionCheckpoint, NativeCompactionRequest, NativeCompactionResponse,
};
use crate::context::tokens::ModelPricing;
use crate::events::ToolCall;
use crate::model::model_trait::{
    GenerationParams, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse, ModelRole,
    ModelStreamChunk, ReasoningBlock, ToolChoice, Usage, is_context_overflow_message,
    retry_after_secs,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
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
    Image {
        source: AnthropicImageSource,
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

/// Base64 image payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    kind: String,
    media_type: String,
    data: String,
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
    None,
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
    #[serde(default)]
    input_tokens: usize,
    #[serde(default)]
    output_tokens: usize,
    cache_creation_input_tokens: Option<usize>,
    cache_read_input_tokens: Option<usize>,
}

impl AnthropicUsage {
    /// Total prompt size: Anthropic reports cached tokens separately from `input_tokens`.
    fn total_input(&self) -> usize {
        self.input_tokens
            + self.cache_creation_input_tokens.unwrap_or(0)
            + self.cache_read_input_tokens.unwrap_or(0)
    }

    fn to_usage(&self) -> Usage {
        Usage {
            prompt_tokens: self.total_input(),
            completion_tokens: self.output_tokens,
            total_tokens: self.total_input() + self.output_tokens,
            cache_read_tokens: self.cache_read_input_tokens.unwrap_or(0),
            cache_write_tokens: self.cache_creation_input_tokens.unwrap_or(0),
            reasoning_tokens: 0,
        }
    }
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
    /// Real API shape: block start payload (`content_block_start`).
    #[serde(default)]
    content_block: Option<AnthropicStreamDelta>,
    /// Real API shape: block index lives at the top level.
    #[serde(default)]
    index: Option<usize>,
    usage: Option<AnthropicUsage>,
    /// Present on mid-stream `error` events.
    #[serde(default)]
    error: Option<AnthropicErrorBody>,
}
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
    #[serde(rename = "type", default)]
    delta_type: String,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    id: Option<String>,
    /// `redacted_thinking` payload.
    #[serde(default)]
    data: Option<String>,
    /// Real API: incremental tool-input JSON text.
    #[serde(default)]
    partial_json: Option<String>,
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
    index: Option<usize>,
    thinking: String,
    signature: Option<String>,
}

/// Provider tag stored on [`ReasoningBlock`]s produced here.
const PROVIDER_TAG: &str = "anthropic";

/// Anthropic/Claude model provider.
pub struct AnthropicModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model_id: String,
    capabilities: ModelCapabilities,
    pricing: Option<ModelPricing>,
    /// Set once the endpoint rejects on-demand compaction so we stop retrying it.
    native_compaction_unsupported: std::sync::atomic::AtomicBool,
}

/// Beta header for Anthropic's on-demand (server-side) compaction.
const COMPACTION_BETA: &str = "compact-2026-09-04";

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
            native_compaction_unsupported: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Build the on-demand compaction request body: the conversation as-is plus
    /// `compaction: {type: summarize}`. Fields the API rejects on this call are removed.
    fn compaction_body(&self, request: &NativeCompactionRequest) -> Value {
        let params = GenerationParams {
            max_tokens: Some(8192),
            ..Default::default()
        };
        let tools = (!request.tools.is_empty()).then_some(request.tools.as_slice());
        let mut messages = Vec::with_capacity(request.messages.len() + 1);
        if !request.system_prompt.is_empty() {
            messages.push(ModelMessage {
                role: ModelRole::System,
                content: request.system_prompt.clone(),
                ..Default::default()
            });
        }
        messages.extend(request.messages.iter().cloned());
        let mut body = Self::request_body(&self.build_request(&messages, &params, tools, false));
        if let Some(obj) = body.as_object_mut() {
            for key in [
                "stream",
                "tool_choice",
                "stop_sequences",
                "temperature",
                "top_p",
            ] {
                obj.remove(key);
            }
            obj.insert(
                "compaction".into(),
                serde_json::json!({"type": "summarize"}),
            );
        }
        body
    }

    /// Extract the readable summary from a compaction response.
    fn parse_compaction_summary(body: &Value) -> Option<String> {
        body.get("content")?
            .as_array()?
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("compaction"))?
            .get("content")?
            .as_str()
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    }

    /// Returns the per-model pricing, if available.
    pub fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    /// Auth headers: `x-api-key` for the direct API, plus Bearer for gateways/proxies.
    fn auth_headers(&self) -> header::HeaderMap {
        let mut headers = header::HeaderMap::new();
        if let Ok(v) = header::HeaderValue::from_str(&self.api_key) {
            headers.insert("x-api-key", v);
        }
        if self.base_url != DEFAULT_BASE_URL
            && let Ok(v) = header::HeaderValue::from_str(&format!("Bearer {}", self.api_key))
        {
            headers.insert(header::AUTHORIZATION, v);
        }
        headers
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
            // 529 is Anthropic's "overloaded" status.
            StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::GATEWAY_TIMEOUT => ModelError::ServiceUnavailable { retry_after_sec: 0 },
            s if s.as_u16() == 529 => ModelError::ServiceUnavailable { retry_after_sec: 0 },
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
                    system_prompt = Some(match system_prompt.take() {
                        Some(prev) => format!("{prev}\n\n{}", msg.content),
                        None => msg.content.clone(),
                    });
                }
                ModelRole::User => {
                    let mut content: Vec<AnthropicContentBlock> = msg
                        .extras
                        .images
                        .iter()
                        .map(|img| AnthropicContentBlock::Image {
                            source: AnthropicImageSource {
                                kind: "base64".to_string(),
                                media_type: img.media_type.clone(),
                                data: img.data.clone(),
                            },
                        })
                        .collect();
                    content.push(AnthropicContentBlock::Text {
                        text: msg.content.clone(),
                    });
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content,
                    });
                }
                ModelRole::Assistant => {
                    let mut content = Vec::new();
                    // Signed thinking blocks must precede other content and round-trip verbatim.
                    for block in msg
                        .extras
                        .reasoning
                        .iter()
                        .filter(|b| b.provider == PROVIDER_TAG)
                    {
                        let Some(signature) = block.signature.clone() else {
                            continue;
                        };
                        content.push(if block.redacted {
                            AnthropicContentBlock::RedactedThinking { data: signature }
                        } else {
                            AnthropicContentBlock::Thinking {
                                thinking: block.text.clone(),
                                signature,
                            }
                        });
                    }
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
                                is_error: msg.extras.is_error.then_some(true),
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
        let mut thinking: Option<String> = None;
        let mut reasoning = Vec::new();

        for block in resp.content {
            match block {
                AnthropicContentBlock::Text { text } => content.push_str(&text),
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
                    signature,
                } => {
                    thinking.get_or_insert_with(String::new).push_str(&t);
                    reasoning.push(ReasoningBlock {
                        provider: PROVIDER_TAG.to_string(),
                        text: t,
                        signature: Some(signature),
                        ..Default::default()
                    });
                }
                AnthropicContentBlock::RedactedThinking { data } => {
                    reasoning.push(ReasoningBlock {
                        provider: PROVIDER_TAG.to_string(),
                        signature: Some(data),
                        redacted: true,
                        ..Default::default()
                    });
                }
                _ => {}
            }
        }

        let usage = Some(resp.usage.to_usage());

        ModelResponse {
            content,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            thinking,
            reasoning,
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
        let has_tools = anthropic_tools.as_ref().is_some_and(|t| !t.is_empty());

        // Extended thinking: needs a budget below max_tokens, forbids temperature/top_p
        // and any forced tool choice.
        let level = params.reasoning.filter(|_| self.capabilities.thinking);
        let mut max_tokens = params.max_tokens.unwrap_or(self.capabilities.max_output);
        let mut thinking = None;
        if let Some(budget) = level.and_then(|l| l.budget_tokens()) {
            max_tokens = max_tokens.max(budget + 1024);
            thinking = Some(serde_json::json!({"type": "enabled", "budget_tokens": budget}));
        }
        let thinking_on = thinking.is_some();

        let tool_choice = if has_tools {
            Some(match &params.tool_choice {
                Some(ToolChoice::None) => AnthropicToolChoice::None,
                Some(ToolChoice::Required) if !thinking_on => AnthropicToolChoice::Any,
                Some(ToolChoice::Tool(name)) if !thinking_on => {
                    AnthropicToolChoice::Tool { name: name.clone() }
                }
                _ => AnthropicToolChoice::Auto,
            })
        } else {
            None
        };

        AnthropicRequest {
            model: self.model_id.clone(),
            messages: anthropic_messages,
            system,
            temperature: params.temperature.filter(|_| !thinking_on),
            max_tokens: Some(max_tokens),
            top_p: params.top_p.filter(|_| !thinking_on),
            stop_sequences: params.stop_sequences.clone(),
            stream: Some(stream),
            tools: anthropic_tools,
            tool_choice,
            thinking,
        }
    }

    /// Serialize the request with prompt-cache breakpoints on the stable prefix
    /// (system prompt, tool list) and on the newest message.
    fn request_body(request: &AnthropicRequest) -> Value {
        let mut body = serde_json::to_value(request).unwrap_or(Value::Null);
        let ephemeral = || serde_json::json!({"type": "ephemeral"});

        if let Some(Value::String(text)) = body.get("system").cloned() {
            body["system"] = serde_json::json!([
                {"type": "text", "text": text, "cache_control": ephemeral()}
            ]);
        }
        if let Some(tool) = body
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .and_then(|t| t.last_mut())
        {
            tool["cache_control"] = ephemeral();
        }
        if let Some(block) = body
            .get_mut("messages")
            .and_then(Value::as_array_mut)
            .and_then(|m| m.last_mut())
            .and_then(|m| m.get_mut("content"))
            .and_then(Value::as_array_mut)
            .and_then(|c| c.last_mut())
        {
            // Thinking blocks cannot carry cache_control.
            let ty = block.get("type").and_then(Value::as_str).unwrap_or("");
            if !ty.contains("thinking") {
                block["cache_control"] = ephemeral();
            }
        }
        body
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

    async fn compact_native(
        &self,
        request: NativeCompactionRequest,
    ) -> Result<Option<NativeCompactionResponse>, ModelError> {
        use std::sync::atomic::Ordering;
        if self.native_compaction_unsupported.load(Ordering::Relaxed) {
            return Ok(None);
        }

        let body = self.compaction_body(&request);
        let response = self
            .client
            .post(self.messages_url())
            .headers(self.auth_headers())
            .header(header::CONTENT_TYPE, "application/json")
            .header("anthropic-version", self.anthropic_version())
            .header("anthropic-beta", COMPACTION_BETA)
            .json(&body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = response.status();
        let text = response.text().await.map_err(Self::map_reqwest_error)?;
        if !status.is_success() {
            // 4xx other than rate limiting means this model/endpoint lacks the beta.
            if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS {
                self.native_compaction_unsupported
                    .store(true, Ordering::Relaxed);
                return Ok(None);
            }
            return Err(Self::map_http_error(status, &text));
        }

        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Ok(None);
        };
        let Some(summary) = Self::parse_compaction_summary(&value) else {
            return Ok(None);
        };
        let tokens_used = value
            .pointer("/usage/iterations")
            .and_then(Value::as_array)
            .map(|its| {
                its.iter()
                    .map(|i| {
                        i.get("input_tokens").and_then(Value::as_u64).unwrap_or(0)
                            + i.get("output_tokens").and_then(Value::as_u64).unwrap_or(0)
                    })
                    .sum::<u64>() as usize
            })
            .unwrap_or(0);

        Ok(Some(NativeCompactionResponse {
            checkpoint: CompactionCheckpoint::ClientSummary {
                summary_message: ModelMessage {
                    role: ModelRole::User,
                    content: summary.clone(),
                    ..Default::default()
                },
            },
            summary_text: summary,
            tokens_used,
        }))
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
            .headers(self.auth_headers())
            .header(header::CONTENT_TYPE, "application/json")
            .header("anthropic-version", self.anthropic_version())
            .json(&Self::request_body(&request))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = response.status();
        let retry_after = retry_after_secs(response.headers());
        let body = response.text().await.map_err(Self::map_reqwest_error)?;

        if !status.is_success() {
            return Err(Self::map_http_error(status, &body).with_retry_after(retry_after));
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
            .headers(self.auth_headers())
            .header(header::CONTENT_TYPE, "application/json")
            .header("anthropic-version", self.anthropic_version())
            .json(&Self::request_body(&request))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = response.status();
        let retry_after = retry_after_secs(response.headers());

        // For streaming, check the status code first.
        if !status.is_success() {
            let body = response.text().await.map_err(Self::map_reqwest_error)?;
            return Err(Self::map_http_error(status, &body).with_retry_after(retry_after));
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
    completed_reasoning: Vec<ReasoningBlock>,
    finished: bool,
    message_started: bool,
    input_tokens: usize,
    output_tokens: usize,
    cache_read_tokens: usize,
    cache_write_tokens: usize,
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
            pending_thinking: StreamingThinking::default(),
            completed_reasoning: Vec::new(),
            finished: false,
            message_started: false,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            truncated: false,
        }
    }

    /// Test helper: a stream over fixed SSE bytes.
    #[cfg(test)]
    fn from_bytes(data: &'static str) -> Self {
        Self::new(futures::stream::iter(vec![Ok(Bytes::from_static(
            data.as_bytes(),
        ))]))
    }

    fn tool_args(input: &str) -> Value {
        if input.trim().is_empty() {
            return serde_json::json!({});
        }
        serde_json::from_str(input).unwrap_or(Value::Null)
    }

    /// Close the open thinking block (if any) into a replayable reasoning block.
    fn finish_thinking(&mut self) -> Option<ReasoningBlock> {
        let state = std::mem::take(&mut self.pending_thinking);
        state.index?;
        Some(ReasoningBlock {
            provider: PROVIDER_TAG.to_string(),
            text: state.thinking,
            signature: state.signature,
            ..Default::default()
        })
    }

    /// Parse an SSE `data:` line into a [`ModelStreamChunk`].
    fn process_data_line(&mut self, data: &str) -> Option<Result<ModelStreamChunk, ModelError>> {
        let trimmed = data.trim();

        if trimmed == "[DONE]" {
            self.finished = true;
            return Some(Ok(self.final_chunk()));
        }

        // Parse the JSON chunk
        let chunk: AnthropicStreamChunk = match serde_json::from_str(trimmed) {
            Ok(c) => c,
            Err(_) => return None, // skip unparseable lines
        };
        let block_index = |c: &AnthropicStreamChunk| {
            c.index
                .or_else(|| c.delta.as_ref().and_then(|d| d.index))
                .or_else(|| c.content_block.as_ref().and_then(|d| d.index))
        };

        match chunk.r#type.as_str() {
            "message_start" => {
                self.message_started = true;
                if let Some(msg) = chunk.message {
                    let usage = msg.usage.to_usage();
                    self.input_tokens = usage.prompt_tokens;
                    self.output_tokens = usage.completion_tokens;
                    self.cache_read_tokens = usage.cache_read_tokens;
                    self.cache_write_tokens = usage.cache_write_tokens;
                }
                None // Don't emit chunk for message_start
            }
            "content_block_start" => {
                let index = block_index(&chunk);
                // Real API nests the block under `content_block`; older synthetic
                // streams used `delta`.
                let block = chunk.content_block.or(chunk.delta)?;
                match block.delta_type.as_str() {
                    "tool_use" => {
                        let index = index.unwrap_or(self.pending_tool_calls.len());
                        let mut input = String::new();
                        // Some servers send complete input up front.
                        if let Some(v) = &block.input
                            && v.as_object().is_none_or(|o| !o.is_empty())
                        {
                            input = v.to_string();
                        }
                        self.pending_tool_calls.push(StreamingToolCall {
                            index,
                            id: block.id,
                            name: block.name,
                            input,
                        });
                    }
                    "thinking" => {
                        self.pending_thinking = StreamingThinking {
                            index: Some(index.unwrap_or(0)),
                            ..Default::default()
                        };
                    }
                    "redacted_thinking" => {
                        self.completed_reasoning.push(ReasoningBlock {
                            provider: PROVIDER_TAG.to_string(),
                            signature: block.data,
                            redacted: true,
                            ..Default::default()
                        });
                    }
                    _ => {}
                }
                None
            }
            "content_block_delta" => {
                let index = block_index(&chunk);
                let delta = chunk.delta?;
                match delta.delta_type.as_str() {
                    "text_delta" => {
                        if let Some(text) = delta.text {
                            return Some(Ok(ModelStreamChunk {
                                delta: text,
                                ..Default::default()
                            }));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(thinking) = delta.thinking {
                            self.pending_thinking.thinking.push_str(&thinking);
                            return Some(Ok(ModelStreamChunk {
                                thinking: Some(thinking),
                                ..Default::default()
                            }));
                        }
                    }
                    "signature_delta" => {
                        if let Some(signature) = delta.signature {
                            self.pending_thinking.signature = Some(signature);
                        }
                    }
                    "input_json_delta" => {
                        let fragment = delta.partial_json.or_else(|| {
                            delta.input.map(|v| match v {
                                Value::String(s) => s,
                                other => other.to_string(),
                            })
                        });
                        if let Some(fragment) = fragment {
                            let tc = match index {
                                Some(idx) => {
                                    self.pending_tool_calls.iter_mut().find(|t| t.index == idx)
                                }
                                None => self.pending_tool_calls.last_mut(),
                            };
                            if let Some(tc) = tc {
                                tc.input.push_str(&fragment);
                            }
                        }
                    }
                    _ => {}
                }
                None
            }
            "content_block_stop" => {
                let idx = block_index(&chunk);
                if self.pending_thinking.index.is_some()
                    && (idx.is_none() || idx == self.pending_thinking.index)
                {
                    let block = self.finish_thinking()?;
                    return Some(Ok(ModelStreamChunk {
                        reasoning: vec![block],
                        ..Default::default()
                    }));
                }
                let pos = match idx {
                    Some(idx) => self.pending_tool_calls.iter().position(|t| t.index == idx),
                    None => (!self.pending_tool_calls.is_empty()).then_some(0),
                }?;
                let tc = self.pending_tool_calls.remove(pos);
                let name = tc.name?;
                Some(Ok(ModelStreamChunk {
                    tool_calls: Some(vec![ToolCall {
                        id: tc.id.unwrap_or_default(),
                        name,
                        args: Self::tool_args(&tc.input),
                        status: crate::events::ToolCallStatus::Ok,
                    }]),
                    ..Default::default()
                }))
            }
            "message_delta" => {
                if let Some(usage) = chunk.usage {
                    if usage.input_tokens > 0 {
                        self.input_tokens = self.input_tokens.max(usage.total_input());
                    }
                    self.output_tokens = usage.output_tokens;
                }
                if chunk.delta.and_then(|d| d.stop_reason).as_deref() == Some("max_tokens") {
                    self.truncated = true;
                }
                None
            }
            "message_stop" => {
                self.finished = true;
                Some(Ok(self.final_chunk()))
            }
            "error" => {
                self.finished = true;
                // e.g. `overloaded_error` delivered mid-stream.
                Some(Err(ModelError::StreamInterrupted(
                    chunk
                        .error
                        .map(|e| format!("{}: {}", e.r#type, e.message))
                        .unwrap_or_else(|| "stream error event".to_string()),
                )))
            }
            _ => None,
        }
    }

    /// Build the terminal chunk: flush pending tool calls/reasoning and attach usage.
    fn final_chunk(&mut self) -> ModelStreamChunk {
        let tool_calls = if self.pending_tool_calls.is_empty() {
            None
        } else {
            Some(self.flush_tool_calls())
        };
        let mut reasoning = std::mem::take(&mut self.completed_reasoning);
        reasoning.extend(self.finish_thinking());
        let usage = (self.input_tokens > 0 || self.output_tokens > 0).then(|| Usage {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: self.input_tokens + self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            reasoning_tokens: 0,
        });
        ModelStreamChunk {
            delta: String::new(),
            tool_calls,
            thinking: None,
            reasoning,
            finished: true,
            usage,
            truncated: self.truncated,
        }
    }

    /// Flush accumulated tool calls into [`ToolCall`] items and clear the buffer.
    fn flush_tool_calls(&mut self) -> Vec<ToolCall> {
        let calls: Vec<ToolCall> = self
            .pending_tool_calls
            .drain(..)
            .filter_map(|tc| {
                let name = tc.name?;
                let args = Self::tool_args(&tc.input);
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

/// Default base URL for Anthropic.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::model_trait::ModelTier;

    #[test]
    fn test_system_messages_are_joined() {
        let msg = |role, content: &str| ModelMessage {
            role,
            content: content.to_string(),
            ..Default::default()
        };
        let msgs = vec![
            msg(ModelRole::System, "a"),
            msg(ModelRole::System, "b"),
            msg(ModelRole::User, "hi"),
        ];
        let (system, out) = AnthropicModel::to_anthropic_messages(&msgs);
        assert_eq!(system.as_deref(), Some("a\n\nb"));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn test_request_body_marks_cache_breakpoints() {
        let model = AnthropicModel::new(
            reqwest::Client::new(),
            "https://api.anthropic.com",
            "k",
            "claude",
            test_capabilities(),
            None,
        );
        let msg = |role, content: &str| ModelMessage {
            role,
            content: content.to_string(),
            ..Default::default()
        };
        let msgs = vec![msg(ModelRole::System, "sys"), msg(ModelRole::User, "hi")];
        let tools = vec![ToolSpec::new(
            "t",
            "d",
            serde_json::json!({"type": "object"}),
        )];
        let req = model.build_request(&msgs, &GenerationParams::default(), Some(&tools), false);
        let body = AnthropicModel::request_body(&req);
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn test_compaction_body_and_summary_parsing() {
        let model = AnthropicModel::new(
            reqwest::Client::new(),
            "https://api.anthropic.com",
            "k",
            "claude",
            test_capabilities(),
            None,
        );
        let request = NativeCompactionRequest {
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: "hi".into(),
                ..Default::default()
            }],
            system_prompt: String::new(),
            tools: Vec::new(),
            keep_recent_tokens: 0,
            trigger: crate::events::CompactionTrigger::Manual,
        };
        let body = model.compaction_body(&request);
        assert_eq!(body["compaction"]["type"], "summarize");
        assert!(body.get("stream").is_none() && body.get("tool_choice").is_none());

        let resp = serde_json::json!({
            "content": [{"type": "compaction", "content": "the summary", "signature": "sig"}],
            "stop_reason": "compaction"
        });
        assert_eq!(
            AnthropicModel::parse_compaction_summary(&resp).as_deref(),
            Some("the summary")
        );
        let empty = serde_json::json!({"content": [{"type": "text", "text": "x"}]});
        assert!(AnthropicModel::parse_compaction_summary(&empty).is_none());
    }

    #[test]
    fn test_usage_total_includes_cache_tokens() {
        let u = AnthropicUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: Some(20),
            cache_read_input_tokens: Some(30),
        };
        assert_eq!(u.total_input(), 60);
    }
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
                total_tokens: 15,
                ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        assert_eq!(model.estimate_cost(&usage), 0.0);
    }

    #[test]
    fn test_to_anthropic_messages_user() {
        let messages = vec![ModelMessage {
            role: ModelRole::User,
            content: "Hello".to_string(),
            ..Default::default()
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
            ..Default::default()
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
            tool_call_id: Some("toolu_123".to_string()),
            ..Default::default()
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

    async fn collect(data: &'static str) -> Vec<ModelStreamChunk> {
        use futures::StreamExt;
        SseStream::from_bytes(data)
            .map(|c| c.expect("chunk"))
            .collect()
            .await
    }

    const TOOL_USE_STREAM: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":25,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":10,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me look.\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"read\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": \"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.txt\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":42}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    #[tokio::test]
    async fn test_stream_real_tool_use_format() {
        let chunks = collect(TOOL_USE_STREAM).await;
        let text: String = chunks.iter().map(|c| c.delta.as_str()).collect();
        assert_eq!(text, "Let me look.");
        let calls: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.tool_calls.clone())
            .flatten()
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_01");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].args["path"], "a.txt");
        let last = chunks.last().unwrap();
        assert!(last.finished);
        let usage = last.usage.clone().unwrap();
        assert_eq!(usage.prompt_tokens, 135);
        assert_eq!(usage.cache_read_tokens, 100);
        assert_eq!(usage.cache_write_tokens, 10);
        assert_eq!(usage.completion_tokens, 42);
    }

    #[tokio::test]
    async fn test_stream_thinking_blocks_round_trip() {
        let data = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Hmm \"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"ok.\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"OPAQUE\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let chunks = collect(data).await;
        let thinking: String = chunks.iter().filter_map(|c| c.thinking.clone()).collect();
        assert_eq!(thinking, "Hmm ok.");
        let blocks: Vec<_> = chunks.iter().flat_map(|c| c.reasoning.clone()).collect();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].signature.as_deref(), Some("SIG"));
        assert_eq!(blocks[0].text, "Hmm ok.");
        assert!(blocks[1].redacted);

        // Replayed to Anthropic verbatim, ahead of the text.
        let msg = ModelMessage {
            role: ModelRole::Assistant,
            content: "done".into(),
            extras: crate::model::MessageExtras {
                reasoning: blocks,
                ..Default::default()
            },
            ..Default::default()
        };
        let (_, out) = AnthropicModel::to_anthropic_messages(&[msg]);
        assert!(matches!(
            &out[0].content[0],
            AnthropicContentBlock::Thinking { signature, .. } if signature == "SIG"
        ));
        assert!(matches!(
            &out[0].content[1],
            AnthropicContentBlock::RedactedThinking { data } if data == "OPAQUE"
        ));
        assert!(matches!(
            &out[0].content[2],
            AnthropicContentBlock::Text { .. }
        ));
    }

    #[tokio::test]
    async fn test_stream_legacy_synthetic_shape_still_parses() {
        let data = concat!(
            "data: {\"type\":\"content_block_start\",\"delta\":{\"type\":\"tool_use\",\"name\":\"read\",\"index\":0}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"input\":\"{\\\"p\\\":1}\",\"index\":0}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"delta\":{\"index\":0}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let chunks = collect(data).await;
        let calls: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.tool_calls.clone())
            .flatten()
            .collect();
        assert_eq!(calls[0].args["p"], 1);
    }

    #[test]
    fn test_other_provider_reasoning_not_replayed() {
        let msg = ModelMessage {
            role: ModelRole::Assistant,
            content: "x".into(),
            extras: crate::model::MessageExtras {
                reasoning: vec![ReasoningBlock {
                    provider: "openai".into(),
                    signature: Some("enc".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let (_, out) = AnthropicModel::to_anthropic_messages(&[msg]);
        assert_eq!(out[0].content.len(), 1);
    }

    #[test]
    fn test_thinking_request_constraints_and_auth() {
        let model = AnthropicModel::new(
            reqwest::Client::new(),
            DEFAULT_BASE_URL,
            "k",
            "claude",
            test_capabilities(),
            None,
        );
        let params = GenerationParams {
            temperature: Some(0.2),
            max_tokens: Some(4096),
            reasoning: Some(crate::model::ReasoningLevel::High),
            tool_choice: Some(ToolChoice::Required),
            ..Default::default()
        };
        let msgs = vec![ModelMessage {
            role: ModelRole::User,
            content: "hi".into(),
            ..Default::default()
        }];
        let tools = vec![ToolSpec::new(
            "t",
            "d",
            serde_json::json!({"type": "object"}),
        )];
        let body =
            AnthropicModel::request_body(&model.build_request(&msgs, &params, Some(&tools), true));
        assert_eq!(body["thinking"]["budget_tokens"], 24576);
        assert!(body["max_tokens"].as_u64().unwrap() > 24576);
        assert!(body.get("temperature").is_none());
        assert_eq!(body["tool_choice"]["type"], "auto");

        let headers = model.auth_headers();
        assert_eq!(headers["x-api-key"], "k");
        assert!(headers.get(header::AUTHORIZATION).is_none());
    }
}
