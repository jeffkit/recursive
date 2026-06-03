//! Agent types — `FinishReason`, `PermissionDecision`, `PermissionHook`,
//! and `PlanningMode`.
//!
//! The legacy `Agent` / `AgentBuilder` / `AgentOutcome` / `OnMessageFn` /
//! `StepEvent` types have been removed in Goal 219. Use
//! [`AgentRuntime`](crate::runtime::AgentRuntime) and
//! [`AgentEvent`](crate::event::AgentEvent) instead.

pub mod types;
pub use types::*;
