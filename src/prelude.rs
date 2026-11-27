//! Shared imports for the binary crate's modules.
#![allow(unused_imports)]

pub(crate) use anyhow::Context as _;
pub(crate) use clap::Parser;
pub(crate) use futures::Stream;
pub(crate) use okeanos::cli::{Cli, Command, ConfigAction, Mode, Shell, Verbosity};
pub(crate) use okeanos::config::Config;
pub(crate) use okeanos::context::memory::user_config_dir;
pub(crate) use okeanos::context::{
    Conversation, MemoryConfig, ProjectContext, build_system_prompt_with_context,
    tokens::default_pricing_table,
};
pub(crate) use okeanos::cost::{CostBudgetConfig, CostTracker};
#[allow(unused_imports)]
pub(crate) use okeanos::events::{Approval, ApprovalDecision, Event, Log, LogLevel};
pub(crate) use okeanos::harness::{ReactConfig, ReactLoop, policy::Policy};
pub(crate) use okeanos::headless::{EventEmitter, HeadlessConfig};
pub(crate) use okeanos::model::{
    GenerationParams, MockModel, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse,
    ModelStreamChunk,
};
pub(crate) use okeanos::session::{SessionManager, replay_events};
pub(crate) use okeanos::snapshot::store::SnapshotStore;
pub(crate) use okeanos::tool::TodoList;
pub(crate) use okeanos::tool::{ToolRegistry, ToolSpec};

#[cfg(feature = "mcp")]
pub(crate) use okeanos::tool::mcp::{
    McpClient, McpClientConfig, McpServerConfig, McpStartupPolicy,
};
pub(crate) use std::pin::Pin;
pub(crate) use std::sync::Arc;
pub(crate) use tokio::sync::{Mutex, mpsc};
pub(crate) use tracing_subscriber::EnvFilter;

#[cfg(feature = "tui")]
pub(crate) use okeanos::ui::TerminalApp;
#[cfg(all(feature = "tui", feature = "markdown"))]
pub(crate) use okeanos::ui::components::markdown::RenderMode;

pub(crate) use crate::commands::*;
pub(crate) use crate::modes::*;
pub(crate) use crate::wiring::*;
#[cfg(feature = "rpc")]
pub(crate) use {
    jsonrpsee::server::{PingConfig, RpcModule, ServerBuilder},
    jsonrpsee::types::{ErrorObject, Params},
    std::collections::HashMap,
    std::sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    tokio::sync::{Mutex as TokioMutex, RwLock},
    tokio::time::Duration,
};
