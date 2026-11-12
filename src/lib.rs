//! okeanos — autonomous coding agent harness.
//! Milestone 0: CLI scaffolding only; harness subsystems come later.
//! Milestone 1: config layering + secret substitution.
//! Milestone 2: core harness loop (Model, Context, ReAct).

pub mod cli;
pub mod config;
pub mod context;
pub mod events;
pub mod harness;
pub mod model;
pub mod session;
pub mod tool;

pub use harness::error::HarnessError;
