// Why this test exists:
// .dev/AGENTS.md invariant #1: "Agent loop stays small. New capabilities are
// tools or providers, not branches inside `agent.rs::Agent::run`."
// .dev/AGENTS.md invariant #2: "Orthogonality. Tools must not depend on LLM
// internals; providers must not depend on tools."
//
// After Goal 219's refactor, the agent loop lives in `kernel.rs` (core loop)
// and `runtime.rs` (orchestration). These should stay lean — new capabilities
// are added as tools/providers, not as branches inside the loop.

use std::path::PathBuf;

/// Returns the workspace root by walking up from the current file's directory.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn src_file(name: &str) -> PathBuf {
    workspace_root().join("src").join(name)
}

// ── Invariant #1: Loop size ────────────────────────────────────────────────

/// The core agent loop (`kernel.rs`) must be under 1000 lines. If this test
/// fails, you are adding branches to the loop instead of splitting work into
/// tools or providers.
///
/// Threshold is generous (~1000 lines) to accommodate the kernel loop logic,
/// run-core orchestration, and structured-output handling.
#[test]
fn kernel_loop_stays_small() {
    let path = src_file("kernel.rs");
    let size = std::fs::metadata(&path)
        .expect("kernel.rs must exist")
        .len();
    let content = std::fs::read_to_string(&path).unwrap();
    let lines = content.lines().count();

    // kernel.rs carries the core dispatch loop. Keep it under 1000 lines.
    assert!(
        lines <= 1000,
        "invariant #1 violation: kernel.rs is {lines} lines (limit: 1000). \
         Move new capabilities into tools/ or llm/ modules, not AgentKernel::run."
    );
    // File size sanity: ~30KB for 1000 lines with modest comments.
    assert!(
        size <= 50_000,
        "invariant #1 violation: kernel.rs is {size} bytes (limit: 50KB)."
    );
}

/// The runtime orchestrator (`runtime.rs`) must be under 3000 lines.
/// This file handles goal loops, multi-agent orchestration, and event dispatch
/// — it should delegate to the kernel for single-turn logic.
#[test]
fn runtime_stays_manageable() {
    let path = src_file("runtime.rs");
    let content = std::fs::read_to_string(&path).expect("runtime.rs must exist");
    let lines = content.lines().count();

    assert!(
        lines <= 3500,
        "invariant #1 violation: runtime.rs is {lines} lines (limit: 3500). \
         Delegate single-turn logic to kernel.rs; add new orchestration modes as \
         separate modules, not branches in AgentRuntime."
    );
}

/// `RunCore::run_inner` is the actual ReAct step loop after Goal 219 moved
/// it out of `Agent::run`. Invariant #1 was updated to point at it. This
/// test pins the loop body's size in lines so future feature additions
/// can't silently inflate it — they must split a helper or move into a
/// tool / provider instead.
///
/// The threshold is 150 lines, just above the post-P1-1 baseline (~117).
/// The original G219 baseline was ~394 lines; the P1-1 split (July 2026)
/// extracted `make_outcome`, `check_shutdown`, `enforce_transcript_budget`,
/// `drain_mailbox`, `handle_no_tool_calls`, `process_tool_results`, and
/// `dispatch_llm_step` as sibling helpers, leaving the loop body as a
/// small linear sequence: drain mailbox → check budget → call LLM →
/// handle no-tool-calls → execute tools → process results.
///
/// When this test fires, the right move is usually to extract another
/// phase of the loop into a sibling helper — NOT to bump the threshold.
#[test]
fn run_inner_function_body_stays_small() {
    let path = src_file("run_core.rs");
    let content = std::fs::read_to_string(&path).expect("run_core.rs must exist");

    // Locate the `pub(crate) async fn run_inner` signature line.
    let sig_line = content
        .lines()
        .position(|l| l.contains("async fn run_inner"))
        .unwrap_or_else(|| panic!("run_core.rs: could not find `async fn run_inner`"));

    // From the signature, walk forward counting brace depth until it
    // returns to zero. The function body is signature_open_brace ..
    // closing_brace (inclusive).
    let lines: Vec<&str> = content.lines().collect();
    let mut depth: i32 = 0;
    let mut end_line: usize = 0;
    for (i, line) in lines.iter().enumerate().skip(sig_line) {
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;
        if depth == 0 {
            end_line = i;
            break;
        }
    }
    assert!(
        end_line != 0,
        "run_core.rs: `run_inner` brace walk did not close — file is malformed"
    );

    let body_lines = end_line - sig_line + 1;
    assert!(
        body_lines <= 150,
        "invariant #1 violation: RunCore::run_inner is {body_lines} lines (limit: 150). \
         Split a phase of the loop into a sibling helper, or move the new capability \
         into a tool / provider. Do NOT bump the threshold."
    );
}

