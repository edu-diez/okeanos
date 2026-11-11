//! Conversation context management.
//!
//! Handles system prompt, skills, message history, token counting,
//! cost estimation, and hard context cap enforcement.

pub mod cap;
pub mod conversation;
pub mod tokens;

pub use cap::{ContextCap, ContextCapError, ContextWarning};
pub use conversation::{Conversation, ConversationError, build_system_prompt, load_skills};
pub use tokens::{
    CostEstimate, ModelPricing, PricingProvider, TokenCounter, default_pricing_table, estimate_cost,
};
