use crate::context::compaction::{NativeCompactionRequest, NativeCompactionResponse};
use crate::events::ToolCall;
use crate::tool::builtin::EditFormat;
use crate::tool::tool_trait::ToolSpec;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

/// Capabilities declared per model.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ModelCapabilities {
    /// Model supports native JSON function calling
    pub tool_calling: bool,
    /// Model supports structured output (e.g., JSON schema constrained generation)
    pub structured_output: bool,
    /// Model supports a separate thinking/reasoning channel
    pub thinking: bool,
    /// Maximum context window in tokens
    pub max_context: usize,
    /// Maximum output tokens per completion
    pub max_output: usize,
    /// Model supports temperature parameter
    pub temperature_supported: bool,
    /// Model tier for tiered prompts and tool selection
    pub tier: ModelTier,
    /// Preferred edit format for this model
    pub edit_format: EditFormat,
}

/// Model capability tier, determines prompt template, tool subset, and generation params.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Fast/small models: <10B params, 4k–32k context, limited/no native FC.
    /// Examples: Llama 8B, Gemma 2B/9B, Phi-4, Qwen 7B/14B, Haiku.
    #[default]
    Small,
    /// Balanced/mid-tier models: 10B–70B params, 32k–128k context, native FC.
    /// Examples: Llama 3 70B, Mistral Large, Command R+, Sonnet.
    Mid,
    /// Frontier models: >70B params, 128k+ context, full capabilities.
    /// Examples: GPT-4o, GPT-5, Claude Opus, Gemini Ultra.
    Frontier,
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelTier::Small => write!(f, "small"),
            ModelTier::Mid => write!(f, "mid"),
            ModelTier::Frontier => write!(f, "frontier"),
        }
    }
}

impl std::str::FromStr for ModelTier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "small" | "fast" => Ok(ModelTier::Small),
            "mid" | "balanced" => Ok(ModelTier::Mid),
            "frontier" => Ok(ModelTier::Frontier),
            _ => Err(format!("invalid tier: {}", s)),
        }
    }
}

/// Generation parameters for a model request.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct GenerationParams {
    pub temperature: Option<f64>,
    pub max_tokens: Option<usize>,
    pub stop_sequences: Option<Vec<String>>,
    pub top_p: Option<f64>,
    /// Provider-neutral reasoning effort; adapters map it to their native knob.
    #[serde(default)]
    pub reasoning: Option<ReasoningLevel>,
    /// Tool selection constraint (default: provider "auto").
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
}

/// Provider-neutral reasoning effort.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    Off,
    Low,
    Medium,
    High,
}

impl ReasoningLevel {
    /// OpenAI-style `reasoning_effort` value; `None` when reasoning is off.
    pub fn effort(self) -> Option<&'static str> {
        match self {
            ReasoningLevel::Off => None,
            ReasoningLevel::Low => Some("low"),
            ReasoningLevel::Medium => Some("medium"),
            ReasoningLevel::High => Some("high"),
        }
    }

    /// Token budget for budget-based providers (Anthropic, Gemini 2.5).
    pub fn budget_tokens(self) -> Option<usize> {
        match self {
            ReasoningLevel::Off => None,
            ReasoningLevel::Low => Some(2_048),
            ReasoningLevel::Medium => Some(8_192),
            ReasoningLevel::High => Some(24_576),
        }
    }
}

impl std::str::FromStr for ReasoningLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "off" | "none" => Ok(ReasoningLevel::Off),
            "low" => Ok(ReasoningLevel::Low),
            "medium" | "med" => Ok(ReasoningLevel::Medium),
            "high" => Ok(ReasoningLevel::High),
            _ => Err(format!("invalid reasoning level: {s}")),
        }
    }
}

/// Constraint on which tool the model may call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Required,
    None,
    Tool(String),
}

/// A provider reasoning block that must round-trip verbatim (signatures,
/// encrypted content). Only replayed to the provider that produced it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReasoningBlock {
    /// Producing provider kind: "anthropic", "openai", "google", ...
    pub provider: String,
    /// Readable text/summary (may be empty).
    #[serde(default)]
    pub text: String,
    /// Anthropic signature, Gemini thoughtSignature, or OpenAI encrypted content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Anthropic `redacted_thinking`: `signature` then holds the opaque data.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redacted: bool,
    /// Provider item id (OpenAI Responses reasoning item).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Base64-encoded image attached to a message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImagePart {
    pub media_type: String,
    pub data: String,
}

