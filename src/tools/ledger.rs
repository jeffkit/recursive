//! `ChangeLedger` tool — read back this turn's change ledger (goal #133).
//!
//! The ledger is produced by [`crate::deliverables::Deliverables`]: a
//! zero-pollution git shadow index for repository workspaces, a
//! content-addressed walk otherwise. This tool only renders it — paths,
//! statuses, sizes and bounded diffs, so the model can check what it
//! actually changed without re-reading the working tree.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::deliverables::Deliverables;
use crate::error::Result;
use crate::llm::ToolSpec;
use crate::tools::audit::ToolSideEffect;
use crate::tools::registry::{SessionToolState, Tool};
use crate::tools::tool_kind::ToolKind;

/// Primary tool name.
pub const CHANGE_LEDGER_TOOL_NAME: &str = "ChangeLedger";

/// Cap on the rendered ledger handed back to the model, so a huge change set
/// cannot flood the transcript. Truncation is announced, never silent.
pub const MAX_LEDGER_RENDER_BYTES: usize = 64 * 1024;

/// Tool that renders the current turn's change ledger.
pub struct ChangeLedgerTool {
    ledger: Arc<Deliverables>,
    max_render_bytes: usize,
}

impl ChangeLedgerTool {
    pub fn new(ledger: Arc<Deliverables>) -> Self {
        Self {
            ledger,
            max_render_bytes: MAX_LEDGER_RENDER_BYTES,
        }
    }

    pub fn with_max_render_bytes(mut self, bytes: usize) -> Self {
        self.max_render_bytes = bytes;
        self
    }

    fn truncate(&self, text: String) -> String {
        if text.len() <= self.max_render_bytes {
            return text;
        }
        let mut end = self.max_render_bytes;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}\n... [ledger output truncated at {} bytes; the full ledger stays in the session event stream]",
            &text[..end], self.max_render_bytes
        )
    }
}

#[async_trait]
impl Tool for ChangeLedgerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: CHANGE_LEDGER_TOOL_NAME.into(),
            description: "Read this turn's change ledger: every file added, modified or deleted \
                          since the turn started, with sizes and bounded unified diffs, plus the \
                          deliverables declared via the `Present` tool. Use it to verify what you \
                          changed instead of re-reading files. Read-only."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
        }
    }

    async fn execute(&self, _args: Value) -> Result<String> {
        let Some(changes) = self.ledger.changes()? else {
            return Ok(
                "no change ledger for this turn: nothing was modified and no baseline was taken"
                    .to_string(),
            );
        };
        if changes.is_empty() {
            return Ok(format!(
                "turn {}: no file changes and no presented deliverables",
                changes.turn
            ));
        }
        Ok(self.truncate(changes.render()))
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    fn is_readonly(&self) -> bool {
        true
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    /// Goal 394: rewire to the fork's own ledger, so a forked session reads
    /// its own change set — never the fork source's.
    fn fork_box(&self, state: &SessionToolState) -> Option<Arc<dyn Tool>> {
        let ledger = state.deliverables.clone()?;
        Some(Arc::new(Self {
            ledger,
            max_render_bytes: self.max_render_bytes,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deliverables::Budgets;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Arc<Deliverables>, ChangeLedgerTool) {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ledger = Arc::new(
            Deliverables::new(&ws, dir.path().join("private"), Budgets::default()).unwrap(),
        );
        let tool = ChangeLedgerTool::new(ledger.clone());
        (dir, ledger, tool)
    }

    #[tokio::test]
    async fn reports_no_ledger_when_nothing_ran() {
        let (_dir, ledger, tool) = fixture();
        ledger.begin_turn(1);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(out.contains("no change ledger"), "{out}");
    }

    #[tokio::test]
    async fn renders_changes_from_the_current_turn() {
        let (_dir, ledger, tool) = fixture();
        let ws = ledger.workspace().to_path_buf();
        std::fs::write(ws.join("a.txt"), "one\n").unwrap();
        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();

        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(out.contains("turn 1 change ledger"), "{out}");
        assert!(out.contains("~ a.txt"), "{out}");
        assert!(out.contains("+two"), "{out}");
    }

    #[tokio::test]
    async fn reports_an_empty_turn_without_a_diff() {
        let (_dir, ledger, tool) = fixture();
        ledger.begin_turn(2);
        ledger.ensure_baseline().unwrap();
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(out, "turn 2: no file changes and no presented deliverables");
    }

    #[tokio::test]
    async fn truncation_is_announced() {
        let (_dir, ledger, tool) = fixture();
        let ws = ledger.workspace().to_path_buf();
        ledger.begin_turn(1);
        ledger.ensure_baseline().unwrap();
        let body: String = (0..400).map(|i| format!("line-{i}\n")).collect();
        std::fs::write(ws.join("big.txt"), body.clone()).unwrap();
        std::fs::write(ws.join("big.txt"), format!("{body}tail\n")).unwrap();

        let tool = tool.with_max_render_bytes(200);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(
            out.contains("[ledger output truncated at 200 bytes"),
            "{out}"
        );
    }

    #[test]
    fn spec_is_read_only_and_takes_no_arguments() {
        let (_dir, _ledger, tool) = fixture();
        let spec = tool.spec();
        assert_eq!(spec.name, CHANGE_LEDGER_TOOL_NAME);
        assert_eq!(spec.parameters["type"], "object");
        assert_eq!(tool.side_effect_class(), ToolSideEffect::ReadOnly);
        assert!(tool.is_readonly());
        // ACP must not report a read-only tool as `other`.
        assert_eq!(tool.kind(), ToolKind::Read);
        assert_eq!(
            ToolKind::from_tool_name(CHANGE_LEDGER_TOOL_NAME),
            ToolKind::Read
        );
    }
}
