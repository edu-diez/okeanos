//! okeanos — autonomous coding agent harness.
//! Milestone 0: CLI scaffolding only; harness subsystems come later.
//! Milestone 1: config layering + secret substitution.
//! Milestone 2: core harness loop (Model, Context, ReAct).

pub mod acp;
pub mod cli;
pub mod config;
pub mod context;
pub mod cost;
pub mod eval;
pub mod events;
pub mod harness;
pub mod headless;
pub mod model;
pub mod observability;
pub mod session;
pub mod snapshot;
pub mod tool;

#[cfg(feature = "tui")]
pub mod ui;

pub use harness::error::HarnessError;
