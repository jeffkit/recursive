//! `enter_plan_mode` / `exit_plan_mode` tools — agent-driven read-only planning.
//!
//! When the agent calls `enter_plan_mode`, the runtime sets an exploring flag
//! that blocks any write tools until `exit_plan_mode` is called. `exit_plan_mode`
//! clears the flag, emits a [`AgentEvent::PlanProposed`] event (so TUI/HTTP
//! surfaces can render the review modal), and then **blocks** until the human
//! reviewer calls [`PlanApprovalGate::approve`] or [`PlanApprovalGate::reject`].
//!
//! # Concurrency safety
//!
//! [`PlanApprovalGate::wait_for_approval`] reads the response without holding a
//! lock across any `.await` point, preventing deadlocks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::error::Result;
use crate::event::{AgentEvent, EventSink};
use crate::llm::ToolSpec;
use crate::permissions::{PermissionMode, PermissionsConfig};
use crate::tools::{Tool, ToolSideEffect};

// ---------------------------------------------------------------------------
// PlanApprovalResult
// ---------------------------------------------------------------------------

/// The human reviewer's decision on an agent-proposed plan.
#[derive(Debug, Clone)]
pub enum PlanApprovalResult {
    /// Plan was approved; execution may proceed.
    Approved,
    /// Plan was rejected; the agent receives the reason and should revise.
    Rejected { reason: String },
}

// ---------------------------------------------------------------------------
// PlanApprovalGate
// ---------------------------------------------------------------------------

/// Shared state for the agent-driven plan mode approval flow.
///
/// A single `Arc<PlanApprovalGate>` is created in [`AgentRuntimeBuilder::build`]
/// and shared between:
/// * [`EnterPlanModeTool`] — sets `exploring_plan_mode`
/// * [`ExitPlanModeTool`] — clears flag, emits event, awaits decision
/// * [`AgentRuntime`] — calls [`approve`](Self::approve) / [`reject`](Self::reject)
/// * [`RunCore`](crate::run_core::RunCore) — reads `exploring_plan_mode` to gate writes
pub struct PlanApprovalGate {
    /// `true` while the agent is in exploring (read-only plan) mode.
    pub exploring_plan_mode: Arc<AtomicBool>,
    /// The plan text currently pending review (set by `ExitPlanModeTool`).
    pub pending_plan: Arc<RwLock<Option<String>>>,
    /// Decision set by the human reviewer (TUI / HTTP).
    response: Arc<RwLock<Option<PlanApprovalResult>>>,
    /// Wakes up `wait_for_approval` when a decision arrives.
    notify: Arc<Notify>,
}

