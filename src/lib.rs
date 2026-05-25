//! Recursive: a minimal, orthogonal, self-improving coding agent kernel.
//!
//! The kernel is intentionally tiny:
//!   - `Message` is the only data primitive shared across the system.
//!   - `LlmProvider` abstracts model backends (HTTP, mock, future local...).
//!   - `Tool` abstracts side effects the model can request.
//!   - `Agent` is a thin loop that wires them together.
//!
//! Everything else is opt-in. New capabilities are added by implementing
//! `Tool` or `LlmProvider`, never by editing the loop.

pub mod agent;
pub mod config;
pub mod error;
pub mod llm;
pub mod message;
pub mod tools;

pub use agent::{Agent, AgentOutcome, StepEvent};
pub use config::Config;
pub use error::{Error, Result};
pub use llm::{pricing_for, Completion, LlmProvider, ModelPricing, TokenUsage, ToolCall, ToolSpec};
pub use message::{Message, Role};
pub use tools::{Tool, ToolRegistry};
