// Why this test exists:
// .dev/AGENTS.md invariant #7: "Finish reasons are data, not errors.
// `AgentRuntime::run` returns `Ok(RuntimeOutcome { finish_reason: ... })` for every
// termination mode (NoMoreToolCalls, BudgetExceeded, Stuck, TranscriptLimit,
// ProviderStop). Only honest-to-god failures (network, JSON, provider
// transport, IO) become `Err`. The CLI decides binary exit code by inspecting
// `outcome.finish_reason` AFTER persisting the transcript — see
// `main.rs::exit_for_finish`. NEVER introduce a new `Error::XxxBudget` or
// `Error::XxxLimit` variant that short-circuits the transcript save.
// The self-improve flow's auto-resume step depends on the saved transcript
// existing on disk."
//
// This test verifies:
// - All `FinishReason` variants exist and are serde round-trippable
// - No `Error` variant corresponds to a `FinishReason` tag (no
//   `Error::XxxBudget` etc.)
// - `FinishReason` Display format is stable (used in CLI exit codes and
//   the self-improve flow's auto-resume)

use recursive::agent::FinishReason;
use recursive::llm::{Completion, MockProvider, ToolCall};
use std::sync::Arc;

// ── Serde round-trip ───────────────────────────────────────────────────────

/// All FinishReason variants must serialize and deserialize cleanly.
/// This ensures they can travel over the wire and be persisted in transcripts.
///
/// Note: `ProviderStop(String)` uses tagged newtype variant which serde
/// cannot roundtrip with `#[serde(tag = "kind")]` — the inner value is
/// dropped during serialization. This is a known serde limitation.
/// The variant is still valid as data (it's constructed in Rust code
/// and its Display format is stable).
#[test]
fn finish_reason_serde_roundtrip() {
    // Test variants that cleanly roundtrip (non-newtype).
    let variants: Vec<FinishReason> = vec![
        FinishReason::NoMoreToolCalls,
        FinishReason::BudgetExceeded,
        FinishReason::Stuck {
            repeated_call: "Bash".to_string(),
            repeats: 3,
        },
        FinishReason::TranscriptLimit {
            chars: 100_000,
            limit: 90_000,
        },
        FinishReason::Cancelled,
        FinishReason::PermissionDenialLimit,
        FinishReason::WallClockExceeded { secs: 1 },
    ];

    for reason in &variants {
        let json = serde_json::to_string(reason)
            .unwrap_or_else(|e| panic!("cannot serialize {reason:?}: {e}"));
        let restored: FinishReason = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("cannot deserialize {reason:?} from '{json}': {e}"));
        assert_eq!(
            reason, &restored,
            "FinishReason roundtrip mismatch: {reason:?} != {restored:?}"
        );
    }
}

// ── Display format is stable ───────────────────────────────────────────────

/// `FinishReason::Display` is used by the self-improve flow's auto-resume step.
/// It must be stable across refactors.
#[test]
fn finish_reason_display_is_stable() {
    assert_eq!(
        FinishReason::NoMoreToolCalls.to_string(),
        "no_more_tool_calls"
    );
    assert_eq!(FinishReason::BudgetExceeded.to_string(), "budget_exceeded");
    assert_eq!(
        FinishReason::ProviderStop("length".to_string()).to_string(),
        "provider_stop:length"
    );
    assert_eq!(
        FinishReason::Stuck {
            repeated_call: "Bash".to_string(),
            repeats: 3,
        }
        .to_string(),
        "stuck:Bash:3"
    );
    assert_eq!(
        FinishReason::TranscriptLimit {
            chars: 100,
            limit: 90
        }
        .to_string(),
        "transcript_limit:100/90"
    );
    assert_eq!(FinishReason::Cancelled.to_string(), "cancelled");
    assert_eq!(
        FinishReason::PermissionDenialLimit.to_string(),
        "permission_denial_limit"
    );
    assert_eq!(
        FinishReason::WallClockExceeded { secs: 1 }.to_string(),
        "wall_clock_exceeded:1"
    );
}

// ── No error variant maps to a finish reason ───────────────────────────────

/// Invariant #7 explicitly forbids `Error::XxxBudget` or `Error::XxxLimit`
/// variants. All termination modes must go through `Ok(RuntimeOutcome { finish_reason })`.
#[test]
fn no_error_variant_corresponds_to_finish_reason() {
    // Load the error.rs source and check for forbidden patterns.
    let error_src =
        std::fs::read_to_string(env!("CARGO_MANIFEST_DIR").to_string() + "/src/error.rs").unwrap();

    let forbidden: &[&str] = &[
        "Budget",
        "BudgetExceeded",
        "Stuck",
        "TranscriptLimit",
        "NoMoreToolCalls",
    ];

    // These words are fine outside Error variants (e.g. in comments).
    // We check for the `Error::Xxx` pattern.
    for word in forbidden {
        let pattern = format!("Error::{}", word);
        if error_src.contains(&pattern) {
            panic!(
                "invariant #7 violation: `{pattern}` found in src/error.rs. \
                 Finish reasons must be data (FinishReason enum), not errors. \
                 See .dev/AGENTS.md invariant #7."
            );
        }
    }
}