impl ImagePart {
    /// `data:` URL form used by OpenAI-style APIs.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

/// OpenAI reasoning families (o-series, gpt-5): they reject `temperature`/`top_p`
/// and `max_tokens`, and accept `reasoning_effort`.
pub fn is_openai_reasoning_model(model_id: &str) -> bool {
    let id = model_id.rsplit('/').next().unwrap_or(model_id);
    id.starts_with("o1") || id.starts_with("o3") || id.starts_with("o4") || id.starts_with("gpt-5")
}

/// Rich message parts beyond plain text, kept out of the common literal shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MessageExtras {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<ReasoningBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImagePart>,
    /// Tool result reported a failure.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

impl MessageExtras {
    pub fn is_empty(&self) -> bool {
        self.reasoning.is_empty() && self.images.is_empty() && !self.is_error
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A single message in the conversation for the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    /// Optional thinking/reasoning content (replayed for frontier models).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "MessageExtras::is_empty")]
    pub extras: MessageExtras,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

/// Response from a model completion request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelResponse {
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub thinking: Option<String>,
    pub usage: Option<Usage>,
    /// Provider reasoning blocks to round-trip on the next request.
    #[serde(default)]
    pub reasoning: Vec<ReasoningBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Usage {
    /// Total prompt tokens, including cached ones.
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
    /// Prompt tokens served from the provider cache (subset of `prompt_tokens`).
    #[serde(default)]
    pub cache_read_tokens: usize,
    /// Prompt tokens written to the provider cache (subset of `prompt_tokens`).
    #[serde(default)]
    pub cache_write_tokens: usize,
    /// Reasoning tokens (subset of `completion_tokens`).
    #[serde(default)]
    pub reasoning_tokens: usize,
}

/// Streaming chunk from a model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelStreamChunk {
    pub delta: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub thinking: Option<String>,
    /// Completed provider reasoning blocks (normally on the final chunk).
    #[serde(default)]
    pub reasoning: Vec<ReasoningBlock>,
    pub finished: bool,
    /// Provider-reported token usage (normally on the final chunk).
    #[serde(default)]
    pub usage: Option<Usage>,
    /// The provider stopped because it hit the output token limit.
    #[serde(default)]
    pub truncated: bool,
}

/// Errors from model providers.
#[derive(Debug, Clone, Error)]
pub enum ModelError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("rate limited: retry after {retry_after_sec}s")]
    RateLimited { retry_after_sec: u64 },
    #[error("context length exceeded: {0}")]
    ContextLengthExceeded(String),
    #[error("authentication failed")]
    AuthFailed,
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("streaming error: {0}")]
    Streaming(String),
    #[error("service unavailable: retry after {retry_after_sec}s")]
    ServiceUnavailable { retry_after_sec: u64 },
    #[error("capability missing: {0}")]
    MissingCapability(String),
    /// The connection dropped or errored while the response was streaming.
    #[error("stream interrupted: {0}")]
    StreamInterrupted(String),
}

impl ModelError {
    /// Returns `true` if this error is transient and the call should be
    /// retried with backoff (e.g. rate limiting, temporary service issues).
    ///
    /// Permanent errors (auth failures, invalid requests, missing
    /// capabilities, model not found, context length) must not be retried.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ModelError::RateLimited { .. }
                | ModelError::ServiceUnavailable { .. }
                | ModelError::StreamInterrupted(_)
        )
    }

    /// Attach a provider `Retry-After` hint (seconds). `0` means "no hint".
    pub fn with_retry_after(self, secs: u64) -> Self {
        match self {
            ModelError::RateLimited { .. } => ModelError::RateLimited {
                retry_after_sec: secs,
            },
            ModelError::ServiceUnavailable { .. } => ModelError::ServiceUnavailable {
                retry_after_sec: secs,
            },
            other => other,
        }
    }
}

