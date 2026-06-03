//! Event-loop reducers: `handle_ui_event` and streaming helpers.

use crate::tui::events::UiEvent;
use crate::tui::ui::transcript::render_block;

use super::render::extract_write_file_path_from_result;
use super::{
    parse_apply_patch_input, preview_args, verb_for_tool, App, ToolResultData, TranscriptBlock,
};

impl App {
    /// Apply an event coming from the backend worker.
    pub fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::AssistantPartial { text } => {
                self.append_streaming_assistant(&text);
            }
            UiEvent::Reasoning { content } => {
                // Each step's reasoning arrives as a single block.
                // Push a new TranscriptBlock::Reasoning; the renderer
                // draws a `thinking…` header followed by the
                // reasoning text in dim grey italics.
                self.blocks
                    .push(TranscriptBlock::Reasoning { text: content });
            }
            UiEvent::AssistantMessage { content } => {
                // Goal-147: the legacy `"plan:"` / `"## plan"` text
                // sniff is gone — plan-mode now arrives through the
                // structured `UiEvent::PlanProposed` channel. Any
                // assistant text that looks like a plan prefix is now
                // just displayed as-is.
                self.finalise_streaming_assistant(content);
            }
            UiEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                let preview = preview_args(&arguments);
                // Try to also synthesise a Diff block when the tool
                // looks like an edit. For apply_patch we'll create the
                // Diff alongside the ToolCall; for write_file we wait
                // for the ToolResult so the byte count is accurate.
                self.blocks.push(TranscriptBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args_preview: preview,
                    result: None,
                });
                if name == "apply_patch" {
                    if let Some((path, hunks)) = parse_apply_patch_input(&arguments) {
                        self.blocks.push(TranscriptBlock::Diff { path, hunks });
                    }
                }
                // Refine spinner verb based on tool category.
                self.turn.spinner_verb = verb_for_tool(&name);
            }
            UiEvent::ToolResult {
                id,
                name,
                output,
                success,
            } => {
                // For write_file, render a synthesised Diff stub
                // ("Created/Updated path (N bytes)") alongside the
                // existing ToolCall. We also stamp the ToolCall as
                // completed (empty output) so its bullet turns green
                // instead of staying yellow. The Diff block below
                // carries the actual file change.
                if name == "write_file" && success {
                    if let Some(path) = extract_write_file_path_from_result(&output) {
                        self.blocks.push(TranscriptBlock::Diff {
                            path,
                            hunks: vec![],
                        });
                        if let Some(TranscriptBlock::ToolCall { result, .. }) = self
                            .blocks
                            .iter_mut()
                            .rev()
                            .find(|b| {
                                matches!(b, TranscriptBlock::ToolCall { id: cid, .. } if cid == &id)
                            }) {
                            *result = Some(ToolResultData {
                                success: true,
                                output: String::new(),
                                expanded: false,
                            });
                        }
                        return;
                    }
                }
                // Find the matching ToolCall (most recent first) and
                // fill in its result. Falls back to pushing a new
                // ToolCall block if no match is found — this can
                // happen when ToolResult arrives before ToolCall
                // (shouldn't, but be defensive).
                let mut filled = false;
                for block in self.blocks.iter_mut().rev() {
                    if let TranscriptBlock::ToolCall {
                        id: cid,
                        result,
                        name: cname,
                        args_preview,
                    } = block
                    {
                        if cid == &id {
                            *result = Some(ToolResultData {
                                success,
                                output: output.clone(),
                                expanded: false,
                            });
                            // Backfill name/args if the matching
                            // ToolCall was synthesised by the
                            // fallback path.
                            if cname.is_empty() {
                                *cname = name.clone();
                            }
                            if args_preview.is_empty() {
                                *args_preview = String::new();
                            }
                            filled = true;
                            break;
                        }
                    }
                }
                if !filled {
                    self.blocks.push(TranscriptBlock::ToolCall {
                        id,
                        name,
                        args_preview: String::new(),
                        result: Some(ToolResultData {
                            success,
                            output,
                            expanded: false,
                        }),
                    });
                }
            }
            UiEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                self.usage.record(input_tokens, output_tokens);
            }
            UiEvent::Latency { llm_ms } => {
                self.usage.last_latency_ms = llm_ms;
                self.pending_latency_ms = Some(llm_ms);
                // Stamp any in-flight streaming assistant block.
                if let Some(TranscriptBlock::Assistant {
                    streaming: true,
                    latency_ms,
                    ..
                }) = self.blocks.last_mut()
                {
                    *latency_ms = Some(llm_ms);
                }
            }
            UiEvent::Compacted { removed, kept } => {
                self.blocks
                    .push(TranscriptBlock::Compacted { removed, kept });
            }
            UiEvent::TurnFinished => {
                // Make sure the last streaming assistant block is
                // closed in case the provider never emitted a final
                // AssistantText (some providers stream tokens then
                // stop without a synthesised final).
                if let Some(TranscriptBlock::Assistant { streaming, .. }) = self.blocks.last_mut() {
                    *streaming = false;
                }
                self.turn.finish();
                self.pending_latency_ms = None;
            }
            UiEvent::Error { message } => {
                self.blocks.push(TranscriptBlock::Error {
                    text: format!("Error: {message}"),
                });
            }
            UiEvent::PlanProposed {
                plan_text,
                tool_calls,
            } => {
                // Fix-E: show the plan inline in the transcript rather
                // than as a floating modal.  The dedicated
                // `TranscriptBlock::PlanProposal` variant is rendered
                // as a bordered box inside the message stream, and the
                // status bar already shows "plan: y/n" to signal that
                // the input layer is awaiting a decision.
                self.blocks.push(TranscriptBlock::PlanProposal {
                    plan_text,
                    tool_calls,
                });
                self.plan_awaiting_approval = true;
            }
            UiEvent::PlanConfirmed => {
                self.close_plan_review_modal();
                self.blocks.push(TranscriptBlock::System {
                    text: "Plan approved".into(),
                });
                self.plan_awaiting_approval = false;
            }
            UiEvent::PlanRejected { reason } => {
                self.close_plan_review_modal();
                self.blocks.push(TranscriptBlock::System {
                    text: format!("Plan rejected: {reason}"),
                });
                self.plan_awaiting_approval = false;
            }

            // ── Goal-202: plan-mode pre-confirmation events ─────────────────
            UiEvent::PlanModeRequested { reason } => {
                // Render the request inline in the transcript so the user can
                // read the reason and decide without a modal obscuring context.
                self.blocks.push(TranscriptBlock::PlanModeRequest {
                    reason,
                    approved: None,
                });
                self.plan_mode_request_pending = true;
            }
            UiEvent::PlanModeApproved => {
                // Mark the last PlanModeRequest block as approved.
                for block in self.blocks.iter_mut().rev() {
                    if let TranscriptBlock::PlanModeRequest { approved, .. } = block {
                        *approved = Some(true);
                        break;
                    }
                }
                self.plan_mode_request_pending = false;
            }
            UiEvent::PlanModeRejected { reason: _ } => {
                // Mark the last PlanModeRequest block as rejected.
                for block in self.blocks.iter_mut().rev() {
                    if let TranscriptBlock::PlanModeRequest { approved, .. } = block {
                        *approved = Some(false);
                        break;
                    }
                }
                self.plan_mode_request_pending = false;
            }

            // Goal-167: replace the task list whenever the agent calls todo_write.
            UiEvent::TodoUpdated { todos } => {
                self.current_todos = todos;
            }

            // ── Goal-168: goal-loop events ──────────────────────────────────
            UiEvent::GoalContinuing { reason, turns } => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!("Goal continuing (turn {turns}): {reason}"),
                });
                // Update mirrored state.
                if let Some(ref mut gs) = self.active_goal {
                    gs.turns = turns;
                    gs.last_reason = Some(reason);
                }
            }
            UiEvent::GoalAchieved { condition, turns } => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!("Goal achieved after {turns} turns: \"{condition}\""),
                });
                self.active_goal = None;
            }
            UiEvent::GoalCleared => {
                self.blocks.push(TranscriptBlock::System {
                    text: "Goal cleared.".into(),
                });
                self.active_goal = None;
            }
            // ── Goal-170: turn abort ──────────────────────────────────────────
            UiEvent::Interrupted => {
                self.blocks.push(TranscriptBlock::System {
                    text: "[Interrupted]".into(),
                });
                self.turn.finish();
                self.pending_latency_ms = None;
                self.plan_awaiting_approval = false;
            }
            UiEvent::McpServersLoaded { entries } => {
                self.push_modal(crate::tui::ui::modal::Modal::McpServers {
                    entries,
                    selected: 0,
                });
            }
            UiEvent::SessionResumed {
                session_id,
                turn_count,
            } => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!("▶ Resumed session {session_id} ({turn_count} messages)"),
                });
                self.turn.finish();
                self.scroll_to_bottom();
            }

            // Goal-210: hook progress events → status-bar style System blocks.
            UiEvent::HookStarted {
                hook_event,
                hook_name,
                ..
            } => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!("⚡ hook [{hook_event}] {hook_name} started"),
                });
                self.scroll_to_bottom();
            }
            UiEvent::HookProgress {
                hook_name,
                last_line,
                ..
            } => {
                // Update the last System block if it was a hook block; otherwise push a new one.
                let hook_prefix = "⚡ hook".to_string();
                let should_update = self
                    .blocks
                    .last()
                    .map(|b| matches!(b, TranscriptBlock::System { text } if text.starts_with(&hook_prefix)))
                    .unwrap_or(false);
                let text = format!("⚡ hook {hook_name}: {last_line}");
                if should_update {
                    if let Some(TranscriptBlock::System { text: t }) = self.blocks.last_mut() {
                        *t = text;
                    }
                } else {
                    self.blocks.push(TranscriptBlock::System { text });
                }
                self.scroll_to_bottom();
            }
            UiEvent::HookFinished {
                hook_event,
                hook_name,
                outcome,
                duration_ms,
            } => {
                self.blocks.push(TranscriptBlock::System {
                    text: format!(
                        "✓ hook [{hook_event}] {hook_name} → {outcome} ({duration_ms}ms)"
                    ),
                });
                self.scroll_to_bottom();
            }
            UiEvent::HookSystemMessage { text } => {
                self.blocks.push(TranscriptBlock::System { text });
                self.scroll_to_bottom();
            }

            #[cfg(feature = "weixin")]
            UiEvent::WeixinMessage { user_id, text } => {
                self.blocks
                    .push(TranscriptBlock::WeixinMessage { user_id, text });
                self.scroll_to_bottom();
            }
        }
        // Sticky-scroll: when the user is already at the bottom
        // (scroll_offset == 0), keep them pinned as new content
        // arrives. If they've explicitly scrolled up (Shift+↑ /
        // PgUp set scroll_offset > 0), preserve their position so
        // streaming tokens don't yank them back down mid-read.
        if self.scroll_offset == 0 {
            self.scroll_to_bottom();
        }
    }

    /// If the topmost modal is a `PlanReview`, pop it. No-op
    /// otherwise — the runtime may emit `PlanConfirmed` after the
    /// user already dismissed the modal manually, in which case we
    /// only want to push the System block.
    fn close_plan_review_modal(&mut self) {
        if matches!(
            self.modals.last(),
            Some(crate::tui::ui::modal::Modal::PlanReview { .. })
        ) {
            self.modals.pop();
        }
    }

    pub(crate) fn start_turn(&mut self) {
        self.turn.start();
        self.turn_count = self.turn_count.saturating_add(1);
    }

    fn append_streaming_assistant(&mut self, chunk: &str) {
        if let Some(TranscriptBlock::Assistant {
            text,
            streaming: true,
            ..
        }) = self.blocks.last_mut()
        {
            text.push_str(chunk);
        } else {
            self.blocks.push(TranscriptBlock::Assistant {
                text: chunk.to_string(),
                streaming: true,
                latency_ms: self.pending_latency_ms,
            });
        }
    }

    fn finalise_streaming_assistant(&mut self, content: String) {
        if let Some(TranscriptBlock::Assistant {
            text,
            streaming,
            latency_ms,
        }) = self.blocks.last_mut()
        {
            if *streaming {
                *text = content;
                *streaming = false;
                if latency_ms.is_none() {
                    *latency_ms = self.pending_latency_ms;
                }
                return;
            }
        }
        self.blocks.push(TranscriptBlock::Assistant {
            text: content,
            streaming: false,
            latency_ms: self.pending_latency_ms,
        });
    }

    /// Scan `blocks[last_printed_idx..]` and push any "finalized"
    /// blocks into `print_queue` so the main loop can flush them to
    /// the terminal's scrollback buffer via `terminal.insert_before()`.
    ///
    /// A block is considered finalized when:
    /// - `User` — always
    /// - `Assistant` — when `!streaming`
    /// - `ToolCall` — when `result.is_some()`
    /// - `Reasoning` — only when the immediately following block is NOT
    ///   a streaming `Assistant` (prevents the reasoning from being
    ///   separated from its answer in the scrollback)
    /// - All other variants — always
    ///
    /// This is idempotent and safe to call after every event.
    pub fn flush_ready_blocks(&mut self) {
        loop {
            let i = self.last_printed_idx;
            if i >= self.blocks.len() {
                break;
            }
            let ready = match &self.blocks[i] {
                TranscriptBlock::User { .. } => true,
                TranscriptBlock::Assistant { streaming, .. } => !streaming,
                TranscriptBlock::ToolCall { result, .. } => result.is_some(),
                TranscriptBlock::Reasoning { .. } => {
                    // Defer until the following Assistant is finalized so
                    // reasoning and its answer always appear together.
                    !matches!(
                        self.blocks.get(i + 1),
                        Some(TranscriptBlock::Assistant {
                            streaming: true,
                            ..
                        })
                    )
                }
                _ => true,
            };
            if !ready {
                break;
            }
            let is_user = matches!(&self.blocks[i], TranscriptBlock::User { .. });
            let is_first = i == 0;

            // Mirror the spacing from render_blocks(): add blank lines between
            // blocks and extra blank lines around User blocks so the scrollback
            // has the same visual breathing room as the live viewport.
            if !is_first {
                let mut pre: Vec<ratatui::text::Line<'static>> = vec![ratatui::text::Line::raw("")];
                if is_user {
                    // Extra blank before User turns.
                    pre.push(ratatui::text::Line::raw(""));
                }
                self.print_queue.push(pre);
            }

            let lines = render_block(&self.blocks[i], self.theme);
            self.print_queue.push(lines);

            // Extra blank after User turns (same as render_blocks).
            if is_user {
                self.print_queue.push(vec![ratatui::text::Line::raw("")]);
            }

            self.last_printed_idx += 1;
        }
    }

    /// Toggle the most recent completed tool call's expanded flag.
    /// Walks back over `ToolCall` blocks that have a `result` (i.e.
    /// the tool has finished) and flips its `expanded` field. Tool
    /// calls still running (no result yet) are skipped.
    pub(crate) fn toggle_last_expandable(&mut self) {
        for block in self.blocks.iter_mut().rev() {
            if let TranscriptBlock::ToolCall {
                result: Some(ToolResultData { expanded, .. }),
                ..
            } = block
            {
                *expanded = !*expanded;
                return;
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::tui::app::{App, AppScreen, TranscriptBlock};
    use crate::tui::events::UiEvent;

    // ── streaming assistant ────────────────────────────────────────

    #[test]
    fn transcript_apply_partial_token_appends_to_streaming_assistant() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantPartial { text: "hel".into() });
        app.handle_ui_event(UiEvent::AssistantPartial { text: "lo".into() });

        match app.blocks.last() {
            Some(TranscriptBlock::Assistant {
                text, streaming, ..
            }) => {
                assert_eq!(text, "hello");
                assert!(*streaming);
            }
            other => panic!("expected streaming Assistant, got {other:?}"),
        }
    }

    #[test]
    fn transcript_apply_assistant_text_finalizes_streaming() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantPartial { text: "hel".into() });
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "hello world".into(),
        });

        match app.blocks.last() {
            Some(TranscriptBlock::Assistant {
                text, streaming, ..
            }) => {
                assert_eq!(text, "hello world");
                assert!(!*streaming);
            }
            other => panic!("expected finalised Assistant, got {other:?}"),
        }
    }

    #[test]
    fn transcript_assistant_text_without_prior_stream_creates_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "single shot".into(),
        });
        match app.blocks.last() {
            Some(TranscriptBlock::Assistant {
                text, streaming, ..
            }) => {
                assert_eq!(text, "single shot");
                assert!(!*streaming);
            }
            other => panic!("expected non-streaming Assistant, got {other:?}"),
        }
    }

    // ── tool call / result ─────────────────────────────────────────

    #[test]
    fn tool_call_and_result_pair_by_id() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::ToolCall {
            id: "abc".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"src/agent.rs"}"#.into(),
        });
        app.handle_ui_event(UiEvent::ToolResult {
            id: "abc".into(),
            name: "read_file".into(),
            output: "ok".into(),
            success: true,
        });

        // ToolResult now merges into the matching ToolCall block
        // rather than producing a sibling ToolResult block. We
        // expect exactly one ToolCall block for "abc" with a
        // Some(result).
        let tool_calls: Vec<_> = app
            .blocks
            .iter()
            .filter(|b| matches!(b, TranscriptBlock::ToolCall { id, .. } if id == "abc"))
            .collect();
        assert_eq!(tool_calls.len(), 1, "ToolResult must merge into ToolCall");
        let block = tool_calls[0];
        match block {
            TranscriptBlock::ToolCall {
                id,
                result: Some(r),
                ..
            } => {
                assert_eq!(id, "abc");
                assert!(r.success);
                assert_eq!(r.output, "ok");
            }
            other => panic!("expected ToolCall with Some(result), got {other:?}"),
        }
    }

    #[test]
    fn apply_patch_call_emits_diff_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let patch = "*** Begin Patch\n*** Update File: src/foo.rs\n@@ pub fn bar()\n pub fn bar() {\n-    let x = 1;\n+    let x = 2;\n }\n*** End Patch";
        let arguments = serde_json::json!({"input": patch}).to_string();
        app.handle_ui_event(UiEvent::ToolCall {
            id: "1".into(),
            name: "apply_patch".into(),
            arguments,
        });
        let has_diff = app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::Diff { path, .. } if path == "src/foo.rs"));
        assert!(has_diff, "expected Diff block, got {:?}", app.blocks);
    }

    #[test]
    fn write_file_result_renders_diff_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            arguments: r#"{"path":"src/new.rs","contents":"x"}"#.into(),
        });
        app.handle_ui_event(UiEvent::ToolResult {
            id: "1".into(),
            name: "write_file".into(),
            output: "Wrote 42 bytes to src/new.rs".into(),
            success: true,
        });
        let has_diff = app.blocks.iter().any(
            |b| matches!(b, TranscriptBlock::Diff { path, .. } if path.contains("src/new.rs")),
        );
        assert!(has_diff, "expected Diff block from write_file");
    }

    // ── compacted ──────────────────────────────────────────────────

    #[test]
    fn compacted_event_creates_compacted_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::Compacted {
            removed: 12,
            kept: 1,
        });
        assert!(matches!(
            app.blocks.last(),
            Some(TranscriptBlock::Compacted {
                removed: 12,
                kept: 1
            })
        ));
    }

    // ── usage stats ────────────────────────────────────────────────

    #[test]
    fn usage_stats_accumulate_across_turns() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::Usage {
            input_tokens: 100,
            output_tokens: 50,
        });
        app.handle_ui_event(UiEvent::Usage {
            input_tokens: 30,
            output_tokens: 20,
        });
        assert_eq!(app.usage.total_input, 130);
        assert_eq!(app.usage.total_output, 70);
        assert_eq!(app.usage.input_tokens, 30);
        assert_eq!(app.usage.output_tokens, 20);
    }

    // ── error event ─────────────────────────────────────────────────

    #[test]
    fn error_event_pushes_error_block() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::Error {
            message: "boom".into(),
        });
        assert!(matches!(
            app.blocks.last(),
            Some(TranscriptBlock::Error { text }) if text.contains("boom")
        ));
    }

    // ── Plan Mode (Goal 147) ───────────────────────────────────────

    #[test]
    fn plan_proposed_event_opens_plan_review_modal() {
        // Fix-E: PlanProposed now renders inline as a TranscriptBlock
        // instead of opening a floating modal.
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::PlanProposed {
            plan_text: "1. read_file\n2. apply_patch".into(),
            tool_calls: vec![serde_json::json!({
                "name": "read_file",
                "id": "1",
                "arguments": { "path": "src/foo.rs" }
            })],
        });
        // No modal should be opened — plan is inline in the transcript.
        assert!(app.modals.is_empty());
        // The plan proposal block should be in the transcript.
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::PlanProposal { plan_text, .. }
                if plan_text.contains("read_file"))));
        assert!(app.plan_awaiting_approval);
    }

    #[test]
    fn plan_confirmed_closes_modal_and_pushes_system_block() {
        use crate::tui::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "do".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        app.handle_ui_event(UiEvent::PlanConfirmed);
        assert!(app.modals.is_empty());
        assert!(app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::System { text } if text == "Plan approved")));
    }

    #[test]
    fn plan_rejected_pushes_system_block_with_reason() {
        use crate::tui::ui::modal::Modal;
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.modals.push(Modal::PlanReview {
            plan_text: "do".into(),
            tool_calls: vec![],
            edited_text: None,
        });
        app.handle_ui_event(UiEvent::PlanRejected {
            reason: "user rejected".into(),
        });
        assert!(app.modals.is_empty());
        assert!(app.blocks.iter().any(|b| matches!(b,
            TranscriptBlock::System { text } if text == "Plan rejected: user rejected")));
    }

    // ── sticky-scroll ───────────────────────────────────────────────

    /// Sticky-scroll: when the user has explicitly scrolled up,
    /// new events should NOT yank them back to the bottom.
    #[test]
    fn new_event_keeps_scroll_offset_when_user_scrolled_up() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 5; // user pressed Ctrl+B / PgUp etc.
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "hello".into(),
        });
        assert_eq!(app.scroll_offset, 5);
    }

    /// Sticky-scroll counterpart: when the user is at the bottom,
    /// new events DO scroll-to-bottom.
    #[test]
    fn new_event_at_bottom_keeps_user_at_bottom() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 0;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "hello".into(),
        });
        assert_eq!(app.scroll_offset, 0);
    }

    // ── TurnFinished ────────────────────────────────────────────────

    #[test]
    fn turn_finished_stops_turn() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("hi");
        app.handle_ui_event(UiEvent::TurnFinished);
        assert!(!app.turn.running);
    }
}
