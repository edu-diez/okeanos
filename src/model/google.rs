//! Google/Gemini model provider.
//!
//! Implements the [`Model`] trait for the Google Generative AI API
//! with native function calling, streaming via SSE, error mapping to
//! [`ModelError`] variants, and cost estimation.

use crate::context::tokens::ModelPricing;
use crate::events::ToolCall;
use crate::model::model_trait::{
    GenerationParams, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse, ModelRole,
    ModelStreamChunk, Usage, is_context_overflow_message, retry_after_secs,
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

/// Google Generative AI request.
#[derive(Debug, Clone, Serialize)]
struct GoogleRequest {
    contents: Vec<GoogleContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GoogleContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GoogleGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GoogleTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<GoogleToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    safety_settings: Option<Vec<GoogleSafetySetting>>,
}

/// Google content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleContent {
    role: String,
    parts: Vec<GooglePart>,
}

/// Google content part.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum GooglePart {
    Text {
        text: String,
    },
    FunctionCall {
        function_call: GoogleFunctionCall,
    },
    FunctionResponse {
        function_response: GoogleFunctionResponse,
    },
    // File data and other types not used for now
}

/// Google function call.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleFunctionCall {
    name: String,
    args: Value,
}

/// Google function response.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleFunctionResponse {
    name: String,
    response: Value,
}

/// Google generation config.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_schema: Option<Value>,
}

/// Google tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleTool {
    function_declarations: Vec<GoogleFunctionDeclaration>,
}

/// Google function declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleFunctionDeclaration {
    name: String,
    description: String,
    parameters: Value,
}

/// Google tool config.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleToolConfig {
    function_calling_config: GoogleFunctionCallingConfig,
}

/// Google function calling config.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleFunctionCallingConfig {
    mode: String, // "AUTO", "ANY", "NONE"
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_function_names: Option<Vec<String>>,
}

/// Google safety setting.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoogleSafetySetting {
    category: String,
    threshold: String,
}

/// Google Generative AI response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GoogleResponse {
    candidates: Vec<GoogleCandidate>,
    prompt_feedback: Option<GooglePromptFeedback>,
    usage_metadata: Option<GoogleUsageMetadata>,
}

/// Google candidate.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GoogleCandidate {
    content: GoogleContent,
    finish_reason: Option<String>,
    index: usize,
    safety_ratings: Option<Vec<GoogleSafetyRating>>,
    token_count: Option<usize>,
}

/// Google prompt feedback.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GooglePromptFeedback {
    safety_ratings: Option<Vec<GoogleSafetyRating>>,
    block_reason: Option<String>,
}

/// Google safety rating.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GoogleSafetyRating {
    category: String,
    probability: String,
}

/// Google usage metadata.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GoogleUsageMetadata {
    #[serde(default, alias = "promptTokenCount")]
    prompt_token_count: usize,
    #[serde(default, alias = "candidatesTokenCount")]
    candidates_token_count: usize,
    #[serde(default, alias = "totalTokenCount")]
    total_token_count: usize,
    cached_content_token_count: Option<usize>,
}

/// Google API error response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GoogleErrorResponse {
    error: GoogleErrorBody,
}

/// Google API error body.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GoogleErrorBody {
    code: u32,
    message: String,
    status: String,
    details: Option<Vec<Value>>,
}

/// Streaming chunk from Google API.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GoogleStreamChunk {
    candidates: Option<Vec<GoogleStreamCandidate>>,
    prompt_feedback: Option<GooglePromptFeedback>,
    #[serde(alias = "usageMetadata")]
    usage_metadata: Option<GoogleUsageMetadata>,
}

/// Streaming candidate.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GoogleStreamCandidate {
    content: Option<GoogleContent>,
    finish_reason: Option<String>,
    index: usize,
    safety_ratings: Option<Vec<GoogleSafetyRating>>,
    token_count: Option<usize>,
}

/// Accumulated state for a streaming function call.
#[derive(Debug, Clone, Default)]
struct StreamingFunctionCall {
    index: usize,
    name: Option<String>,
    args: String,
}

/// Google/Gemini model provider.
pub struct GoogleModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model_id: String,
    capabilities: ModelCapabilities,
    pricing: Option<ModelPricing>,
}

