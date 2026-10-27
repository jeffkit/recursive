//! Output helpers: usage printing, transcript saving, cost tracking, event streaming.

use std::path::Path;
use std::sync::Arc;

use recursive::llm::{pricing_for, TokenUsage};
use recursive::{
    AgentEvent, FinishReason, SessionFile, SessionStatus, SessionWriter, TranscriptFile,
};
use tokio::sync::mpsc;

pub(crate) fn print_usage(usage: TokenUsage, model: &str, total_llm_latency_ms: u64, steps: usize) {
    if usage.total_tokens > 0 {
        eprintln!(
            "tokens: prompt={} completion={} total={}",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
        );
        if usage.cache_hit_tokens > 0 {
            let total_cache = usage.cache_hit_tokens + usage.cache_miss_tokens;
            let hit_rate = if total_cache > 0 {
                (usage.cache_hit_tokens as f64 / total_cache as f64) * 100.0
            } else {
                0.0
            };
            eprintln!(
                "cache: hit={} miss={} ({:.1}% hit rate)",
                usage.cache_hit_tokens, usage.cache_miss_tokens, hit_rate
            );
        }
        if let Some(pricing) = pricing_for(model) {
            let cost = pricing.cost_usd(usage);
            eprintln!("cost: ${:.4} ({})", cost, model);
        }
    }
    if total_llm_latency_ms > 0 && steps > 0 {
        let avg = total_llm_latency_ms / steps as u64;
        eprintln!(
            "llm latency: total={}ms avg={}ms over {} steps",
            total_llm_latency_ms, avg, steps
        );
    }
}

pub(crate) fn print_finish_note(finish: &FinishReason) {
    match finish {
        FinishReason::TranscriptLimit { chars, limit } => {
            eprintln!(
                "note: stopped because transcript reached {} chars (limit {})",
                chars, limit
            );
        }
        FinishReason::Cancelled => {
            eprintln!("shutdown: agent stopped at next step boundary after signal");
        }
        _ => {}
    }
}

/// Save the transcript to disk if a path was requested. Always called
/// before any exit-code decision so auto-resume (which keys off the
/// transcript file's existence) works even when the agent terminated
/// abnormally (e.g. BudgetExceeded).
pub(crate) fn save_transcript(
    outcome_transcript: &[recursive::message::Message],
    outcome_steps: usize,
    model: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let file = TranscriptFile::new(
        outcome_transcript.to_vec(),
        outcome_steps,
        Some(model.into()),
    );
    file.write_to(path)?;
    eprintln!(
        "transcript: wrote {} messages to {}",
        outcome_transcript.len(),
        path.display()
    );
    Ok(())
}

/// Save a session file for non-success finishes.
pub(crate) fn save_session(
    transcript: &[recursive::message::Message],
    steps: usize,
    goal: String,
    model: &str,
    provider: &str,
    tool_specs: &[recursive::ToolSpec],
    path: &Path,
) -> anyhow::Result<()> {
    let session = SessionFile::new(
        goal,
        model.to_string(),
        provider.to_string(),
        tool_specs,
        steps,
        transcript.to_vec(),
    );
    session.write_to(path)?;
    eprintln!(
        "session: wrote {} messages to {}",
        transcript.len(),
        path.display()
    );
    Ok(())
}

/// Return Err iff the finish reason should propagate as a non-zero binary
/// exit code so that the self-improve flow's auto-resume step fires. The
/// transcript has already been saved by the caller before this is called.
///
/// `Cancelled` is intentionally **not** an error: shutdown via SIGINT
/// or SIGTERM is user-initiated, the saved transcript is intact, and
/// the self-improve flow must NOT auto-resume something the user explicitly
/// stopped.
///
/// Every non-success, non-cancel finish reason is an error (exit 1) so a
/// supervising script can distinguish "agent finished normally" from "agent
/// got stuck / hit the wall-clock deadline / provider crashed".
pub(crate) fn exit_for_finish(finish: &FinishReason, steps: usize) -> anyhow::Result<()> {
    match finish {
        // True success — model produced a final response without tool calls.
        FinishReason::NoMoreToolCalls => Ok(()),
        // User-initiated shutdown (SIGINT/SIGTERM); must not auto-resume.
        FinishReason::Cancelled => Ok(()),
        FinishReason::BudgetExceeded => {
            anyhow::bail!("agent exceeded step budget ({steps})")
        }
        FinishReason::WallClockExceeded { secs } => {
            anyhow::bail!("agent exceeded wall-clock timeout ({secs}s)")
        }
        FinishReason::Stuck {
            repeated_call,
            repeats,
        } => {
            anyhow::bail!("agent stuck: repeated tool call '{repeated_call}' ({repeats}x)")
        }
        FinishReason::TranscriptLimit { chars, limit } => {
            anyhow::bail!(
                "transcript size {chars} exceeded hard limit {limit} and could not be reduced"
            )
        }
        FinishReason::PermissionDenialLimit => {
            anyhow::bail!("agent hit permission denial limit (loop of denied tool calls)")
        }
        FinishReason::ProviderStop(reason) => {
            // `run_core` only constructs `ProviderStop` for reasons other than
            // the normal stop signals ("stop"/"end_turn" map to
            // `NoMoreToolCalls`); treat those plus a bare empty reason as
            // success, and everything else (e.g. "rate_limited", "404",
            // "context_length_exceeded") as a failure.
            if reason == "stop" || reason == "end_turn" || reason.is_empty() {
                Ok(())
            } else {
                anyhow::bail!("provider stopped: {reason}")
            }
        }
        // `FinishReason` is `#[non_exhaustive]`, so a wildcard arm is required
        // from this crate. Treat unknown future variants conservatively as
        // errors — a supervisor must never mistake an unrecognised terminal
        // state for success.
        _ => anyhow::bail!("agent finished with unknown reason: {finish}"),
    }
}