/// Extract a retry delay in whole seconds from provider response headers.
///
/// Honors `retry-after-ms` and `retry-after` (delta-seconds form). Returns 0
/// when no usable hint is present.
pub fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> u64 {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok())
        && ms > 0.0
    {
        return (ms / 1000.0).ceil() as u64;
    }
    get("retry-after")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .map(|s| s.ceil() as u64)
        .unwrap_or(0)
}

/// Whether an HTTP error message indicates the prompt exceeded the context window.
pub fn is_context_overflow_message(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("context length")
        || m.contains("context_length")
        || m.contains("context window")
        || m.contains("prompt is too long")
        || m.contains("maximum context")
        || m.contains("too many tokens")
        || m.contains("exceeds the maximum number of tokens")
        || m.contains("request too large")
}

/// The Model trait - text-in/text-out as the primitive.
/// Native JSON tool calling is an optional capability (supports_tool_calling), not a requirement.
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    /// Returns the model's declared capabilities.
    fn capabilities(&self) -> &ModelCapabilities;

    /// Returns the model identifier (e.g., "gpt-4o", "llama-3.1-8b").
    fn model_id(&self) -> &str;

    /// Non-streaming completion.
    async fn complete(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<ModelResponse, ModelError>;

    /// Streaming completion.
    async fn stream(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ModelStreamChunk, ModelError>> + Send + 'static>>,
        ModelError,
    >;

    /// Optional: provider-native context compaction.
    ///
    /// Returns `None` if not supported (client-side fallback will be used).
    /// Implementations should delegate to provider compaction endpoints
    /// (e.g., OpenAI Responses `compaction_trigger`, Anthropic compaction API).
    async fn compact_native(
        &self,
        _request: NativeCompactionRequest,
    ) -> Result<Option<NativeCompactionResponse>, ModelError> {
        Ok(None)
    }

    /// Validate that the model has the required capabilities.
    fn require_capability(&self, capability: &str) -> Result<(), ModelError> {
        match capability {
            "tool_calling" if !self.capabilities().tool_calling => {
                Err(ModelError::MissingCapability("tool_calling".into()))
            }
            "structured_output" if !self.capabilities().structured_output => {
                Err(ModelError::MissingCapability("structured_output".into()))
            }
            "thinking" if !self.capabilities().thinking => {
                Err(ModelError::MissingCapability("thinking".into()))
            }
            _ => Ok(()),
        }
    }
}

/// Implement Model for Arc<dyn Model> to allow trait object usage in ReactLoop.
/// This delegates all calls to the inner trait object.
#[async_trait::async_trait]
impl Model for Arc<dyn Model + Send + Sync> {
    fn capabilities(&self) -> &ModelCapabilities {
        (**self).capabilities()
    }

    fn model_id(&self) -> &str {
        (**self).model_id()
    }

    async fn complete(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<ModelResponse, ModelError> {
        (**self).complete(messages, params, tools).await
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
        (**self).stream(messages, params, tools).await
    }

    async fn compact_native(
        &self,
        request: NativeCompactionRequest,
    ) -> Result<Option<NativeCompactionResponse>, ModelError> {
        (**self).compact_native(request).await
    }

    fn require_capability(&self, capability: &str) -> Result<(), ModelError> {
        (**self).require_capability(capability)
    }
}

/// Default generation parameters per tier.
pub fn default_generation_params(tier: ModelTier) -> GenerationParams {
    match tier {
        ModelTier::Small => GenerationParams {
            temperature: Some(0.2),
            max_tokens: Some(2048),
            stop_sequences: None,
            top_p: Some(0.9),
            ..Default::default()
        },
        ModelTier::Mid => GenerationParams {
            temperature: Some(0.5),
            max_tokens: Some(4096),
            stop_sequences: None,
            top_p: Some(0.95),
            ..Default::default()
        },
        ModelTier::Frontier => GenerationParams {
            temperature: Some(0.7),
            max_tokens: Some(8192),
            stop_sequences: None,
            top_p: Some(0.95),
            ..Default::default()
        },
    }
}

/// System prompt templates per tier.
pub fn system_prompt_template(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Small => include_str!("../../assets/prompts/small_system_prompt.txt"),
        ModelTier::Mid => include_str!("../../assets/prompts/mid_system_prompt.txt"),
        ModelTier::Frontier => include_str!("../../assets/prompts/frontier_system_prompt.txt"),
    }
}
