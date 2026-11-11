//! okeanos — autonomous coding agent harness.
//! Milestone 0: CLI scaffolding only; harness subsystems come later.
//! Milestone 1: config layering + secret substitution.

pub mod cli;
pub mod events;
pub mod harness;

pub use harness::error::HarnessError;
