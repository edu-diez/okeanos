//! Model abstraction layer.
//!
//! Provides the `Model` trait (text-in/text-out primitive), capability
//! negotiation, automatic tier classification, and a mock provider for testing.

pub mod anthropic;
pub mod cache;
pub mod catalog;
pub mod classifier;
pub mod discovery;
pub mod google;
pub mod http;
pub mod mock;
pub mod model_trait;
pub mod openai;
pub mod openai_native;
pub mod openai_responses;
pub mod profile;
pub mod registry;
pub mod text_tool;

pub use anthropic::{AnthropicModel, DEFAULT_BASE_URL as ANTHROPIC_DEFAULT_BASE_URL};
pub use cache::ModelCache;
pub use classifier::{
    ModelClassification, classify_model, default_capabilities_for_tier, discover_and_classify,
    merge_capabilities,
};
pub use discovery::{
    DiscoveredModel, DiscoveryError, fetch_openrouter_models, minimal_fallback_models,
};
pub use google::{DEFAULT_BASE_URL as GOOGLE_DEFAULT_BASE_URL, GoogleModel};
pub use mock::MockModel;
pub use model_trait::{
    GenerationParams, ImagePart, MessageExtras, Model, ModelCapabilities, ModelError, ModelMessage,
    ModelResponse, ModelRole, ModelStreamChunk, ModelTier, ReasoningBlock, ReasoningLevel,
    ToolChoice, Usage, default_generation_params, system_prompt_template,
};
pub use openai::{DEFAULT_BASE_URL, OpenRouterModel};
pub use openai_native::{DEFAULT_BASE_URL as OPENAI_DEFAULT_BASE_URL, OpenAIModel};
pub use openai_responses::OpenAIResponsesModel;