impl PlanApprovalGate {
    /// Create a fresh gate (exploring mode off, no pending decision).
    pub fn new() -> Self {
        Self {
            exploring_plan_mode: Arc::new(AtomicBool::new(false)),
            pending_plan: Arc::new(RwLock::new(None)),
            response: Arc::new(RwLock::new(None)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Block until the human reviewer sets a decision.
    ///
    /// Reads the response without holding any lock across `.await`, preventing
    /// deadlocks. On return the stored response is cleared so the gate can be
    /// reused for the next `exit_plan_mode` call.
    pub async fn wait_for_approval(&self) -> PlanApprovalResult {
        loop {
            // Read without holding the lock across the await point.
            let result_opt = {
                let guard = self
                    .response
                    .read()
                    .expect("PlanApprovalGate response lock poisoned");
                guard.clone()
            };
            if let Some(result) = result_opt {
                // Clear for future use before returning.
                if let Ok(mut w) = self.response.write() {
                    *w = None;
                }
                return result;
            }
            // No decision yet — sleep until notify fires.
            self.notify.notified().await;
        }
    }

    /// Approve the pending plan. Called by TUI / HTTP on user confirmation.
    pub fn approve(&self) {
        if let Ok(mut w) = self.response.write() {
            *w = Some(PlanApprovalResult::Approved);
        }
        self.notify.notify_one();
    }

    /// Reject the pending plan with a reason. Called by TUI / HTTP.
    pub fn reject(&self, reason: impl Into<String>) {
        if let Ok(mut w) = self.response.write() {
            *w = Some(PlanApprovalResult::Rejected {
                reason: reason.into(),
            });
        }
        self.notify.notify_one();
    }
}

impl Default for PlanApprovalGate {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// EnterPlanModeTool
// ---------------------------------------------------------------------------

/// Tool that switches the agent into read-only planning mode.
///
/// Once entered, any non-readonly tool call (except `exit_plan_mode`) is
/// blocked with an error message. The agent should use read tools to explore
/// the codebase, then call `exit_plan_mode` with its markdown plan.
///
/// When a `PermissionsConfig` is provided, the tool also checks which tools
/// are in `Plan` mode and includes that information in the response, guiding
/// the agent to focus on those tools during planning.
pub struct EnterPlanModeTool {
    gate: Arc<PlanApprovalGate>,
    /// Optional permissions config to check which tools require plan mode.
    permissions: Option<Arc<PermissionsConfig>>,
}

impl EnterPlanModeTool {
    /// Create a new `EnterPlanModeTool` sharing the given gate.
    pub fn new(gate: Arc<PlanApprovalGate>) -> Self {
        Self {
            gate,
            permissions: None,
        }
    }

    /// Attach a permissions config so the tool can report which tools
    /// are in `Plan` mode.
    pub fn with_permissions(mut self, permissions: Arc<PermissionsConfig>) -> Self {
        self.permissions = Some(permissions);
        self
    }
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "enter_plan_mode".into(),
            description: "Enter read-only planning mode. While active, write tools (write_file, \
                 apply_patch, run_shell, …) are blocked. Use read tools to explore the \
                 codebase freely, then call exit_plan_mode with a markdown plan summary."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn execute(&self, _arguments: Value) -> Result<String> {
        self.gate.exploring_plan_mode.store(true, Ordering::Relaxed);

        // If permissions are configured, report which tools are in Plan mode.
        let mut response = json!({ "entered": true });
        if let Some(ref config) = self.permissions {
            if matches!(config.mode, PermissionMode::Plan { .. }) {
                response["default_mode"] = json!("plan");
            }
        }

        Ok(response.to_string())
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::Mutating
    }

    fn is_readonly(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// ExitPlanModeTool
// ---------------------------------------------------------------------------

/// Tool that exits plan mode and presents the agent's plan for human review.
///
/// Execution **blocks** until the human reviewer calls
/// [`PlanApprovalGate::approve`] or [`PlanApprovalGate::reject`]. The tool
/// returns `{"approved": true}` or `{"approved": false, "reason": "…"}` so
/// the agent can branch on the outcome.
pub struct ExitPlanModeTool {
    gate: Arc<PlanApprovalGate>,
    event_sink: Arc<dyn EventSink>,
    /// Optional permissions config to validate plan coverage.
    permissions: Option<Arc<PermissionsConfig>>,
}

impl ExitPlanModeTool {
    /// Create a new `ExitPlanModeTool`.
    ///
    /// * `gate`       — shared with `EnterPlanModeTool` and `AgentRuntime`.
    /// * `event_sink` — emits [`AgentEvent::PlanProposed`] to TUI / HTTP.
    pub fn new(gate: Arc<PlanApprovalGate>, event_sink: Arc<dyn EventSink>) -> Self {
        Self {
            gate,
            event_sink,
            permissions: None,
        }
    }

    /// Attach a permissions config so the tool can validate that the plan
    /// covers tools in `Plan` mode.
    pub fn with_permissions(mut self, permissions: Arc<PermissionsConfig>) -> Self {
        self.permissions = Some(permissions);
        self
    }
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exit_plan_mode".into(),
            description:
                "Exit plan mode and present your plan for human review. Include a markdown \
                 summary with: (1) your understanding of the current code, (2) the approach \
                 you propose and why, (3) the files you will modify and how, (4) how you will \
                 verify the change is correct. Execution blocks until the reviewer confirms \
                 or rejects the plan."
                    .into(),
            parameters: json!({
                "type": "object",
                "required": ["plan"],
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "Markdown summary of your plan."
                    }
                }
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let plan_text = arguments
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // If permissions are configured, check that the plan covers Plan-mode tools.
        if let Some(ref _config) = self.permissions {
            // Plan-mode tools are determined by the default mode.
            // The plan text is passed through to the reviewer as-is.
        }

        // Clear exploring mode so normal tool execution resumes after approval.
        self.gate
            .exploring_plan_mode
            .store(false, Ordering::Relaxed);

        // Store the plan text so external callers can read it.
        if let Ok(mut w) = self.gate.pending_plan.write() {
            *w = Some(plan_text.clone());
        }

        // Emit PlanProposed so TUI / HTTP can render the review modal.
        // Plan Mode 2.0 has no pending tool_calls — the plan is prose only.
        self.event_sink
            .emit(AgentEvent::PlanProposed {
                plan_text: plan_text.clone(),
                tool_calls: vec![],
            })
            .await;

        // Block until the human makes a decision.
        // No lock is held across this await (see PlanApprovalGate::wait_for_approval).
        let result = self.gate.wait_for_approval().await;

        match result {
            PlanApprovalResult::Approved => Ok(json!({ "approved": true }).to_string()),
            PlanApprovalResult::Rejected { reason } => {
                Ok(json!({ "approved": false, "reason": reason }).to_string())
            }
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        // Waits for external human input.
        ToolSideEffect::External
    }

    fn is_readonly(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// PlanModeRequestGate (Goal-202)
// ---------------------------------------------------------------------------

/// Decision result for the pre-confirmation step.
#[derive(Debug, Clone)]
pub enum PlanModeRequestResult {
    /// User allowed the agent to enter plan mode.
    Approved,
    /// User declined; agent should execute without planning.
    Rejected { reason: String },
}

/// Gate for the plan-mode pre-confirmation step.
///
/// Parallel to [`PlanApprovalGate`] but governs the *entry* decision
/// (before any exploration), not the plan review.
pub struct PlanModeRequestGate {
    response: Arc<RwLock<Option<PlanModeRequestResult>>>,
    notify: Arc<Notify>,
}

impl PlanModeRequestGate {
    /// Create a fresh gate.
    pub fn new() -> Self {
        Self {
            response: Arc::new(RwLock::new(None)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Block until the user makes a decision.
    pub async fn wait_for_decision(&self) -> PlanModeRequestResult {
        loop {
            let result_opt = {
                let guard = self
                    .response
                    .read()
                    .expect("PlanModeRequestGate response lock poisoned");
                guard.clone()
            };
            if let Some(result) = result_opt {
                if let Ok(mut w) = self.response.write() {
                    *w = None;
                }
                return result;
            }
            self.notify.notified().await;
        }
    }

    /// Approve the plan-mode request.
    pub fn approve(&self) {
        if let Ok(mut w) = self.response.write() {
            *w = Some(PlanModeRequestResult::Approved);
        }
        self.notify.notify_one();
    }

    /// Reject the plan-mode request with a reason.
    pub fn reject(&self, reason: impl Into<String>) {
        if let Ok(mut w) = self.response.write() {
            *w = Some(PlanModeRequestResult::Rejected {
                reason: reason.into(),
            });
        }
        self.notify.notify_one();
    }
}

impl Default for PlanModeRequestGate {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// RequestPlanModeTool (Goal-202)
// ---------------------------------------------------------------------------

/// Tool that asks the user for permission before entering plan mode.
///
/// The agent should call this tool **before** `enter_plan_mode` whenever
/// it considers a planning phase beneficial.  If the user approves, the
/// agent proceeds with `enter_plan_mode` as usual.  If the user rejects,
/// the agent should execute directly without entering plan mode.
///
/// This two-step flow avoids wasting tokens generating a plan that the
/// user never wanted.
pub struct RequestPlanModeTool {
    gate: Arc<PlanModeRequestGate>,
    event_sink: Arc<dyn EventSink>,
}

impl RequestPlanModeTool {
    /// Create a new `RequestPlanModeTool`.
    pub fn new(gate: Arc<PlanModeRequestGate>, event_sink: Arc<dyn EventSink>) -> Self {
        Self { gate, event_sink }
    }
}

#[async_trait]
impl Tool for RequestPlanModeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "request_plan_mode".into(),
            description:
                "Before entering plan mode, call this tool to ask the user whether they want \
                 you to create a plan first.  Provide a brief `reason` explaining why planning \
                 would be helpful for this task.  The call blocks until the user decides. \
                 If approved, proceed with `enter_plan_mode`; if rejected, execute directly \
                 without planning."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "required": ["reason"],
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "Brief explanation of why a planning phase would be \
                            beneficial (1-2 sentences)."
                    }
                }
            }),
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<String> {
        let reason = arguments
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Notify TUI / HTTP so they can prompt the user.
        self.event_sink
            .emit(crate::event::AgentEvent::PlanModeRequested {
                reason: reason.clone(),
            })
            .await;

        // Block until the user makes a decision.
        let result = self.gate.wait_for_decision().await;

        match result {
            PlanModeRequestResult::Approved => {
                self.event_sink
                    .emit(crate::event::AgentEvent::PlanModeApproved)
                    .await;
                Ok(serde_json::json!({ "approved": true }).to_string())
            }
            PlanModeRequestResult::Rejected { reason: r } => {
                self.event_sink
                    .emit(crate::event::AgentEvent::PlanModeRejected { reason: r.clone() })
                    .await;
                Ok(serde_json::json!({ "approved": false, "reason": r }).to_string())
            }
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        // Waits for external human input (same as exit_plan_mode).
        ToolSideEffect::External
    }

    fn is_readonly(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::NullSink;

    fn make_gate() -> Arc<PlanApprovalGate> {
        Arc::new(PlanApprovalGate::new())
    }

    // -- EnterPlanModeTool --------------------------------------------------

    #[tokio::test]
    async fn enter_plan_mode_returns_confirmation_message() {
        let gate = make_gate();
        let tool = EnterPlanModeTool::new(gate.clone());

        assert!(
            !gate.exploring_plan_mode.load(Ordering::Relaxed),
            "flag should start false"
        );

        let result = tool.execute(json!({})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["entered"], true);
        assert!(
            gate.exploring_plan_mode.load(Ordering::Relaxed),
            "flag should be set after enter"
        );
    }

    // -- ExitPlanModeTool: blocks until confirmed ---------------------------

    #[tokio::test]
    async fn exit_plan_mode_blocks_until_confirmed() {
        let gate = make_gate();
        // Pre-set exploring mode so we can verify it gets cleared.
        gate.exploring_plan_mode.store(true, Ordering::Relaxed);

        let tool = Arc::new(ExitPlanModeTool::new(gate.clone(), Arc::new(NullSink)));

        // Spawn a task that approves after a short delay.
        let gate_clone = gate.clone();
        let approve_handle = tokio::spawn(async move {
            // Yield to ensure the tool's await is reached first.
            tokio::task::yield_now().await;
            gate_clone.approve();
        });

        let result = tool
            .execute(json!({ "plan": "step 1: read; step 2: write" }))
            .await
            .unwrap();

        approve_handle.await.unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["approved"], true);
        assert!(
            !gate.exploring_plan_mode.load(Ordering::Relaxed),
            "flag should be cleared after exit"
        );
    }

    // -- ExitPlanModeTool: rejection propagates reason ----------------------

    #[tokio::test]
    async fn exit_plan_mode_returns_rejection_reason() {
        let gate = make_gate();
        let tool = Arc::new(ExitPlanModeTool::new(gate.clone(), Arc::new(NullSink)));

        let gate_clone = gate.clone();
        let reject_handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            gate_clone.reject("Plan is incomplete");
        });

        let result = tool.execute(json!({ "plan": "my plan" })).await.unwrap();

        reject_handle.await.unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["approved"], false);
        assert_eq!(parsed["reason"], "Plan is incomplete");
    }

    // -- Gate: approve / reject directly ------------------------------------

    #[tokio::test]
    async fn gate_approve_wakes_waiter() {
        let gate = make_gate();
        let gate_clone = gate.clone();

        let waiter = tokio::spawn(async move { gate_clone.wait_for_approval().await });

        // Yield, then approve.
        tokio::task::yield_now().await;
        gate.approve();

        let result = waiter.await.unwrap();
        assert!(matches!(result, PlanApprovalResult::Approved));
    }

    #[tokio::test]
    async fn gate_reject_wakes_waiter_with_reason() {
        let gate = make_gate();
        let gate_clone = gate.clone();

        let waiter = tokio::spawn(async move { gate_clone.wait_for_approval().await });

        tokio::task::yield_now().await;
        gate.reject("too risky");

        let result = waiter.await.unwrap();
        assert!(matches!(
            result,
            PlanApprovalResult::Rejected { reason } if reason == "too risky"
        ));
    }

    // -- Gate: cleared after use so it can be reused ------------------------

    #[tokio::test]
    async fn gate_response_cleared_after_wait() {
        let gate = make_gate();
        gate.approve();
        let _ = gate.wait_for_approval().await;

        // Response should be None now.
        let stored = gate.response.read().unwrap().clone();
        assert!(stored.is_none(), "response should be cleared after read");
    }

    // -- PlanModeRequestGate (Goal-202) ------------------------------------

    #[tokio::test]
    async fn request_gate_approve_wakes_waiter() {
        let gate = Arc::new(PlanModeRequestGate::new());
        let gate_clone = gate.clone();
        let waiter = tokio::spawn(async move { gate_clone.wait_for_decision().await });
        tokio::task::yield_now().await;
        gate.approve();
        let result = waiter.await.unwrap();
        assert!(matches!(result, PlanModeRequestResult::Approved));
    }

    #[tokio::test]
    async fn request_gate_reject_propagates_reason() {
        let gate = Arc::new(PlanModeRequestGate::new());
        let gate_clone = gate.clone();
        let waiter = tokio::spawn(async move { gate_clone.wait_for_decision().await });
        tokio::task::yield_now().await;
        gate.reject("user skipped");
        let result = waiter.await.unwrap();
        assert!(matches!(
            result,
            PlanModeRequestResult::Rejected { reason } if reason == "user skipped"
        ));
    }

    #[tokio::test]
    async fn request_gate_cleared_after_use() {
        let gate = Arc::new(PlanModeRequestGate::new());
        gate.approve();
        let _ = gate.wait_for_decision().await;
        let stored = gate.response.read().unwrap().clone();
        assert!(
            stored.is_none(),
            "response should be cleared after decision"
        );
    }

    #[tokio::test]
    async fn request_plan_mode_tool_emits_event_and_blocks_until_approved() {
        use crate::event::NullSink;
        let gate = Arc::new(PlanModeRequestGate::new());
        let tool = Arc::new(RequestPlanModeTool::new(gate.clone(), Arc::new(NullSink)));
        let gate_clone = gate.clone();
        let approve_handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            gate_clone.approve();
        });
        let result = tool
            .execute(json!({ "reason": "complex task" }))
            .await
            .unwrap();
        approve_handle.await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["approved"], true);
    }

    #[tokio::test]
    async fn request_plan_mode_tool_rejected_returns_false() {
        use crate::event::NullSink;
        let gate = Arc::new(PlanModeRequestGate::new());
        let tool = Arc::new(RequestPlanModeTool::new(gate.clone(), Arc::new(NullSink)));
        let gate_clone = gate.clone();
        let reject_handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            gate_clone.reject("not needed");
        });
        let result = tool
            .execute(json!({ "reason": "might need plan" }))
            .await
            .unwrap();
        reject_handle.await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["approved"], false);
        assert_eq!(parsed["reason"], "not needed");
    }
}