// ── Invariant #2: Orthogonality ────────────────────────────────────────────

/// Tools must not import LLM internals beyond the `ToolSpec` shared type.
/// Specifically, tools must not import `crate::llm::ChatProvider`,
/// `crate::llm::LlmProvider`, `crate::llm::StructuredRequest`, or any
/// provider-specific types.
///
/// `ToolSpec` is allowed because it is the contract between the agent and tools.
#[test]
fn tools_do_not_import_llm_internals() {
    let tools_dir = workspace_root().join("src").join("tools");

    // allowed: ToolSpec is the shared contract type
    let forbidden_patterns: &[&str] = &[
        "use crate::llm::ChatProvider",
        "use crate::llm::LlmProvider",
        "use crate::llm::StructuredRequest",
        "use crate::llm::Completion",
        "use crate::llm::openai",
        "use crate::llm::anthropic",
        "use crate::llm::mock",
        "use crate::llm::chat",
        "use crate::llm::search",
        "use crate::llm::pricing",
    ];

    let mut violations = Vec::new();
    for entry in walkdir::WalkDir::new(&tools_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
    {
        let content = std::fs::read_to_string(entry.path()).unwrap();
        for pattern in forbidden_patterns {
            if content.contains(pattern) {
                violations.push(format!(
                    "{}: imports `{pattern}`",
                    entry.path().strip_prefix(&tools_dir).unwrap().display()
                ));
            }
        }
    }

    if !violations.is_empty() {
        panic!(
            "invariant #2 violation: tools/ modules import LLM internals:\n  - {}\n\
             Tools may only import crate::llm::ToolSpec and crate::llm::ToolCall.",
            violations.join("\n  - ")
        );
    }
}

/// LLM providers must not import from tools/ (they are orthogonal layers).
/// Imports in `#[cfg(test)]` blocks are exempt — test code needs to reference
/// tool constants/types for assertions.
#[test]
fn providers_do_not_import_tools_internals() {
    let llm_dir = workspace_root().join("src").join("llm");

    let mut violations = Vec::new();
    for entry in walkdir::WalkDir::new(&llm_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
    {
        let content = std::fs::read_to_string(entry.path()).unwrap();

        // Track whether we're inside a #[cfg(test)] block
        let mut in_test_module = false;
        let mut brace_depth = 0u32;

        for line in content.lines() {
            let trimmed = line.trim();

            // Detect entering/exiting test module
            if trimmed.starts_with("#[cfg(test)]") {
                in_test_module = true;
                continue;
            }

            if in_test_module {
                // Count braces to track test module scope
                brace_depth += trimmed.matches('{').count() as u32;
                // When we've closed the test module, exit
                if trimmed.contains('}') {
                    let closes = trimmed.matches('}').count() as u32;
                    if closes >= brace_depth {
                        brace_depth = 0;
                        in_test_module = false;
                    } else {
                        brace_depth -= closes;
                    }
                }
                continue;
            }

            if trimmed.starts_with("use crate::tools") && !trimmed.contains("//") {
                violations.push(format!(
                    "{}: imports `{trimmed}`",
                    entry.path().strip_prefix(&llm_dir).unwrap().display()
                ));
            }
        }
    }

    if !violations.is_empty() {
        panic!(
            "invariant #2 violation: llm/ modules import tools internals:\n  - {}\n\
             Providers must not depend on tools.",
            violations.join("\n  - ")
        );
    }
}