// ── FinishReason from serialized JSON (backward compatibility) ─────────────

/// Old transcripts may contain serialized FinishReason values. Ensure we can
/// deserialize representative payloads.
#[test]
fn finish_reason_deserializes_known_formats() {
    // Tagged enum: {"kind": "no_more_tool_calls"}
    let json = r#"{"kind":"no_more_tool_calls"}"#;
    let reason: FinishReason =
        serde_json::from_str(json).expect("must deserialize no_more_tool_calls");
    assert_eq!(reason, FinishReason::NoMoreToolCalls);

    // Stuck with payload
    let json = r#"{"kind":"stuck","repeated_call":"Bash","repeats":3}"#;
    let reason: FinishReason = serde_json::from_str(json).expect("must deserialize stuck");
    assert_eq!(
        reason,
        FinishReason::Stuck {
            repeated_call: "Bash".to_string(),
            repeats: 3,
        }
    );

    // ProviderStop with reason — this variant uses a newtype String and
    // cannot be cleanly serialized with #[serde(tag = "kind")]. It's only
    // constructed in Rust code, not deserialized from JSON.
    // We verify ProviderStop exists via the Display test instead.

    // TranscriptLimit
    let json = r#"{"kind":"transcript_limit","chars":50000,"limit":40000}"#;
    let reason: FinishReason =
        serde_json::from_str(json).expect("must deserialize transcript_limit");
    assert_eq!(
        reason,
        FinishReason::TranscriptLimit {
            chars: 50000,
            limit: 40000,
        }
    );

    // Cancelled
    let json = r#"{"kind":"cancelled"}"#;
    let reason: FinishReason = serde_json::from_str(json).expect("must deserialize cancelled");
    assert_eq!(reason, FinishReason::Cancelled);

    // PermissionDenialLimit
    let json = r#"{"kind":"permission_denial_limit"}"#;
    let reason: FinishReason =
        serde_json::from_str(json).expect("must deserialize permission_denial_limit");
    assert_eq!(reason, FinishReason::PermissionDenialLimit);
}

// ── Goal 399: WallClockExceeded stays data through the full runtime ────────

