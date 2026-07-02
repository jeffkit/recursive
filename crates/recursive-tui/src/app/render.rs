//! Standalone helper functions for rendering / display.
//!
//! These are pure functions (no `&mut self`) used by event_loop and the
//! terminal UI to format tool arguments, parse diffs, etc.

use serde_json::Value;

use super::{DiffHunk, DiffLine, DiffLineKind, ToolResultData, TranscriptBlock};

// ── Session-resume reconstruction ────────────────────────────────────

/// Rebuild the visible transcript blocks from a loaded session's messages.
///
/// Used by `/resume`: when a previous session is selected, the runtime
/// transcript is replaced *and* the on-screen conversation must be
/// reconstructed so the user sees the resumed dialogue rather than just a
/// "resumed session" note appended to the current chat.
///
/// Mapping:
/// - `System` messages (the system prompt / injected context) are skipped.
/// - `User` / `Assistant` text become their respective blocks.
/// - assistant `reasoning_content` becomes a finalised `Reasoning` block.
/// - assistant `tool_calls` become `ToolCall` blocks; the matching `Tool`
///   message fills the result. Persisted results carry no success flag, so
///   they are shown as succeeded.
pub fn blocks_from_messages(messages: &[recursive::message::Message]) -> Vec<TranscriptBlock> {
    use recursive::message::Role;

    let mut blocks: Vec<TranscriptBlock> = Vec::new();
    for msg in messages {
        match msg.role {
            Role::System => {}
            Role::User => {
                if !msg.content.trim().is_empty() {
                    blocks.push(TranscriptBlock::User {
                        text: msg.content.clone(),
                    });
                }
            }
            Role::Assistant => {
                if let Some(reasoning) = &msg.reasoning_content {
                    if !reasoning.trim().is_empty() {
                        blocks.push(TranscriptBlock::Reasoning {
                            text: reasoning.clone(),
                            streaming: false,
                        });
                    }
                }
                if !msg.content.trim().is_empty() {
                    blocks.push(TranscriptBlock::Assistant {
                        text: msg.content.clone(),
                        streaming: false,
                        latency_ms: None,
                    });
                }
                for tc in &msg.tool_calls {
                    blocks.push(TranscriptBlock::ToolCall {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        args_preview: preview_args(&tc.arguments.to_string()),
                        result: None,
                    });
                }
            }
            Role::Tool => {
                let id = msg.tool_call_id.clone().unwrap_or_default();
                let matched = blocks.iter_mut().rev().find(|b| {
                    matches!(b, TranscriptBlock::ToolCall { id: cid, result: None, .. } if cid == &id)
                });
                if let Some(TranscriptBlock::ToolCall { result, .. }) = matched {
                    *result = Some(ToolResultData {
                        success: true,
                        output: msg.content.clone(),
                        expanded: false,
                    });
                } else {
                    blocks.push(TranscriptBlock::ToolCall {
                        id,
                        name: String::new(),
                        args_preview: String::new(),
                        result: Some(ToolResultData {
                            success: true,
                            output: msg.content.clone(),
                            expanded: false,
                        }),
                    });
                }
            }
        }
    }
    blocks
}

// ── Argument preview ─────────────────────────────────────────────────

/// Produce a short preview of a JSON-encoded arguments string.
///
/// Picks up to two top-level fields, formats them as `k=v`, and clamps
/// to ~60 characters with an ellipsis.
pub fn preview_args(arguments: &str) -> String {
    let parsed: Result<Value, _> = serde_json::from_str(arguments);
    let Ok(Value::Object(map)) = parsed else {
        // Not JSON-y; just clamp the raw string.
        return clamp(arguments, 60);
    };

    let mut parts = Vec::new();
    for (k, v) in map.iter().take(2) {
        let v_str = match v {
            Value::String(s) => format!("\"{}\"", clamp(s, 30)),
            other => clamp(&other.to_string(), 30),
        };
        parts.push(format!("{k}={v_str}"));
    }
    clamp(&parts.join(" "), 60)
}

fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

// ── Spinner verb ─────────────────────────────────────────────────────

/// Pick a spinner verb based on the tool name.
pub fn verb_for_tool(name: &str) -> &'static str {
    match name {
        "Read" | "Grep" | "Glob" => "Reading",
        "Edit" | "Write" => "Editing",
        "Bash" => "Running",
        _ => "Calling tool",
    }
}

// ── V4A patch parser ─────────────────────────────────────────────────

