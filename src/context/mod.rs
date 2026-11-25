//! Conversation context management.
//!
//! Handles system prompt, skills, message history, token counting,
//! cost estimation, hard context cap enforcement, tiered prompts,
//! selective file inclusion, context window warnings, and compaction.

pub mod cap;
pub mod compaction;
pub mod conversation;
pub mod selective;
pub mod tier;
pub mod tokens;
pub mod warnings;

pub use cap::{ContextCap, ContextCapError};
pub use compaction::{
    CompactionBudget, CompactionConfig, CompactionError, CompactionManager,
    CompactionModelOverride, CompactionObserver, CompactionPlan, CompactionProvidersConfig,
    CompactionResult, CompactionStrategyConfig, FileOperationSummary, NativeCompactionRequest,
    NativeCompactionResponse, NoopCompactionObserver, summarize_plan,
};
pub use conversation::{
    Conversation, ConversationError, PruneStats, build_system_prompt, load_skills,
};
pub use selective::{
    IncludedFile, SelectiveContextConfig, SelectiveContextError, build_file_context,
    expand_mentions, find_relevant_files,
};
pub use tier::{TierConfig, tool_subsets};
pub use tokens::{
    CostEstimate, ModelPricing, PricingProvider, TokenCounter, default_pricing_table, estimate_cost,
};
pub use warnings::{
    ContextTracker, ContextUsage, WarningConfig, WarningLevel, check_and_emit_warnings,
};
