//! `Present` tool — declare the files this turn delivers (goal #133).
//!
//! Declaring a path is the whole operation: the file body is never read into
//! the transcript, only its path and size are recorded on the turn ledger
//! and announced as an [`AgentEvent::DeliverablesPresented`] event. At most
//! [`crate::deliverables::DEFAULT_MAX_PRESENT_FILES`] paths are accepted per
//! call (hard ceiling [`crate::deliverables::MAX_PRESENT_FILES_CEILING`]);
//! anything beyond that is reported back with the reason instead of being
//! dropped silently.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::deliverables::{Deliverables, PresentedFile, DEFAULT_MAX_PRESENT_FILES};
use crate::error::{Error, Result};
use crate::event::{AgentEvent, EventSink};
use crate::llm::ToolSpec;
use crate::tools::audit::ToolSideEffect;
use crate::tools::registry::{SessionToolState, Tool};

/// Primary tool name (PascalCase, matching the rest of the registry).
pub const PRESENT_TOOL_NAME: &str = "Present";

/// Tool that records the turn's delivered files.
pub struct PresentTool {
    ledger: Arc<Deliverables>,
    event_sink: Arc<dyn EventSink>,
    max_files: usize,
}

impl PresentTool {
    /// Create the tool. `event_sink` receives
    /// [`AgentEvent::DeliverablesPresented`] on every successful call with at
    /// least one accepted path.
    pub fn new(ledger: Arc<Deliverables>, event_sink: Arc<dyn EventSink>) -> Self {
        Self {
            ledger,
            event_sink,
            max_files: DEFAULT_MAX_PRESENT_FILES,
        }
    }

    /// Override how many paths one call accepts (clamped to the hard ceiling
    /// by [`Deliverables::present`]).
    pub fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_files = max_files;
        self
    }

    fn bad_args(message: impl Into<String>) -> Error {
        Error::BadToolArgs {
            name: PRESENT_TOOL_NAME.into(),
            message: message.into(),
        }
    }
}

/// Parse the `files` argument: a non-empty array of non-empty strings.
fn parse_paths(args: &Value) -> Result<Vec<String>> {
    let Some(raw) = args.get("files") else {
        return Err(PresentTool::bad_args(
            "missing `files`: expected an array of workspace-relative paths",
        ));
    };
    let Some(items) = raw.as_array() else {
        return Err(PresentTool::bad_args("`files` must be an array of paths"));
    };
    if items.is_empty() {
        return Err(PresentTool::bad_args("`files` must name at least one path"));
    }
    let mut paths = Vec::new();
    for item in items {
        let Some(path) = item.as_str() else {
            return Err(PresentTool::bad_args(
                "every entry of `files` must be a string",
            ));
        };
        let path = path.trim();
        if path.is_empty() {
            return Err(PresentTool::bad_args(
                "`files` must not contain empty paths",
            ));
        }
        paths.push(path.to_string());
    }
    Ok(paths)
}