impl GoogleModel {
    /// Create a new Google model provider.
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

    fn generate_content_url(&self) -> String {
        format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, self.model_id
        )
    }

    fn stream_generate_content_url(&self) -> String {
        format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, self.model_id
        )
    }

    /// Map an HTTP error response to a [`ModelError`].
    fn map_http_error(status: StatusCode, body: &str) -> ModelError {
        let error_message = serde_json::from_str::<GoogleErrorResponse>(body)
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

    /// Convert a [`ModelMessage`] to a Google content.
    fn to_google_contents(
        messages: &[ModelMessage],
    ) -> (Option<GoogleContent>, Vec<GoogleContent>) {
        let mut system_instruction = None;
        let mut contents = Vec::new();

        for msg in messages {
            match msg.role {
                ModelRole::System => {
                    system_instruction = Some(GoogleContent {
                        role: "user".to_string(), // System instructions use "user" role in parts
                        parts: vec![GooglePart::Text {
                            text: msg.content.clone(),
                        }],
                    });
                }
                ModelRole::User => {
                    contents.push(GoogleContent {
                        role: "user".to_string(),
                        parts: vec![GooglePart::Text {
                            text: msg.content.clone(),
                        }],
                    });
                }
                ModelRole::Assistant => {
                    let mut parts = Vec::new();
                    if !msg.content.is_empty() {
                        parts.push(GooglePart::Text {
                            text: msg.content.clone(),
                        });
                    }
                    if let Some(tool_calls) = &msg.tool_calls {
                        for tc in tool_calls {
                            parts.push(GooglePart::FunctionCall {
                                function_call: GoogleFunctionCall {
                                    name: tc.name.clone(),
                                    args: tc.args.clone(),
                                },
                            });
                        }
                    }
                    if let Some(thinking) = &msg.thinking {
                        // Google doesn't have a native thinking channel in the standard API
                        // Include as text for now
                        parts.push(GooglePart::Text {
                            text: format!("<thinking>{}</thinking>", thinking),
                        });
                    }
                    if !parts.is_empty() {
                        contents.push(GoogleContent {
                            role: "model".to_string(),
                            parts,
                        });
                    }
                }
                ModelRole::Tool => {
                    // Tool result - Google uses function_response
                    if let Some(tool_call_id) = &msg.tool_call_id {
                        // We need to know the function name for the response
                        // For now, extract from tool_call_id or use a placeholder
                        contents.push(GoogleContent {
                            role: "user".to_string(),
                            parts: vec![GooglePart::FunctionResponse {
                                function_response: GoogleFunctionResponse {
                                    name: tool_call_id.clone(), // Using tool_call_id as function name
                                    response: Value::String(msg.content.clone()),
                                },
                            }],
                        });
                    }
                }
            }
        }

        (system_instruction, contents)
    }

    /// Convert [`ToolSpec`] items to Google tool definitions.
    fn to_google_tools(specs: &[ToolSpec]) -> Vec<GoogleTool> {
        if specs.is_empty() {
            return vec![];
        }
        vec![GoogleTool {
            function_declarations: specs
                .iter()
                .map(|spec| GoogleFunctionDeclaration {
                    name: spec.name.clone(),
                    description: spec.description.clone(),
                    parameters: spec.input_schema.clone(),
                })
                .collect(),
        }]
    }

    /// Convert a Google response to a [`ModelResponse`].
    fn from_google_response(resp: GoogleResponse) -> ModelResponse {
        let candidate = resp.candidates.into_iter().next();
        let content = candidate.map(|c| c.content);

        let mut text_content = String::new();
        let mut tool_calls = Vec::new();
        let mut thinking = None;

        if let Some(content) = content {
            for part in content.parts {
                match part {
                    GooglePart::Text { text } => {
                        if text.starts_with("<thinking>") && text.ends_with("</thinking>") {
                            thinking = Some(
                                text["<thinking>".len()..text.len() - "</thinking>".len()]
                                    .to_string(),
                            );
                        } else {
                            text_content.push_str(&text);
                        }
                    }
                    GooglePart::FunctionCall { function_call } => {
                        tool_calls.push(ToolCall {
                            id: format!("call_{}", function_call.name), // Generate ID
                            name: function_call.name,
                            args: function_call.args,
                            status: crate::events::ToolCallStatus::Ok,
                        });
                    }
                    _ => {}
                }
            }
        }

        let usage = resp.usage_metadata.map(|u| Usage {
            prompt_tokens: u.prompt_token_count,
            completion_tokens: u.candidates_token_count,
            total_tokens: u.total_token_count,
        });

        ModelResponse {
            content: text_content,
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

    /// Build the generate content request body.
    fn build_request(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> GoogleRequest {
        let (system_instruction, contents) = Self::to_google_contents(messages);

        let google_tools = tools.map(Self::to_google_tools);

        let tool_config = if google_tools
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false)
        {
            Some(GoogleToolConfig {
                function_calling_config: GoogleFunctionCallingConfig {
                    mode: "AUTO".to_string(),
                    allowed_function_names: None,
                },
            })
        } else {
            None
        };

        let generation_config = GoogleGenerationConfig {
            temperature: params.temperature,
            max_output_tokens: params.max_tokens,
            top_p: params.top_p,
            top_k: Some(40),
            stop_sequences: params.stop_sequences.clone(),
            candidate_count: Some(1),
            response_mime_type: None,
            response_schema: None,
        };

        GoogleRequest {
            contents,
            system_instruction,
            generation_config: Some(generation_config),
            tools: google_tools,
            tool_config,
            safety_settings: None, // Use defaults
        }
    }
}

#[async_trait]
impl Model for GoogleModel {
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
        let request = self.build_request(messages, params, tools);

        let response = self
            .client
            .post(self.generate_content_url())
            .query(&[("key", &self.api_key)])
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

        let resp: GoogleResponse = serde_json::from_str(&body)
            .map_err(|e| ModelError::Provider(format!("JSON parse error: {e}")))?;

        Ok(Self::from_google_response(resp))
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
        let request = self.build_request(messages, params, tools);

        let response = self
            .client
            .post(self.stream_generate_content_url())
            .query(&[("key", &self.api_key)])
            .header(header::CONTENT_TYPE, "application/json")
            .json(&request)
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

/// SSE stream processor that converts Google SSE chunks into [`ModelStreamChunk`] items.
pub(crate) struct SseStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: Vec<u8>,
    pending_function_calls: Vec<StreamingFunctionCall>,
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
            pending_function_calls: Vec::new(),
            finished: false,
            usage: None,
            truncated: false,
        }
    }

    /// Terminal chunk: flushes pending function calls and attaches usage.
    fn final_chunk(&mut self) -> ModelStreamChunk {
        let tool_calls = if self.pending_function_calls.is_empty() {
            None
        } else {
            Some(self.flush_function_calls())
        };
        ModelStreamChunk {
            tool_calls,
            finished: true,
            usage: self.usage.take(),
            truncated: self.truncated,
            ..Default::default()
        }
    }

    /// Parse an SSE `data:` line into a [`ModelStreamChunk`].
    ///
    /// The terminal (`finished`) chunk is only emitted at end of stream so
    /// that trailing usage metadata is not lost.
    fn process_data_line(&mut self, data: &str) -> Option<Result<ModelStreamChunk, ModelError>> {
        let trimmed = data.trim();

        if trimmed == "[DONE]" || trimmed.is_empty() {
            self.finished = true;
            return Some(Ok(self.final_chunk()));
        }

        // Parse the JSON chunk
        let chunk: GoogleStreamChunk = match serde_json::from_str(trimmed) {
            Ok(c) => c,
            Err(_) => return None, // skip unparseable lines
        };

        if let Some(u) = chunk.usage_metadata {
            let total = if u.total_token_count > 0 {
                u.total_token_count
            } else {
                u.prompt_token_count + u.candidates_token_count
            };
            self.usage = Some(Usage {
                prompt_tokens: u.prompt_token_count,
                completion_tokens: u.candidates_token_count,
                total_tokens: total,
            });
        }

        let candidate = chunk.candidates.and_then(|c| c.into_iter().next());

        if let Some(candidate) = candidate {
            let finish_reason = candidate.finish_reason.clone();
            let finished = finish_reason.is_some();
            if finish_reason.as_deref() == Some("MAX_TOKENS") {
                self.truncated = true;
            }

            if let Some(content) = candidate.content {
                let mut text_delta = String::new();
                let mut function_call_delta: Option<GoogleFunctionCall> = None;

                for part in content.parts {
                    match part {
                        GooglePart::Text { text } => {
                            text_delta.push_str(&text);
                        }
                        GooglePart::FunctionCall { function_call } => {
                            function_call_delta = Some(function_call);
                        }
                        _ => {}
                    }
                }

                // Handle function call streaming
                if let Some(fc) = function_call_delta {
                    // For Google, function calls come in complete form in each chunk
                    // Find or create the accumulated function call
                    let idx = candidate.index;
                    let existing = self
                        .pending_function_calls
                        .iter_mut()
                        .find(|t| t.index == idx);

                    match existing {
                        Some(acc) => {
                            if acc.name.is_none() {
                                acc.name = Some(fc.name);
                            }
                            acc.args = fc.args.to_string();
                        }
                        None => {
                            self.pending_function_calls.push(StreamingFunctionCall {
                                index: idx,
                                name: Some(fc.name),
                                args: fc.args.to_string(),
                            });
                        }
                    }
                }

                // If finished, flush function calls
                let mut output_tool_calls = None;
                if finished && !self.pending_function_calls.is_empty() {
                    output_tool_calls = Some(self.flush_function_calls());
                }

                if text_delta.is_empty() && output_tool_calls.is_none() {
                    return None;
                }

                return Some(Ok(ModelStreamChunk {
                    delta: text_delta,
                    tool_calls: output_tool_calls,
                    ..Default::default()
                }));
            }
        }

        None
    }

    /// Flush accumulated function calls into [`ToolCall`] items and clear the buffer.
    fn flush_function_calls(&mut self) -> Vec<ToolCall> {
        let calls: Vec<ToolCall> = self
            .pending_function_calls
            .drain(..)
            .filter_map(|fc| {
                let name = fc.name?;
                let args = serde_json::from_str(&fc.args).unwrap_or(Value::Null);
                Some(ToolCall {
                    id: format!("call_{}", name),
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
                    return std::task::Poll::Ready(Some(Err(ModelError::StreamInterrupted(
                        e.to_string(),
                    ))));
                }
                std::task::Poll::Ready(None) => {
                    // Stream ended
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
                    // Flush pending function calls
                    return std::task::Poll::Ready(Some(Ok(this.final_chunk())));
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Default base URL for Google Generative AI.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::model_trait::ModelTier;

    fn test_capabilities() -> ModelCapabilities {
        ModelCapabilities {
            tool_calling: true,
            structured_output: true,
            thinking: false,
            max_context: 1048576, // 1M tokens for Gemini 1.5
            max_output: 8192,
            temperature_supported: true,
            tier: ModelTier::Frontier,
            edit_format: EditFormat::Diff,
        }
    }

    #[test]
    fn test_google_model_new() {
        let client = reqwest::Client::new();
        let model = GoogleModel::new(
            client,
            "https://generativelanguage.googleapis.com",
            "test-key",
            "gemini-1.5-pro",
            test_capabilities(),
            None,
        );
        assert_eq!(model.model_id(), "gemini-1.5-pro");
        assert!(model.capabilities().tool_calling);
    }

    #[test]
    fn test_map_http_error_auth_failed() {
        let err = GoogleModel::map_http_error(
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"code":401,"message":"API key not valid","status":"UNAUTHENTICATED"}}"#,
        );
        assert!(matches!(err, ModelError::AuthFailed));
    }

    #[test]
    fn test_map_http_error_rate_limited() {
        let err = GoogleModel::map_http_error(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"code":429,"message":"Rate limit exceeded","status":"RESOURCE_EXHAUSTED"}}"#,
        );
        assert!(matches!(
            err,
            ModelError::RateLimited { retry_after_sec: 0 }
        ));
    }

    #[test]
    fn test_map_http_error_model_not_found() {
        let err = GoogleModel::map_http_error(
            StatusCode::NOT_FOUND,
            r#"{"error":{"code":404,"message":"Model not found","status":"NOT_FOUND"}}"#,
        );
        assert!(matches!(err, ModelError::ModelNotFound(_)));
    }

    #[test]
    fn test_to_google_tools() {
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
        let tools = GoogleModel::to_google_tools(&specs);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function_declarations.len(), 1);
        assert_eq!(tools[0].function_declarations[0].name, "todo");
        assert_eq!(
            tools[0].function_declarations[0].description,
            "Manage todo list"
        );
    }

    #[test]
    fn test_from_google_response() {
        let resp = GoogleResponse {
            candidates: vec![GoogleCandidate {
                content: GoogleContent {
                    role: "model".to_string(),
                    parts: vec![GooglePart::Text {
                        text: "Hello!".to_string(),
                    }],
                },
                finish_reason: Some("STOP".to_string()),
                index: 0,
                safety_ratings: None,
                token_count: None,
            }],
            prompt_feedback: None,
            usage_metadata: Some(GoogleUsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: 5,
                total_token_count: 15,
                cached_content_token_count: None,
            }),
        };
        let model_resp = GoogleModel::from_google_response(resp);
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
    fn test_from_google_response_with_function_calls() {
        let resp = GoogleResponse {
            candidates: vec![GoogleCandidate {
                content: GoogleContent {
                    role: "model".to_string(),
                    parts: vec![GooglePart::FunctionCall {
                        function_call: GoogleFunctionCall {
                            name: "todo".to_string(),
                            args: serde_json::json!({"action":"add","title":"Test"}),
                        },
                    }],
                },
                finish_reason: Some("FUNCTION_CALL".to_string()),
                index: 0,
                safety_ratings: None,
                token_count: None,
            }],
            prompt_feedback: None,
            usage_metadata: None,
        };
        let model_resp = GoogleModel::from_google_response(resp);
        assert_eq!(model_resp.content, "");
        let calls = model_resp.tool_calls.expect("tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "todo");
        assert_eq!(calls[0].args["action"], "add");
        assert_eq!(calls[0].args["title"], "Test");
    }

    #[test]
    fn test_estimate_cost_with_pricing() {
        let client = reqwest::Client::new();
        let pricing = Some(ModelPricing {
            input_per_million: 1.25,
            output_per_million: 5.00,
        });
        let model = GoogleModel::new(
            client,
            "https://generativelanguage.googleapis.com",
            "key",
            "gemini-1.5-pro",
            test_capabilities(),
            pricing,
        );
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
        };
        let cost = model.estimate_cost(&usage);
        assert!((cost - 6.25).abs() < 0.001); // 1.25 + 5.00
    }

    #[test]
    fn test_estimate_cost_without_pricing() {
        let client = reqwest::Client::new();
        let model = GoogleModel::new(
            client,
            "https://generativelanguage.googleapis.com",
            "key",
            "gemini-1.5-pro",
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
    fn test_to_google_contents_user() {
        let messages = vec![ModelMessage {
            role: ModelRole::User,
            content: "Hello".to_string(),
            tool_calls: None,
            tool_call_id: None,
            thinking: None,
        }];
        let (system, contents) = GoogleModel::to_google_contents(&messages);
        assert!(system.is_none());
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
        assert!(matches!(
            &contents[0].parts[0],
            GooglePart::Text { text } if text == "Hello"
        ));
    }

    #[test]
    fn test_to_google_contents_system() {
        let messages = vec![ModelMessage {
            role: ModelRole::System,
            content: "You are a helpful assistant".to_string(),
            tool_calls: None,
            tool_call_id: None,
            thinking: None,
        }];
        let (system, contents) = GoogleModel::to_google_contents(&messages);
        assert!(system.is_some());
        assert_eq!(contents.len(), 0);
        assert!(matches!(
            &system.unwrap().parts[0],
            GooglePart::Text { text } if text == "You are a helpful assistant"
        ));
    }

    #[test]
    fn test_to_google_contents_tool_result() {
        let messages = vec![ModelMessage {
            role: ModelRole::Tool,
            content: "File contents here".to_string(),
            tool_calls: None,
            tool_call_id: Some("my_function".to_string()),
            thinking: None,
        }];
        let (system, contents) = GoogleModel::to_google_contents(&messages);
        assert!(system.is_none());
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
        assert!(matches!(
            &contents[0].parts[0],
            GooglePart::FunctionResponse { function_response } if function_response.name == "my_function" && function_response.response == "File contents here"
        ));
    }
}