/// Pure parser for a V4A patch string.
pub fn parse_v4a_patch(input: &str) -> Option<(String, Vec<DiffHunk>)> {
    let mut path: Option<String> = None;
    let mut current = Vec::new();
    let mut hunks: Vec<DiffHunk> = Vec::new();

    for line in input.lines() {
        if let Some(rest) = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
        {
            if path.is_some() {
                // Multiple update sections — only the first is used,
                // per goal scope.
                break;
            }
            path = Some(rest.trim().to_string());
            continue;
        }
        if line.starts_with("*** Begin Patch")
            || line.starts_with("*** End Patch")
            || line.starts_with("*** End of File")
        {
            continue;
        }
        if path.is_none() {
            continue;
        }
        // @@ anchor lines start a new hunk.
        if let Some(stripped) = line.strip_prefix("@@") {
            if !current.is_empty() {
                hunks.push(DiffHunk {
                    lines: std::mem::take(&mut current),
                });
            }
            let text = stripped.trim_start().to_string();
            if !text.is_empty() {
                current.push(DiffLine {
                    kind: DiffLineKind::Context,
                    text,
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            current.push(DiffLine {
                kind: DiffLineKind::Add,
                text: rest.to_string(),
            });
        } else if let Some(rest) = line.strip_prefix('-') {
            current.push(DiffLine {
                kind: DiffLineKind::Remove,
                text: rest.to_string(),
            });
        } else if let Some(rest) = line.strip_prefix(' ') {
            current.push(DiffLine {
                kind: DiffLineKind::Context,
                text: rest.to_string(),
            });
        }
    }
    if !current.is_empty() {
        hunks.push(DiffHunk { lines: current });
    }

    let path = path?;
    if hunks.is_empty() {
        return None;
    }
    Some((path, hunks))
}

// ── write_file path extractor ────────────────────────────────────────

/// Best-effort path extraction from a write_file ToolResult output.
///
/// The `WriteFile` tool emits something like
/// `"Wrote 42 bytes to crates/foo/bar.rs"`. We parse that pattern and
/// fall back to `None` if it doesn't match.
pub(crate) fn extract_write_file_path_from_result(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if let Some(idx) = trimmed.rfind(" to ") {
        let candidate = &trimmed[idx + 4..];
        if !candidate.is_empty() {
            return Some(candidate.to_string());
        }
    }
    None
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::DiffLineKind;

    #[test]
    fn tool_call_args_preview_extracts_path() {
        let preview = preview_args(r#"{"path":"src/agent.rs"}"#);
        assert!(preview.contains("path"));
        assert!(preview.contains("src/agent.rs"));
    }

    #[test]
    fn blocks_from_messages_reconstructs_conversation() {
        use recursive::llm::ToolCall;
        use recursive::message::Message;

        let messages = vec![
            Message::system("you are a helpful agent"),
            Message::user("hello"),
            Message::assistant_with_tool_calls(
                "let me check",
                vec![ToolCall {
                    id: "call-1".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({"path": "src/foo.rs"}),
                }],
            ),
            Message::tool_result("call-1", "file contents here"),
            Message::assistant("all done"),
        ];

        let blocks = blocks_from_messages(&messages);

        // System message is skipped.
        assert!(!blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::System { .. })));

        // User → assistant(text) → tool call(filled) → assistant(text).
        assert!(matches!(&blocks[0], TranscriptBlock::User { text } if text == "hello"));
        assert!(
            matches!(&blocks[1], TranscriptBlock::Assistant { text, .. } if text == "let me check")
        );
        match &blocks[2] {
            TranscriptBlock::ToolCall {
                id, name, result, ..
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "Read");
                let r = result.as_ref().expect("tool result should be filled");
                assert!(r.success);
                assert_eq!(r.output, "file contents here");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert!(
            matches!(&blocks[3], TranscriptBlock::Assistant { text, .. } if text == "all done")
        );
    }

    #[test]
    fn blocks_from_messages_orphan_tool_result_renders_standalone() {
        use recursive::message::Message;
        // A tool result with no preceding tool call (truncated transcript).
        let messages = vec![Message::tool_result("orphan", "result body")];
        let blocks = blocks_from_messages(&messages);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            TranscriptBlock::ToolCall { id, result, .. } => {
                assert_eq!(id, "orphan");
                assert_eq!(result.as_ref().unwrap().output, "result body");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn verb_for_tool_categorises_tools() {
        assert_eq!(verb_for_tool("Read"), "Reading");
        assert_eq!(verb_for_tool("Edit"), "Editing");
        assert_eq!(verb_for_tool("Bash"), "Running");
        assert_eq!(verb_for_tool("custom_xyz"), "Calling tool");
    }

    #[test]
    fn parse_v4a_patch_extracts_path_and_pm_lines() {
        let patch = "*** Begin Patch\n*** Update File: src/foo.rs\n@@ pub fn bar()\n pub fn bar() {\n-    let x = 1;\n+    let x = 2;\n }\n*** End Patch";
        let (path, hunks) = parse_v4a_patch(patch).unwrap();
        assert_eq!(path, "src/foo.rs");
        assert!(!hunks.is_empty());
        let kinds: Vec<_> = hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| l.kind.clone()))
            .collect();
        assert!(kinds.contains(&DiffLineKind::Add));
        assert!(kinds.contains(&DiffLineKind::Remove));
    }

    // ── debt tests (2026-07-02) ───────────────────────────────────────────

    #[test]
    fn blocks_from_messages_emits_reasoning_block_when_non_empty() {
        // Assistant message with non-empty reasoning_content and empty
        // content. orig pushes a Reasoning block; mutant `delete !` (42:24)
        // flips the guard to `if reasoning.trim().is_empty()` -> skips it.
        use recursive::message::Message;
        let msg = Message {
            role: recursive::message::Role::Assistant,
            content: String::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: Some("thinking hard".to_string()),
            is_compaction_summary: false,
        };
        let blocks = blocks_from_messages(&[msg]);
        assert_eq!(
            blocks.len(),
            1,
            "expected exactly one Reasoning block, got {blocks:?}"
        );
        assert!(matches!(
            &blocks[0],
            TranscriptBlock::Reasoning { text, .. } if text == "thinking hard"
        ));
    }

    #[test]
    fn clamp_returns_input_unchanged_at_exact_max() {
        // kills `<=`->`>` (119:26): at chars().count() == max, orig returns
        // the string unchanged; mutant enters the truncation branch and
        // appends `…`.
        assert_eq!(clamp("abcd", 4), "abcd");
    }

    #[test]
    fn extract_write_file_path_from_result_finds_path_after_to() {
        // kills `+`->`-` (223:38): `trimmed[idx + 4..]` vs `idx - 4`. The
        // mutant slices the wrong region (and would underflow if idx < 4).
        let path = extract_write_file_path_from_result("Wrote 42 bytes to crates/foo/bar.rs");
        assert_eq!(path.as_deref(), Some("crates/foo/bar.rs"));
    }
}
