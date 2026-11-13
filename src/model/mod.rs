//! Model abstraction layer.
//!
//! Provides the `Model` trait (text-in/text-out primitive), capability
//! negotiation, automatic tier classification, and a mock provider for testing.

pub mod cache;
pub mod classifier;
pub mod discovery;
pub mod mock;
pub mod model_trait;
pub mod openai;
pub mod text_tool;

pub use cache::ModelCache;
pub use classifier::{
    ModelClassification, classify_model, default_capabilities_for_tier, discover_and_classify,
    merge_capabilities,
};
pub use discovery::{
    DiscoveredModel, DiscoveryError, fetch_openrouter_models, minimal_fallback_models,
};
pub use mock::MockModel;
pub use model_trait::{
    GenerationParams, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse, ModelRole,
    ModelStreamChunk, ModelTier, Usage, default_generation_params, system_prompt_template,
};
pub use openai::{DEFAULT_BASE_URL, OpenRouterModel};