/// Map a turn's `FinishReason` to the `(SessionStatus, finish_reason)` pair
/// written to `.meta.json`.
///
/// Used by `run_loop`: the loop's *last* turn decides the whole session's
/// status. `NoMoreToolCalls` → `Completed`; `Cancelled` (SIGINT/SIGTERM via
/// the shutdown token) → `Interrupted` (user-initiated, resumable); every
/// other finish (budget / stuck / transcript-limit / provider-stop / …) →
/// `Crashed` **plus** the canonical reason string that keeps those causes
/// apart on disk (issue #111).
///
/// Delegates to the session crate's exhaustive mapping rather than keeping
/// a second `_ => Crashed` table here, so the two cannot drift.
pub(crate) fn finish_to_session_status(finish: &FinishReason) -> (SessionStatus, Option<String>) {
    SessionStatus::for_finish(finish)
}

/// Close a run's session envelope (`status` + failure detail on
/// `.meta.json`), then report where it landed.
///
/// The writer is *shared*: JSON-mode runs hand a clone to the control session
/// (and its stdin demux), which outlives the runtime. Requiring unique
/// ownership here silently skipped every such finalize — the session stayed
/// `active` forever (issue #110). Locking the mutex is enough: every caller
/// drops the runtime — and thus the persistence sink — first, so no append
/// can interleave, and the mutex serializes us against any straggler.
pub(crate) fn finalize_session_writer(
    session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>>,
    status: SessionStatus,
    finish_reason: Option<String>,
    error: Option<String>,
) {
    let Some(sw) = session_writer else { return };
    let mut w = match sw.lock() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("session: failed to lock writer: {e}");
            return;
        }
    };
    if let Err(e) = w.finish_with_details(status, finish_reason, error) {
        eprintln!("session: failed to finalize: {e}");
    } else {
        eprintln!(
            "session: saved {} message(s) to {}",
            w.message_count(),
            w.session_dir().display()
        );
    }
}

pub(crate) fn finalize_cost_tracker(
    cost_tracker: Option<std::sync::Mutex<recursive::cost::CostTracker>>,
    usage: recursive::llm::TokenUsage,
    llm_latency_ms: u64,
    model: &str,
) {
    let Some(tracker) = cost_tracker else { return };
    match tracker.into_inner() {
        Ok(mut t) => {
            t.record_usage(usage, llm_latency_ms);
            if let Err(e) = t.finish() {
                eprintln!("cost: failed to write cost.json: {e}");
            } else {
                eprintln!("{}", format_cost_line(t.cost_usd(), model));
            }
        }
        Err(e) => eprintln!("cost: failed to lock cost tracker: {e}"),
    }
}

pub(crate) fn format_cost_line(cost: Option<f64>, model: &str) -> String {
    match cost {
        Some(c) => format!("cost: ${c:.4} ({model})"),
        None => format!("cost: unknown (no pricing for {model})"),
    }
}

/// Issue #119: render a delegated worker's forwarded event for the human CLI
/// stream. Returns `None` for the worker's internal events (step-level noise
/// stays out of the parent's stream). Kept pure so the mapping is unit-testable
/// without capturing stdout.
fn format_worker_event(worker_id: &str, event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
            ..
        } => Some(format!(
            "[worker {worker_id}] tokens: prompt={input_tokens} completion={output_tokens}"
        )),
        AgentEvent::AssistantText { text, .. } if !text.trim().is_empty() => {
            Some(format!("[worker {worker_id}] {text}"))
        }
        AgentEvent::ToolCall { name, .. } => Some(format!("[worker {worker_id}] -> {name}")),
        AgentEvent::TurnFinished { reason, steps } => Some(format!(
            "[worker {worker_id}] done after {steps} steps (reason: {reason})"
        )),
        _ => None,
    }
}