#[async_trait]
impl Tool for PresentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: PRESENT_TOOL_NAME.into(),
            description: format!(
                "Declare the files this turn delivered. Pass workspace-relative paths only — \
                 file contents are never read into the transcript, and the declaration is \
                 recorded on the turn's change ledger and emitted as a `deliverables_presented` \
                 event for the user interface. Declare the finished deliverables, not every \
                 file you touched; at most {DEFAULT_MAX_PRESENT_FILES} paths per call \
                 (recommended: 1-2, never more than 4). Files written by a sub-agent must be \
                 declared here by you, the caller."
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Workspace-relative paths of the delivered files."
                    }
                },
                "required": ["files"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let raw_paths = parse_paths(&args)?;
        let workspace = self.ledger.workspace().to_path_buf();

        let mut candidates: Vec<PresentedFile> = Vec::new();
        let mut rejected: Vec<(String, String)> = Vec::new();
        for path in &raw_paths {
            match crate::tools::resolve_within(&workspace, path) {
                Err(_) => rejected.push((path.clone(), "escapes the workspace sandbox".into())),
                Ok(abs) => match self.ledger.describe(&abs) {
                    Some(file) => candidates.push(file),
                    None => rejected.push((path.clone(), "not a readable regular file".into())),
                },
            }
        }

        let outcome = self.ledger.present(&candidates, self.max_files);
        rejected.extend(
            outcome
                .rejected
                .iter()
                .map(|r| (r.path.clone(), r.reason.clone())),
        );

        if !outcome.accepted.is_empty() {
            self.event_sink
                .emit(AgentEvent::DeliverablesPresented {
                    turn: self.ledger.current_turn(),
                    files: outcome.accepted.clone(),
                })
                .await;
        }

        let mut out = if outcome.accepted.is_empty() {
            "presented 0 files".to_string()
        } else {
            format!(
                "presented {}/{} file(s) for turn {}:",
                outcome.accepted.len(),
                raw_paths.len(),
                self.ledger.current_turn()
            )
        };
        for file in &outcome.accepted {
            out.push_str(&format!(
                "\n  {} ({})",
                file.path,
                crate::deliverables::human_bytes(file.bytes)
            ));
        }
        if !rejected.is_empty() {
            out.push_str("\nnot presented:");
            for (path, reason) in &rejected {
                out.push_str(&format!("\n  {path} — {reason}"));
            }
        }
        Ok(out)
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        // Records a declaration on the turn ledger — in-memory state only.
        ToolSideEffect::Mutating
    }

    fn is_readonly(&self) -> bool {
        false
    }

    /// Goal 394: rewire to the fork's own ledger, so a forked session
    /// declares into its ledger — never the fork source's.
    fn fork_box(&self, state: &SessionToolState) -> Option<Arc<dyn Tool>> {
        let ledger = state.deliverables.clone()?;
        Some(Arc::new(Self {
            ledger,
            event_sink: self.event_sink.clone(),
            max_files: self.max_files,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deliverables::Budgets;
    use crate::event::ChannelSink;
    use tempfile::TempDir;

    fn tool(dir: &TempDir) -> (PresentTool, Arc<Deliverables>) {
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ledger = Arc::new(
            Deliverables::new(&ws, dir.path().join("private"), Budgets::default()).unwrap(),
        );
        let (sink, rx) = ChannelSink::new();
        drop(rx);
        (PresentTool::new(ledger.clone(), Arc::new(sink)), ledger)
    }

    #[tokio::test]
    async fn presents_existing_files_by_path_only() {
        let dir = TempDir::new().unwrap();
        let (tool, ledger) = tool(&dir);
        ledger.begin_turn(2);
        let ws = ledger.workspace().to_path_buf();
        std::fs::write(ws.join("report.md"), "SECRET-BODY-CONTENT").unwrap();

        let out = tool
            .execute(serde_json::json!({"files": ["report.md"]}))
            .await
            .unwrap();
        assert!(out.contains("presented 1/1 file(s) for turn 2"), "{out}");
        assert!(out.contains("report.md (19 B)"), "{out}");
        assert!(
            !out.contains("SECRET-BODY-CONTENT"),
            "the body must never enter the tool result: {out}"
        );
        assert_eq!(ledger.presented().len(), 1);
    }

    #[tokio::test]
    async fn emits_a_presented_event_on_success() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("out.txt"), "x").unwrap();
        let ledger = Arc::new(
            Deliverables::new(&ws, dir.path().join("private"), Budgets::default()).unwrap(),
        );
        ledger.begin_turn(5);
        let (sink, mut rx) = ChannelSink::new();
        let tool = PresentTool::new(ledger, Arc::new(sink));

        tool.execute(serde_json::json!({"files": ["out.txt"]}))
            .await
            .unwrap();
        match rx.recv().await.expect("a presented event") {
            AgentEvent::DeliverablesPresented { turn, files } => {
                assert_eq!(turn, 5);
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path, "out.txt");
                assert_eq!(files[0].bytes, 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_event_when_every_path_is_rejected() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ledger = Arc::new(
            Deliverables::new(&ws, dir.path().join("private"), Budgets::default()).unwrap(),
        );
        let (sink, mut rx) = ChannelSink::new();
        let tool = PresentTool::new(ledger, Arc::new(sink));

        let out = tool
            .execute(serde_json::json!({"files": ["../escape.txt", "missing.txt"]}))
            .await
            .unwrap();
        assert!(out.contains("presented 0 files"), "{out}");
        assert!(out.contains("escapes the workspace sandbox"), "{out}");
        assert!(out.contains("not a readable regular file"), "{out}");
        assert!(rx.try_recv().is_err(), "a rejected present must not emit");
    }

    #[tokio::test]
    async fn directories_are_not_deliverables() {
        let dir = TempDir::new().unwrap();
        let (tool, ledger) = tool(&dir);
        ledger.begin_turn(1);
        let ws = ledger.workspace().to_path_buf();
        std::fs::create_dir_all(ws.join("out")).unwrap();
        std::fs::write(ws.join("out/report.md"), "x").unwrap();

        let out = tool
            .execute(serde_json::json!({"files": ["out"]}))
            .await
            .unwrap();
        assert!(out.contains("presented 0 files"), "{out}");
        assert!(out.contains("out — not a readable regular file"), "{out}");
        assert!(ledger.presented().is_empty());
    }

    #[tokio::test]
    async fn max_files_budget_is_reported_not_swallowed() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ledger = Arc::new(
            Deliverables::new(&ws, dir.path().join("private"), Budgets::default()).unwrap(),
        );
        let (sink, rx) = ChannelSink::new();
        drop(rx);
        let tool = PresentTool::new(ledger.clone(), Arc::new(sink)).with_max_files(3);
        ledger.begin_turn(1);
        let names: Vec<String> = (0..5).map(|i| format!("f{i}.txt")).collect();
        for name in &names {
            std::fs::write(ws.join(name), "x").unwrap();
        }

        let out = tool
            .execute(serde_json::json!({ "files": names }))
            .await
            .unwrap();
        assert!(out.contains("presented 3/5 file(s)"), "{out}");
        assert!(out.contains("beyond the maxFiles=3 budget"), "{out}");
        assert_eq!(ledger.presented().len(), 3);
    }

    #[tokio::test]
    async fn malformed_arguments_are_rejected() {
        let dir = TempDir::new().unwrap();
        let (tool, _ledger) = tool(&dir);
        for args in [
            serde_json::json!({}),
            serde_json::json!({"files": "a.txt"}),
            serde_json::json!({"files": []}),
            serde_json::json!({"files": [""]}),
            serde_json::json!({"files": [1]}),
        ] {
            let err = tool.execute(args.clone()).await.unwrap_err();
            assert!(
                matches!(err, Error::BadToolArgs { .. }),
                "{args} should be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn spec_declares_the_files_array() {
        let dir = TempDir::new().unwrap();
        let (tool, _ledger) = tool(&dir);
        let spec = tool.spec();
        assert_eq!(spec.name, PRESENT_TOOL_NAME);
        assert_eq!(spec.parameters["required"][0], "files");
        assert_eq!(spec.parameters["properties"]["files"]["type"], "array");
        assert!(
            spec.description.contains("never read into the transcript"),
            "{}",
            spec.description
        );
    }

    #[test]
    fn side_effect_class_is_mutating_but_never_readonly() {
        let dir = TempDir::new().unwrap();
        let (tool, _ledger) = tool(&dir);
        assert_eq!(tool.side_effect_class(), ToolSideEffect::Mutating);
        assert!(!tool.is_readonly());
    }
}
