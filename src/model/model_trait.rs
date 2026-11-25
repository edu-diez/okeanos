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
}

/// A single message in the conversation for the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    /// Optional thinking/reasoning content (replayed for frontier models).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Response from a model completion request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelResponse {
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub thinking: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
}

/// Streaming chunk from a model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelStreamChunk {
    pub delta: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub thinking: Option<String>,
    pub finished: bool,
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
            ModelError::RateLimited { .. } | ModelError::ServiceUnavailable { .. }
        )
    }
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
        },
        ModelTier::Mid => GenerationParams {
            temperature: Some(0.5),
            max_tokens: Some(4096),
            stop_sequences: None,
            top_p: Some(0.95),
        },
        ModelTier::Frontier => GenerationParams {
            temperature: Some(0.7),
            max_tokens: Some(8192),
            stop_sequences: None,
            top_p: Some(0.95),
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