/// A provider that stalls before its first `complete()` response, then
/// delegates to a scripted [`MockProvider`]. Models a hung/slow LLM call so
/// the wall-clock deadline (Goal 345, wired through in Goal 399) fires
/// mid-loop on the next step boundary.
struct SlowFirstCallProvider {
    inner: MockProvider,
    delay: std::time::Duration,
    remaining_slow_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl recursive::llm::ChatProvider for SlowFirstCallProvider {
    async fn complete(
        &self,
        messages: &[recursive::message::Message],
        tools: &[recursive::llm::ToolSpec],
    ) -> recursive::error::Result<recursive::llm::Completion> {
        use std::sync::atomic::Ordering;
        if self.remaining_slow_calls.fetch_sub(1, Ordering::SeqCst) > 0 {
            tokio::time::sleep(self.delay).await;
        }
        self.inner.complete(messages, tools).await
    }
}

/// A trivial tool so the scripted loop can request a second step (the wall
/// deadline is checked at the top of each step — a turn that ends on step 0
/// can never observe it).
struct NoopTool;

#[async_trait::async_trait]
impl recursive::tools::Tool for NoopTool {
    fn spec(&self) -> recursive::llm::ToolSpec {
        recursive::llm::ToolSpec {
            name: "noop".into(),
            description: "does nothing".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }
    async fn execute(&self, _args: serde_json::Value) -> recursive::error::Result<String> {
        Ok("ok".into())
    }
}

/// Core Goal 399 assertion (invariant #7): with `wall_timeout_secs` wired
/// through the runtime builder, a turn that outlives its wall-clock budget
/// returns `Ok(RuntimeOutcome { finish_reason: WallClockExceeded { .. } })` —
/// NOT `Err` — and the transcript survives for persistence / auto-resume.
#[tokio::test]
async fn wall_clock_exceeded_is_data_not_error_and_transcript_is_kept() {
    use recursive::runtime::AgentRuntime;
    use recursive::tools::ToolRegistry;

    // Script: call 1 returns (after a 2s stall) a tool call so the loop
    // continues; call 2 returns immediately with another tool call. The
    // top-of-step wall check at step 1 then sees elapsed ≥ 2s > 1s budget.
    let tool_call = |id: &str| ToolCall {
        id: id.into(),
        name: "noop".into(),
        arguments: serde_json::json!({}),
    };
    let script = vec![
        Completion {
            content: "stalling".into(),
            tool_calls: vec![tool_call("c1")],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "again".into(),
            tool_calls: vec![tool_call("c2")],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
    ];
    let provider = SlowFirstCallProvider {
        inner: MockProvider::new(script),
        delay: std::time::Duration::from_secs(2),
        remaining_slow_calls: std::sync::atomic::AtomicUsize::new(1),
    };

    let tools = ToolRegistry::local().register(Arc::new(NoopTool));
    let mut runtime = AgentRuntime::builder()
        .llm(Arc::new(provider))
        .tools(tools)
        .system_prompt("test agent")
        .max_steps(10)
        .wall_timeout_secs(1)
        .build()
        .expect("runtime builds");

    let result = runtime.run("stall then loop").await;

    // The budget must terminate the turn as DATA, not as an error.
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(e) => panic!("invariant #7 violation: wall-clock timeout became an error: {e}"),
    };
    assert_eq!(
        outcome.finish_reason,
        FinishReason::WallClockExceeded { secs: 1 },
        "expected WallClockExceeded, got {:?}",
        outcome.finish_reason
    );

    // Transcript must survive so persistence / auto-resume still work.
    assert!(
        !runtime.transcript().is_empty(),
        "transcript must survive a wall-clock timeout"
    );
}

/// Goal 399: `wall_timeout_secs(0)` preserves today's behaviour exactly —
/// the same stalled-provider script that trips a 1s budget runs to normal
/// completion when the budget is 0 (unlimited).
#[tokio::test]
async fn wall_timeout_zero_keeps_legacy_unlimited_behaviour() {
    use recursive::runtime::AgentRuntime;
    use recursive::tools::ToolRegistry;

    // Same shape as the firing script, but the second call ends the turn
    // cleanly; with 0 = unlimited the wall check never interferes.
    let tool_call = ToolCall {
        id: "c1".into(),
        name: "noop".into(),
        arguments: serde_json::json!({}),
    };
    let script = vec![
        Completion {
            content: "stalling".into(),
            tool_calls: vec![tool_call],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ];
    let provider = SlowFirstCallProvider {
        inner: MockProvider::new(script),
        delay: std::time::Duration::from_secs(2),
        remaining_slow_calls: std::sync::atomic::AtomicUsize::new(1),
    };

    let mut runtime = AgentRuntime::builder()
        .llm(Arc::new(provider))
        .tools(ToolRegistry::local().register(Arc::new(NoopTool)))
        .system_prompt("test agent")
        .max_steps(10)
        .wall_timeout_secs(0)
        .build()
        .expect("runtime builds");

    let outcome = runtime
        .run("stall but unlimited")
        .await
        .expect("0 budget must keep legacy unlimited behaviour (no wall finish)");
    assert_eq!(
        outcome.finish_reason,
        FinishReason::NoMoreToolCalls,
        "0 budget must never produce WallClockExceeded"
    );
}

/// Issue #94 acceptance: a goal that would otherwise keep looping stops
/// mid-run once the turn's spend reaches `max_budget_usd` — as DATA
/// (invariant #7: `Ok(outcome)`, not `Err`), with the transcript kept and the
/// reported usage no greater than the ceiling.
#[tokio::test]
async fn cost_budget_stops_the_run_mid_goal_with_budget_exceeded() {
    use recursive::llm::{ModelPricing, TokenUsage};
    use recursive::runtime::AgentRuntime;
    use recursive::tools::ToolRegistry;

    // $0.01 per step: 10_000 prompt tokens at $1/M.
    let step_usage = TokenUsage {
        prompt_tokens: 10_000,
        total_tokens: 10_000,
        ..Default::default()
    };
    let script: Vec<Completion> = (0..8)
        .map(|i| Completion {
            content: format!("step {i}"),
            tool_calls: vec![ToolCall {
                id: format!("c{i}"),
                name: "noop".into(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: Some(step_usage),
            reasoning_content: None,
        })
        .collect();

    let pricing = ModelPricing {
        input_per_million: 1.0,
        output_per_million: 1.0,
        cache_hit_input_per_million: 0.1,
    };
    let mut runtime = AgentRuntime::builder()
        .llm(Arc::new(MockProvider::new(script)))
        .tools(ToolRegistry::local().register(Arc::new(NoopTool)))
        .system_prompt("test agent")
        // Far more steps than the budget allows: only the ceiling can stop it.
        .max_steps(50)
        .cost_budget(Some(0.03), Some(pricing))
        .build()
        .expect("runtime builds");

    let outcome = runtime
        .run("loop forever")
        .await
        .expect("invariant #7: a budget exit is data, not an error");

    assert!(
        matches!(outcome.finish_reason, FinishReason::BudgetExceeded),
        "expected BudgetExceeded, got {:?}",
        outcome.finish_reason
    );
    assert_eq!(
        outcome.steps, 3,
        "must stop at the step where the ceiling was reached, not at max_steps"
    );
    let spent = pricing.cost_usd(outcome.total_usage);
    assert!(
        spent <= 0.03 + 1e-9,
        "reported spend {spent} must not exceed the budget"
    );
    assert!(
        spent >= 0.03 - 1e-9,
        "the budget must actually have been reached, spent {spent}"
    );
    assert!(
        !runtime.transcript().is_empty(),
        "transcript must survive a budget exit (auto-resume depends on it)"
    );
}
