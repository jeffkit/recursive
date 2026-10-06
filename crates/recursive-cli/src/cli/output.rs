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

pub(crate) fn finalize_session_writer(
    session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>>,
    status: SessionStatus,
    finish_reason: Option<String>,
    error: Option<String>,
) {
    let Some(sw) = session_writer else { return };
    match Arc::into_inner(sw) {
        Some(mutex) => match mutex.lock() {
            Ok(mut w) => {
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
            Err(e) => eprintln!("session: failed to lock writer: {e}"),
        },
        None => eprintln!("session: writer still has other references; cannot finalize"),
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
    }

    #[test]
    fn exit_for_finish_permission_denial_limit_errors() {
        let err = exit_for_finish(&FinishReason::PermissionDenialLimit, 6).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("permission"), "unexpected: {msg}");
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
}