pub(crate) async fn stream_events(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            // Issue #119: a delegated worker's activity was previously lost
            // with its `NullSink`; surface the interesting parts, attributed.
            AgentEvent::WorkerEvent {
                ref worker_id,
                ref event,
                ..
            } => {
                if let Some(line) = format_worker_event(worker_id, event) {
                    println!("{line}");
                }
            }
            AgentEvent::AssistantText { ref text, step } if !text.trim().is_empty() => {
                println!("[step {step}] assistant: {text}");
            }
            AgentEvent::ToolCall {
                ref name,
                ref arguments,
                step,
                ..
            } => {
                println!("[step {step}] -> {name} {arguments}");
            }
            AgentEvent::ToolResult {
                ref name,
                ref output,
                step,
                ..
            } => {
                let preview = if output.len() > 800 {
                    let mut end = 800.min(output.len());
                    while end > 0 && !output.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}\n...[truncated]", &output[..end])
                } else {
                    output.clone()
                };
                println!("[step {step}] <- {name}\n{preview}");
            }
            AgentEvent::TurnFinished { ref reason, steps } => {
                println!("[done after {steps} steps] reason: {reason}");
            }
            AgentEvent::Latency { step, llm_ms } => {
                println!("[step {step}] llm latency: {llm_ms}ms");
            }
            AgentEvent::Compacted {
                removed,
                kept,
                summary_chars,
                step,
            } => {
                println!(
                    "[step {step}] compacted {removed} msgs -> {kept} kept + {summary_chars}-char summary"
                );
            }
            AgentEvent::PlanProposed { ref plan_text, .. } => {
                println!("[plan] proposed: {plan_text}");
            }
            AgentEvent::PlanConfirmed => {
                println!("[plan] confirmed");
            }
            AgentEvent::PlanRejected { ref reason } => {
                println!("[plan] rejected: {reason}");
            }
            _ => {}
        }
    }
}

/// REPL-specific event handler: clean output without step prefixes on assistant text.
/// Tool calls are shown briefly; assistant text is printed directly.
///
/// TODO(plan-mode-repl): implement y/n approval prompt for PlanProposed events.
/// When `build_runtime(interactive=true)` is restored for the REPL (see
/// `src/main.rs:repl`), this handler must:
///   - on `PlanProposed`: print the plan, ask "Approve plan? [y/n]: ", read stdin,
///     and call `gate.approve()` or `gate.reject(&reason)`.
///   - on `PlanConfirmed` / `PlanRejected`: print a note.
///
/// Issue #48 / Goal 409: until that dialog exists, the plan-approval wait is
/// the one await where the REPL looks frozen while it is actually waiting on
/// a reviewer that cannot answer. Print a hint so the "fake hang" is
/// distinguishable from a real one (Ctrl-C now ends the wait).
pub(crate) async fn stream_events_repl(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            // Issue #119: surface delegated-worker activity in the REPL too.
            AgentEvent::WorkerEvent {
                ref worker_id,
                ref event,
                ..
            } => {
                if let Some(line) = format_worker_event(worker_id, event) {
                    println!("{line}");
                }
            }
            AgentEvent::AssistantText { ref text, .. } if !text.trim().is_empty() => {
                println!("{text}");
            }
            AgentEvent::ToolCall { ref name, .. } => {
                eprintln!("  ↳ {name}");
            }
            AgentEvent::PlanProposed { ref plan_text, .. } => {
                println!("[plan] proposed: {plan_text}");
                eprintln!(
                    "[plan] waiting for plan approval — no reviewer is attached in repl; \
Ctrl-C to cancel the wait (use tui/http surfaces to approve or reject)"
                );
            }
            _ => {}
        }
    }
}

/// Legacy Recursive wire: raw [`AgentEvent`] as NDJSON.
pub(crate) async fn stream_events_json(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        if let Ok(line) = serde_json::to_string(&ev) {
            println!("{line}");
        }
    }
}

/// Claude Code–compatible stream-json: translate events and print NDJSON.
///
/// The terminal `result` object is **not** emitted here — the caller prints
/// it via [`crate::cli::claude_json::ClaudeJsonEmitter::build_result`] after
/// the run finishes (so cost / finish reason are accurate).
///
/// When `bridge` is `Some`, stdout writes go through
/// [`crate::cli::control::ControlBridge::println_locked`] so they cannot
/// interleave with `control_request` frames.
pub(crate) async fn stream_events_claude_json(
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut emitter: crate::cli::claude_json::ClaudeJsonEmitter,
    bridge: Option<std::sync::Arc<crate::cli::control::ControlBridge>>,
) -> crate::cli::claude_json::ClaudeJsonEmitter {
    if let Some(init) = emitter.take_init() {
        emit_json_line(&init, bridge.as_deref()).await;
    }
    while let Some(ev) = rx.recv().await {
        for obj in emitter.on_event(ev) {
            emit_json_line(&obj, bridge.as_deref()).await;
        }
    }
    emitter
}

async fn emit_json_line(
    value: &serde_json::Value,
    bridge: Option<&crate::cli::control::ControlBridge>,
) {
    if let Some(b) = bridge {
        b.println_locked(value).await;
    } else {
        crate::cli::claude_json::println_json(value);
    }
}

/// Drain the event channel without printing (used by Claude `--output-format json`
/// which only emits the terminal result object).
async fn drain_events(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while rx.recv().await.is_some() {}
}

/// Background task that owns the event receiver for a JSON-mode run.
pub(crate) enum JsonEventTask {
    Legacy(tokio::task::JoinHandle<()>),
    /// Drain only; result printed after the run.
    Single {
        drain: tokio::task::JoinHandle<()>,
        emitter: Box<crate::cli::claude_json::ClaudeJsonEmitter>,
    },
    Stream {
        handle: tokio::task::JoinHandle<crate::cli::claude_json::ClaudeJsonEmitter>,
    },
}

