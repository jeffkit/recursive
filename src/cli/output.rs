//! Output helpers: usage printing, transcript saving, cost tracking, event streaming.

use std::path::Path;
use std::sync::Arc;

use recursive::llm::{pricing_for, TokenUsage};
use recursive::{AgentEvent, FinishReason, SessionFile, SessionWriter, TranscriptFile};
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
/// exit code so that self-improve.sh's auto-resume gate fires. The
/// transcript has already been saved by the caller before this is called.
///
/// `Cancelled` is intentionally **not** an error: shutdown via SIGINT
/// or SIGTERM is user-initiated, the saved transcript is intact, and
/// self-improve.sh must NOT auto-resume something the user explicitly
/// stopped. The fall-through `_ => Ok(())` covers it.
pub(crate) fn exit_for_finish(finish: &FinishReason, steps: usize) -> anyhow::Result<()> {
    match finish {
        FinishReason::BudgetExceeded => {
            anyhow::bail!("agent exceeded step budget ({steps})")
        }
        _ => Ok(()),
    }
}

pub(crate) fn finalize_session_writer(
    session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>>,
    status: &str,
) {
    let Some(sw) = session_writer else { return };
    match Arc::into_inner(sw) {
        Some(mutex) => match mutex.lock() {
            Ok(mut w) => {
                if let Err(e) = w.finish(status) {
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
                eprintln!("cost: ${:.4} ({})", t.cost_usd().unwrap_or(0.0), model);
            }
        }
        Err(e) => eprintln!("cost: failed to lock cost tracker: {e}"),
    }
}

pub(crate) async fn stream_events(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
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
pub(crate) async fn stream_events_repl(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::AssistantText { ref text, .. } if !text.trim().is_empty() => {
                println!("{text}");
            }
            AgentEvent::ToolCall { ref name, .. } => {
                eprintln!("  ↳ {name}");
            }
            _ => {}
        }
    }
}

pub(crate) async fn stream_events_json(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        if let Ok(line) = serde_json::to_string(&ev) {
            println!("{line}");
        }
    }
}
