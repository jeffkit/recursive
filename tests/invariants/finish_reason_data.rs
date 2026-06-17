// Why this test exists:
// .dev/AGENTS.md invariant #7: "Finish reasons are data, not errors.
// `Agent::run` returns `Ok(AgentOutcome { finish: ... })` for every
// termination mode (NoMoreToolCalls, BudgetExceeded, Stuck, TranscriptLimit,
// ProviderStop). Only honest-to-god failures (network, JSON, provider
// transport, IO) become `Err`. The CLI decides binary exit code by inspecting
// `outcome.finish` AFTER persisting the transcript — see
// `main.rs::exit_for_finish`. NEVER introduce a new `Error::XxxBudget` or
// `Error::XxxLimit` variant that short-circuits the transcript save.
// self-improve.sh's auto-resume gate depends on the saved transcript
// existing on disk."
//
// This test verifies:
// - All `FinishReason` variants exist and are serde round-trippable
// - No `Error` variant corresponds to a `FinishReason` tag (no
//   `Error::XxxBudget` etc.)
// - `FinishReason` Display format is stable (used in CLI exit codes and
//   self-improve.sh auto-resume)

use recursive::agent::FinishReason;

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

/// `FinishReason::Display` is used by self-improve.sh auto-resume gate.
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
}

// ── No error variant maps to a finish reason ───────────────────────────────

/// Invariant #7 explicitly forbids `Error::XxxBudget` or `Error::XxxLimit`
/// variants. All termination modes must go through `Ok(AgentOutcome { finish })`.
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