impl JsonEventTask {
    pub(crate) fn spawn(
        mode: crate::cli::claude_json::JsonOutputMode,
        rx: mpsc::UnboundedReceiver<AgentEvent>,
        ctx: crate::cli::claude_json::ClaudeJsonContext,
        bridge: Option<std::sync::Arc<crate::cli::control::ControlBridge>>,
    ) -> Self {
        use crate::cli::claude_json::{ClaudeJsonEmitter, JsonOutputMode};
        match mode {
            JsonOutputMode::Legacy => Self::Legacy(tokio::spawn(stream_events_json(rx))),
            JsonOutputMode::Single => Self::Single {
                drain: tokio::spawn(drain_events(rx)),
                emitter: Box::new(ClaudeJsonEmitter::new(ctx)),
            },
            JsonOutputMode::Stream => {
                let emitter = ClaudeJsonEmitter::new(ctx);
                Self::Stream {
                    handle: tokio::spawn(stream_events_claude_json(rx, emitter, bridge)),
                }
            }
        }
    }

    /// Await the background task and print the Claude `result` envelope when
    /// applicable. Legacy mode prints nothing extra.
    pub(crate) async fn finish(
        self,
        finish: &FinishReason,
        final_text: Option<&str>,
        usage: TokenUsage,
        llm_latency_ms: u64,
        steps: usize,
        bridge: Option<&crate::cli::control::ControlBridge>,
    ) {
        match self {
            Self::Legacy(h) => {
                h.await.ok();
            }
            Self::Single { drain, emitter } => {
                drain.await.ok();
                let result = emitter.build_result(finish, final_text, usage, llm_latency_ms, steps);
                emit_json_line(&result, bridge).await;
            }
            Self::Stream { handle } => {
                if let Ok(emitter) = handle.await {
                    let result =
                        emitter.build_result(finish, final_text, usage, llm_latency_ms, steps);
                    emit_json_line(&result, bridge).await;
                }
            }
        }
    }

    /// Like [`finish`](Self::finish) but skips the terminal `result` line.
    ///
    /// Used by `--input-format stream-json` multi-turn mode, where each turn
    /// already emitted its own Claude `result` envelope (matching the Claude
    /// Agent SDK streaming-input contract).
    pub(crate) async fn finish_without_result(self) {
        match self {
            Self::Legacy(h) => {
                h.await.ok();
            }
            Self::Single { drain, .. } => {
                drain.await.ok();
            }
            Self::Stream { handle } => {
                handle.await.ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_to_session_status_maps_cancelled_to_interrupted() {
        assert_eq!(
            finish_to_session_status(&FinishReason::Cancelled),
            (SessionStatus::Interrupted, None)
        );
    }

    #[test]
    fn format_cost_line_priced_model_shows_amount() {
        let line = format_cost_line(Some(0.00028), "deepseek-chat");
        assert_eq!(line, "cost: $0.0003 (deepseek-chat)");
    }

    #[test]
    fn format_cost_line_unknown_model_shows_unknown() {
        let line = format_cost_line(None, "no-such-model");
        assert!(line.contains("unknown"), "line: {line}");
        assert!(
            !line.contains("$0.0000"),
            "must not fake a zero cost: {line}"
        );
        assert_eq!(line, "cost: unknown (no pricing for no-such-model)");
    }

    #[test]
    fn finish_to_session_status_maps_success_to_completed() {
        assert_eq!(
            finish_to_session_status(&FinishReason::NoMoreToolCalls),
            (SessionStatus::Completed, None)
        );
    }

    #[test]
    fn finish_to_session_status_maps_errors_to_crashed_with_reason() {
        // Issue #111: the failure variants all share `Crashed`, so the
        // reason string is the only thing that tells them apart on disk.
        assert_eq!(
            finish_to_session_status(&FinishReason::BudgetExceeded),
            (SessionStatus::Crashed, Some("budget_exceeded".to_string()))
        );
        assert_eq!(
            finish_to_session_status(&FinishReason::Stuck {
                repeated_call: "x".into(),
                repeats: 3,
            }),
            (SessionStatus::Crashed, Some("stuck:x:3".to_string()))
        );
        assert_eq!(
            finish_to_session_status(&FinishReason::ProviderStop("boom".into())),
            (
                SessionStatus::Crashed,
                Some("provider_stop:boom".to_string())
            )
        );
    }

    // --- exit_for_finish: the CLI exit-code contract ---

    #[test]
    fn exit_for_finish_success_returns_ok() {
        assert!(exit_for_finish(&FinishReason::NoMoreToolCalls, 7).is_ok());
    }

    #[test]
    fn exit_for_finish_cancelled_returns_ok() {
        // Pins the intentional semantics: user-initiated shutdown must NOT
        // propagate as a non-zero exit (which would trigger auto-resume).
        assert!(exit_for_finish(&FinishReason::Cancelled, 3).is_ok());
    }

    #[test]
    fn exit_for_finish_budget_exceeded_errors() {
        let err = exit_for_finish(&FinishReason::BudgetExceeded, 42).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("step budget"), "unexpected: {msg}");
        assert!(msg.contains("42"), "missing step count: {msg}");
    }

    #[test]
    fn exit_for_finish_stuck_errors() {
        let err = exit_for_finish(
            &FinishReason::Stuck {
                repeated_call: "Read".into(),
                repeats: 3,
            },
            9,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("stuck"), "unexpected: {msg}");
        assert!(msg.contains("Read"), "missing tool name: {msg}");
        assert!(msg.contains("3"), "missing repeat count: {msg}");
        // Issue #245 batch 5: the `_` arm's "unknown reason: stuck:Read:3"
        // also satisfies every `contains` above, so only the full message
        // proves the dedicated arm produced it.
        assert_eq!(msg, "agent stuck: repeated tool call 'Read' (3x)");
    }

    #[test]
    fn exit_for_finish_wallclock_errors() {
        let err = exit_for_finish(&FinishReason::WallClockExceeded { secs: 600 }, 5).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("wall-clock"), "unexpected: {msg}");
        assert!(msg.contains("600"), "missing timeout: {msg}");
    }

    #[test]
    fn exit_for_finish_transcript_limit_errors() {
        let err = exit_for_finish(
            &FinishReason::TranscriptLimit {
                chars: 100_000,
                limit: 80_000,
            },
            4,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("transcript"), "unexpected: {msg}");
        assert!(msg.contains("100000"), "missing size: {msg}");
        assert!(msg.contains("80000"), "missing limit: {msg}");
        // Issue #245 batch 5: the `_` arm renders
        // "unknown reason: transcript_limit:100000/80000" — same substrings.
        assert_eq!(
            msg,
            "transcript size 100000 exceeded hard limit 80000 and could not be reduced"
        );
    }

    #[test]
    fn exit_for_finish_permission_denial_limit_errors() {
        let err = exit_for_finish(&FinishReason::PermissionDenialLimit, 6).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("permission"), "unexpected: {msg}");
        // Issue #245 batch 5: the `_` arm renders
        // "unknown reason: permission_denial_limit" — also contains "permission".
        assert_eq!(
            msg,
            "agent hit permission denial limit (loop of denied tool calls)"
        );
    }

