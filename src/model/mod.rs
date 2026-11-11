//! Model abstraction layer.
//!
//! Provides the `Model` trait (text-in/text-out primitive), capability
//! negotiation, and a mock provider for testing.

pub mod mock;
pub mod model_trait;

pub use mock::MockModel;
pub use model_trait::{
    GenerationParams, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse, ModelRole,
    ModelStreamChunk, ModelTier, Usage, default_generation_params, system_prompt_template,
};
