//! Non-interactive subcommands (`doctor`, `sessions`, `models`, ...).

mod doctor;
mod init;
mod mcp;
mod models;
mod sessions;
mod stats_export_replay;
mod trust;

pub(crate) use doctor::cmd_doctor;
pub(crate) use init::{cmd_config_validate, cmd_init, generate_completions};
pub(crate) use mcp::cmd_mcp;
pub(crate) use models::cmd_models;
pub(crate) use sessions::cmd_sessions;
pub(crate) use stats_export_replay::{cmd_export, cmd_replay, cmd_stats};
pub(crate) use trust::{cmd_trust, handle_untrusted_project};