    #[test]
    fn exit_for_finish_provider_stop_stop_is_ok() {
        assert!(exit_for_finish(&FinishReason::ProviderStop("stop".into()), 8).is_ok());
    }

    #[test]
    fn exit_for_finish_provider_stop_end_turn_is_ok() {
        assert!(exit_for_finish(&FinishReason::ProviderStop("end_turn".into()), 8).is_ok());
    }

    #[test]
    fn exit_for_finish_provider_stop_empty_is_ok() {
        assert!(exit_for_finish(&FinishReason::ProviderStop(String::new()), 8).is_ok());
    }

    #[test]
    fn exit_for_finish_provider_stop_error_fails() {
        let err =
            exit_for_finish(&FinishReason::ProviderStop("rate_limited".into()), 2).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("provider stopped"), "unexpected: {msg}");
        assert!(msg.contains("rate_limited"), "missing reason: {msg}");
    }

    // ── Issue #119: delegated-worker events on the human CLI stream ──────────

    #[test]
    fn format_worker_event_renders_usage_with_worker_id() {
        let line = format_worker_event(
            "coder",
            &AgentEvent::Usage {
                input_tokens: 123,
                output_tokens: 45,
                cache_hit_tokens: 0,
                cache_miss_tokens: 123,
                step: 2,
            },
        );
        assert_eq!(
            line.as_deref(),
            Some("[worker coder] tokens: prompt=123 completion=45")
        );
    }

    #[test]
    fn format_worker_event_renders_text_tool_call_and_turn_finish() {
        assert_eq!(
            format_worker_event(
                "planner",
                &AgentEvent::AssistantText {
                    text: "on it".into(),
                    step: 0,
                }
            )
            .as_deref(),
            Some("[worker planner] on it")
        );
        assert_eq!(
            format_worker_event(
                "planner",
                &AgentEvent::ToolCall {
                    name: "Read".into(),
                    id: "c1".into(),
                    arguments: "{}".into(),
                    step: 0,
                }
            )
            .as_deref(),
            Some("[worker planner] -> Read")
        );
        assert_eq!(
            format_worker_event(
                "planner",
                &AgentEvent::TurnFinished {
                    reason: "no_more_tool_calls".into(),
                    steps: 3,
                }
            )
            .as_deref(),
            Some("[worker planner] done after 3 steps (reason: no_more_tool_calls)")
        );
    }

    #[test]
    fn format_worker_event_drops_internal_and_blank_events() {
        // Internal worker events must stay out of the parent's stream.
        assert_eq!(
            format_worker_event(
                "w",
                &AgentEvent::Compacted {
                    removed: 1,
                    kept: 2,
                    summary_chars: 3,
                    step: 0,
                }
            ),
            None
        );
        // Blank assistant text is not worth a line.
        assert_eq!(
            format_worker_event(
                "w",
                &AgentEvent::AssistantText {
                    text: "   ".into(),
                    step: 0,
                }
            ),
            None
        );
    }

    #[test]
    fn drain_events_stays_pending_until_the_last_sender_closes() {
        // Issue #245 batch 4: a body replaced with `()` still drops `rx`, so
        // the only observable difference is *when* it finishes — the real
        // drain keeps polling while any sender is alive. (In-process is safe
        // here: no `println!`, so libtest's capture is irrelevant.)
        use std::time::Duration;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let (tx, rx) = mpsc::unbounded_channel();
            let mut task = tokio::spawn(drain_events(rx));
            tx.send(AgentEvent::AssistantText {
                text: "queued".into(),
                step: 0,
            })
            .unwrap();
            let finished_early = tokio::time::timeout(Duration::from_millis(100), &mut task)
                .await
                .is_ok();
            assert!(
                !finished_early,
                "drain must keep consuming while a sender is alive"
            );
            drop(tx);
            tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .expect("drain exits once the channel closes")
                .expect("drain task");
        });
    }

    #[test]
    fn finish_without_result_awaits_the_background_drain() {
        // Issue #245 batch 5: a body replaced with `()` returns before the
        // owned task has drained. Observable the same way as the drain pin
        // above — while a sender is alive the real future must stay pending.
        use std::time::Duration;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let (tx, rx) = mpsc::unbounded_channel();
            let task = JsonEventTask::Legacy(tokio::spawn(stream_events_json(rx)));
            let mut fut = Box::pin(task.finish_without_result());
            let finished_early = tokio::time::timeout(Duration::from_millis(100), fut.as_mut())
                .await
                .is_ok();
            assert!(
                !finished_early,
                "finish_without_result must await the drain while a sender is alive"
            );
            drop(tx);
            tokio::time::timeout(Duration::from_secs(5), fut.as_mut())
                .await
                .expect("finish_without_result returns once the channel closes");
        });
    }

    // ── Issue #245 batch 5: the stderr printers ──────────────────────────────
    //
    // `print_usage` / `print_finish_note` only `eprintln!`. libtest's capture
    // on this toolchain is process-wide (batch-4 probe: output from worker
    // threads is captured too, so redirecting fd 2 sees an empty pipe), which
    // leaves one way to observe them: re-run this very test binary with
    // `--exact <child> --nocapture`. The harness chatter goes to stdout, the
    // child's `eprintln!` lands on fd 2, and the parent reads it back through
    // `Command::output()`. The `*_child_*` tests below are those children —
    // plain `#[test]`s that are harmless when the whole suite runs them.

    const UNPRICED_MODEL: &str = "mutants-245-unpriced-model";

    /// Re-run this test binary on one `*_child_*` test with capture off and
    /// return its stderr. Insists the child ran exactly that one test green:
    /// a name that matches nothing ("running 0 tests") or a child that panics
    /// under a mutant must never pass as "printed nothing".
    fn child_stderr(leaf: &str) -> String {
        // `module_path!()` keeps the crate root (`recursive::cli::…`); libtest
        // names tests from the crate root's children (`cli::…`).
        let module = module_path!();
        let module = module.split_once("::").map_or(module, |(_, rest)| rest);
        let name = format!("{module}::{leaf}");
        let exe = std::env::current_exe().expect("current test binary");
        // Pin the user data dir so `pricing_for` cannot see a developer's
        // `providers.d/` — the model name is unpriced in the bundled catalog.
        let home = tempfile::tempdir().expect("tempdir");
        let out = std::process::Command::new(exe)
            .args(["--exact", &name, "--nocapture"])
            .env("RECURSIVE_HOME", home.path())
            .output()
            .expect("spawn child test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8(out.stderr).expect("child stderr is utf-8");
        assert!(
            out.status.success() && stdout.contains("1 passed; 0 failed"),
            "child {name} did not run exactly one green test: status={:?}\n\
             --stdout--\n{stdout}\n--stderr--\n{stderr}",
            out.status
        );
        stderr
    }

    #[test]
    fn usage_child_full_positive() {
        print_usage(
            TokenUsage {
                prompt_tokens: 60,
                completion_tokens: 40,
                total_tokens: 100,
                cache_hit_tokens: 30,
                cache_miss_tokens: 70,
                reasoning_tokens: 0,
            },
            UNPRICED_MODEL,
            1000,
            4,
        );
    }

    #[test]
    fn usage_child_all_zero() {
        print_usage(TokenUsage::default(), UNPRICED_MODEL, 0, 5);
    }

    #[test]
    fn usage_child_tokens_without_cache_or_latency() {
        print_usage(
            TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            },
            UNPRICED_MODEL,
            0,
            5,
        );
    }

    #[test]
    fn finish_note_child_transcript_limit_then_cancelled() {
        print_finish_note(&FinishReason::TranscriptLimit {
            chars: 123_456,
            limit: 100_000,
        });
        print_finish_note(&FinishReason::Cancelled);
    }

    #[test]
    fn print_usage_full_positive_lines_pinned_via_child_process() {
        let stderr = child_stderr("usage_child_full_positive");
        // hit=30 miss=70 pins the rate arithmetic (30/100*100 = 30.0; `-`
        // underflows or wraps, `*`/`%`/`+`/`/` all land elsewhere) and
        // 1000ms over 4 steps pins the integer average.
        let lines: Vec<&str> = stderr.lines().collect();
        assert_eq!(
            lines,
            vec![
                "tokens: prompt=60 completion=40 total=100",
                "cache: hit=30 miss=70 (30.0% hit rate)",
                "llm latency: total=1000ms avg=250ms over 4 steps",
            ],
            "stderr: {stderr}"
        );
    }

    #[test]
    fn print_usage_zero_usage_prints_nothing_pinned_via_child_process() {
        let stderr = child_stderr("usage_child_all_zero");
        // total_tokens == 0 skips the token block; latency 0 with steps > 0
        // skips the latency line — neither guard may degrade to `>= 0`.
        assert!(
            stderr.trim().is_empty(),
            "zero usage must print nothing, got: {stderr:?}"
        );
    }

    #[test]
    fn print_usage_tokens_only_pinned_via_child_process() {
        let stderr = child_stderr("usage_child_tokens_without_cache_or_latency");
        // cache_hit_tokens == 0 must not produce a "0.0% hit rate" line.
        let lines: Vec<&str> = stderr.lines().collect();
        assert_eq!(
            lines,
            vec!["tokens: prompt=10 completion=5 total=15"],
            "stderr: {stderr}"
        );
    }

    #[test]
    fn print_usage_zero_steps_never_divides() {
        // latency > 0 with steps == 0 cannot happen in a run, but the guard
        // is what keeps `total / steps` unreachable: `&&`→`||` or
        // `steps > 0`→`steps >= 0` divides by zero right here. In-process is
        // fine — the pin is "returns", not what it prints.
        print_usage(TokenUsage::default(), UNPRICED_MODEL, 100, 0);
    }

    #[test]
    fn print_finish_note_lines_pinned_via_child_process() {
        let stderr = child_stderr("finish_note_child_transcript_limit_then_cancelled");
        let lines: Vec<&str> = stderr.lines().collect();
        assert_eq!(
            lines,
            vec![
                "note: stopped because transcript reached 123456 chars (limit 100000)",
                "shutdown: agent stopped at next step boundary after signal",
            ],
            "stderr: {stderr}"
        );
    }

    // ── #245 tail segment: surface pins for print_usage / print_finish_note
    // and exact-error pins for exit_for_finish. The print_* pins observe
    // stderr through a child process: the test binary re-invokes itself
    // with OUTPUT_PIN_CHILD naming a case; in the child that same test
    // returns right after making the real call, and `--nocapture` (which
    // also skips libtest's spawn-hook capture inheritance) lets the call's
    // eprintln! reach the real stderr, which the parent captures.
    // In-process capture is not possible: libtest routes print macros of
    // every thread into a sink the test cannot read.

    const OUTPUT_PIN_CHILD: &str = "RECURSIVE_OUTPUT_PIN_CHILD";

    fn output_pin_child_case() -> Option<String> {
        std::env::var(OUTPUT_PIN_CHILD).ok()
    }

    /// Run this test binary filtered to `test_name` with
    /// `OUTPUT_PIN_CHILD=case`; returns (success, stderr).
    fn run_output_pin_child(case: &str, test_name: &str) -> (bool, String) {
        let exe = std::env::current_exe().expect("current test executable");
        let out = std::process::Command::new(exe)
            .arg(format!("cli::output::tests::{test_name}"))
            .arg("--exact")
            .arg("--nocapture")
            .env_clear()
            .env(OUTPUT_PIN_CHILD, case)
            .output()
            .expect("spawn output-pin child");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    fn print_usage_renders_tokens_cache_rate_and_latency_lines() {
        const NAME: &str = "print_usage_renders_tokens_cache_rate_and_latency_lines";
        if output_pin_child_case().as_deref() == Some("usage-full") {
            print_usage(
                TokenUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cache_hit_tokens: 8,
                    cache_miss_tokens: 2,
                    reasoning_tokens: 0,
                },
                "no-such-model-245",
                1_000,
                4,
            );
            return;
        }
        let (ok, stderr) = run_output_pin_child("usage-full", NAME);
        assert!(ok, "child failed: {stderr}");
        assert!(
            stderr.contains("tokens: prompt=100 completion=50 total=150"),
            "stderr: {stderr}"
        );
        assert!(
            stderr.contains("cache: hit=8 miss=2 (80.0% hit rate)"),
            "stderr: {stderr}"
        );
        assert!(
            stderr.contains("llm latency: total=1000ms avg=250ms over 4 steps"),
            "stderr: {stderr}"
        );
        // The model is not in the pricing table, so there is no cost line.
        assert!(!stderr.contains("cost:"), "stderr: {stderr}");
    }

    #[test]
    fn print_usage_stays_silent_outside_its_gates() {
        const NAME: &str = "print_usage_stays_silent_outside_its_gates";
        let zero_usage = TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
            reasoning_tokens: 0,
        };
        let no_cache = TokenUsage {
            prompt_tokens: 6,
            completion_tokens: 4,
            total_tokens: 10,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
            reasoning_tokens: 0,
        };
        match output_pin_child_case() {
            None => {
                for case in ["all-zero", "zero-latency", "zero-steps", "no-cache-hits"] {
                    let (ok, stderr) = run_output_pin_child(case, NAME);
                    assert!(ok, "case {case}: child failed: {stderr}");
                    match case {
                        // Zero traffic and zero latency: no block at all.
                        "all-zero" => {
                            assert!(!stderr.contains("tokens: prompt="), "stderr: {stderr}");
                            assert!(!stderr.contains("cache: hit="), "stderr: {stderr}");
                            assert!(!stderr.contains("llm latency:"), "stderr: {stderr}");
                        }
                        // The latency gate is on the latency value, not on
                        // the step count: zero latency with steps
                        // outstanding stays silent.
                        "zero-latency" => {
                            assert!(!stderr.contains("llm latency:"), "stderr: {stderr}");
                        }
                        // steps == 0 with real latency stays silent and
                        // must not panic on the average division.
                        "zero-steps" => {
                            assert!(!stderr.contains("llm latency:"), "stderr: {stderr}");
                        }
                        // The cache block is gated on cache_hit_tokens, not
                        // on total tokens: traffic without cache hits
                        // renders no cache line.
                        "no-cache-hits" => {
                            assert!(
                                stderr.contains("tokens: prompt=6 completion=4 total=10"),
                                "stderr: {stderr}"
                            );
                            assert!(
                                stderr.contains("llm latency: total=100ms avg=50ms over 2 steps"),
                                "stderr: {stderr}"
                            );
                            assert!(!stderr.contains("cache: hit="), "stderr: {stderr}");
                        }
                        _ => unreachable!(),
                    }
                }
            }
            Some(case) => match case.as_str() {
                "all-zero" => print_usage(zero_usage, "m", 0, 0),
                "zero-latency" => print_usage(zero_usage, "m", 0, 3),
                "zero-steps" => print_usage(zero_usage, "m", 500, 0),
                "no-cache-hits" => print_usage(no_cache, "m", 100, 2),
                _ => unreachable!("unknown case {case}"),
            },
        }
    }

    #[test]
    fn print_finish_note_renders_transcript_limit_and_cancelled_notes() {
        const NAME: &str = "print_finish_note_renders_transcript_limit_and_cancelled_notes";
        match output_pin_child_case() {
            None => {
                for case in ["transcript-limit", "cancelled", "other-silent"] {
                    let (ok, stderr) = run_output_pin_child(case, NAME);
                    assert!(ok, "case {case}: child failed: {stderr}");
                    match case {
                        "transcript-limit" => assert!(
                            stderr.contains(
                                "note: stopped because transcript reached 1234 chars (limit 500)"
                            ),
                            "stderr: {stderr}"
                        ),
                        "cancelled" => assert!(
                            stderr.contains(
                                "shutdown: agent stopped at next step boundary after signal"
                            ),
                            "stderr: {stderr}"
                        ),
                        // Any other finish reason is silent.
                        "other-silent" => {
                            assert!(!stderr.contains("note:"), "stderr: {stderr}");
                            assert!(!stderr.contains("shutdown:"), "stderr: {stderr}");
                        }
                        _ => unreachable!(),
                    }
                }
            }
            Some(case) => match case.as_str() {
                "transcript-limit" => print_finish_note(&FinishReason::TranscriptLimit {
                    chars: 1234,
                    limit: 500,
                }),
                "cancelled" => print_finish_note(&FinishReason::Cancelled),
                "other-silent" => print_finish_note(&FinishReason::NoMoreToolCalls),
                _ => unreachable!("unknown case {case}"),
            },
        }
    }

    #[test]
    fn exit_for_finish_maps_reasons_to_exact_errors_and_successes() {
        // Byte-exact strings: the `delete match arm` mutants for Stuck /
        // TranscriptLimit / PermissionDenialLimit fall through to the
        // `#[non_exhaustive]` wildcard, whose "agent finished with unknown
        // reason: …" text differs — these assertions are what kills them.
        let err = exit_for_finish(
            &FinishReason::Stuck {
                repeated_call: "Bash".into(),
                repeats: 3,
            },
            12,
        )
        .expect_err("stuck must be an error");
        assert_eq!(
            err.to_string(),
            "agent stuck: repeated tool call 'Bash' (3x)"
        );

        let err = exit_for_finish(
            &FinishReason::TranscriptLimit {
                chars: 1234,
                limit: 500,
            },
            12,
        )
        .expect_err("transcript limit must be an error");
        assert_eq!(
            err.to_string(),
            "transcript size 1234 exceeded hard limit 500 and could not be reduced"
        );

        let err = exit_for_finish(&FinishReason::PermissionDenialLimit, 12)
            .expect_err("permission denial limit must be an error");
        assert_eq!(
            err.to_string(),
            "agent hit permission denial limit (loop of denied tool calls)"
        );

        // Success reasons stay Ok — Cancelled must never auto-resume.
        assert!(exit_for_finish(&FinishReason::NoMoreToolCalls, 1).is_ok());
        assert!(exit_for_finish(&FinishReason::Cancelled, 1).is_ok());
        assert!(exit_for_finish(&FinishReason::ProviderStop("end_turn".into()), 1).is_ok());
        // The remaining error arms keep their exact shape too.
        assert_eq!(
            exit_for_finish(&FinishReason::BudgetExceeded, 7)
                .unwrap_err()
                .to_string(),
            "agent exceeded step budget (7)"
        );
        assert_eq!(
            exit_for_finish(&FinishReason::WallClockExceeded { secs: 9 }, 7)
                .unwrap_err()
                .to_string(),
            "agent exceeded wall-clock timeout (9s)"
        );
        assert_eq!(
            exit_for_finish(&FinishReason::ProviderStop("rate_limited".into()), 7)
                .unwrap_err()
                .to_string(),
            "provider stopped: rate_limited"
        );
    }
}
