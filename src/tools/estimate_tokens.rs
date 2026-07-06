//! Token estimation tool: estimate token count for text or file contents.
//!
//! Uses a simple char/4 heuristic — good enough for budget planning across
//! GPT/Claude/DeepSeek models (all converge to ~3.5-4.5 chars/token in English,
//! code is closer to 3).

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::{resolve_within_any, AccessTier, SharedSandboxRoots, Tool};

pub struct EstimateTokens {
    workspace: PathBuf,
    extra_roots: Vec<(PathBuf, AccessTier)>,
    session_roots: Option<SharedSandboxRoots>,
}

impl EstimateTokens {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            extra_roots: Vec::new(),
            session_roots: None,
        }
    }

    /// Append additional allowed sandbox roots. See
    /// [`crate::tools::fs::ReadFile::with_extra_roots`].
    pub fn with_extra_roots(
        mut self,
        extra: impl IntoIterator<Item = (PathBuf, AccessTier)>,
    ) -> Self {
        self.extra_roots.extend(extra);
        self
    }

    /// Attach the shared, session-mutable roots slot. See [`SharedSandboxRoots`].
    pub fn with_session_roots(mut self, slot: SharedSandboxRoots) -> Self {
        self.session_roots = Some(slot);
        self
    }

    /// Convenience: attach the shared slot only when `Some`.
    pub fn with_session_roots_opt(mut self, slot: Option<SharedSandboxRoots>) -> Self {
        if let Some(s) = slot {
            self.session_roots = Some(s);
        }
        self
    }

    fn all_roots(&self) -> Vec<(PathBuf, AccessTier)> {
        let mut v: Vec<(PathBuf, AccessTier)> = Vec::with_capacity(self.extra_roots.len() + 1);
        v.push((self.workspace.clone(), AccessTier::ReadWrite));
        v.extend(self.extra_roots.iter().cloned());
        if let Some(slot) = &self.session_roots {
            if let Ok(roots) = slot.read() {
                v.extend(roots.iter().cloned());
            }
        }
        v
    }

    /// Estimate tokens using chars/4 heuristic.
    fn estimate(&self, text: &str) -> (usize, usize, &'static str) {
        let chars = text.len();
        let tokens = (chars as f64 / 4.0).ceil() as usize;
        (tokens, chars, "chars-over-4")
    }
}

#[async_trait]
impl Tool for EstimateTokens {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "estimate_tokens".into(),
            description:
                "Estimate the number of tokens in a piece of text or a file. Useful for budgeting transcript space."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Literal text to estimate tokens for"
                    },
                    "path": {
                        "type": "string",
                        "description": "Path to a file (workspace-relative) to estimate tokens for"
                    }
                },
                "anyOf": [
                    {"required": ["text"]},
                    {"required": ["path"]}
                ]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let text_arg = arguments.get("text");
        let path_arg = arguments.get("path");

        // Check which argument is provided
        let (tokens, chars, method) = match (text_arg, path_arg) {
            (Some(text_val), None) => {
                // text provided directly
                let text = text_val.as_str().ok_or_else(|| Error::BadToolArgs {
                    name: "estimate_tokens".into(),
                    message: "text must be a string".into(),
                })?;
                self.estimate(text)
            }
            (None, Some(path_val)) => {
                // path provided - read file contents
                let path = path_val.as_str().ok_or_else(|| Error::BadToolArgs {
                    name: "estimate_tokens".into(),
                    message: "path must be a string".into(),
                })?;

                // Resolve and read the file (sandboxed)
                let abs_path = resolve_within_any(&self.all_roots(), path, false)?;
                let content =
                    tokio::fs::read_to_string(&abs_path)
                        .await
                        .map_err(|e| Error::Tool {
                            name: "estimate_tokens".into(),
                            call_id: None,
                            message: format!("failed to read file {}: {}", abs_path.display(), e),
                        })?;
                self.estimate(&content)
            }
            (Some(_), Some(_)) => {
                // Both provided - prefer text (or error, as per spec)
                return Err(Error::BadToolArgs {
                    name: "estimate_tokens".into(),
                    message: "provide exactly one of 'text' or 'path', not both".into(),
                });
            }
            (None, None) => {
                return Err(Error::BadToolArgs {
                    name: "estimate_tokens".into(),
                    message: "must provide either 'text' or 'path'".into(),
                });
            }
        };

        Ok(format!(
            "tokens≈{} (chars={}, method={})",
            tokens, chars, method
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a temporary workspace dir.
    fn tmp_workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path().to_path_buf();
        (tmp, ws)
    }

    #[test]
    fn estimate_text_basic() {
        let tool = EstimateTokens::new("/tmp");
        let text = "Hello, world! This is a test string.";
        let (tokens, chars, method) = tool.estimate(text);

        // 36 chars / 4 = 9 -> ceil = 9 tokens
        assert_eq!(chars, 36);
        assert_eq!(tokens, 9);
        assert_eq!(method, "chars-over-4");
    }

    #[tokio::test]
    async fn estimate_path_reads_file() {
        let (_tmp, ws) = tmp_workspace();
        let tool = EstimateTokens::new(&ws);

        // Write a test file
        let test_path = ws.join("test.txt");
        let content = "This is a test file.\nIt has multiple lines.\n";
        tokio::fs::write(&test_path, content).await.unwrap();

        let result = tool.execute(json!({ "path": "test.txt" })).await.unwrap();
        assert!(result.contains("tokens≈"));
        assert!(result.contains("chars="));
        assert!(result.contains("method=chars-over-4"));
    }

    #[tokio::test]
    async fn estimate_path_outside_workspace() {
        let (_tmp, ws) = tmp_workspace();
        let tool = EstimateTokens::new(&ws);

        // Try to read a path outside the workspace - should error
        let result = tool.execute(json!({ "path": "../etc/passwd" })).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("escapes"),
            "expected escape error, got: {err}"
        );
    }

    #[tokio::test]
    async fn estimate_neither_arg_errors() {
        let tool = EstimateTokens::new("/tmp");
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err
            .to_string()
            .contains("must provide either 'text' or 'path'"));
    }

    #[tokio::test]
    async fn estimate_both_args_errors() {
        let tool = EstimateTokens::new("/tmp");
        let result = tool
            .execute(json!({ "text": "hello", "path": "test.txt" }))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("provide exactly one of"));
    }

    #[tokio::test]
    async fn estimate_text_direct() {
        let tool = EstimateTokens::new("/tmp");
        let result = tool
            .execute(json!({ "text": "Hello, world!" }))
            .await
            .unwrap();
        // "Hello, world!" = 13 chars -> 13/4 = 3.25 -> ceil = 4 tokens
        assert!(result.contains("tokens≈4"));
        assert!(result.contains("chars=13"));
    }

    #[test]
    fn is_deferred_true() {
        // kills `replace <impl Tool for EstimateTokens>::is_deferred -> bool with false`
        use crate::tools::Tool;
        let tool = EstimateTokens::new("/tmp");
        assert!(tool.is_deferred(), "EstimateTokens must be deferred (low-frequency tool)");
    }

    #[test]
    fn estimate_uses_ceil_not_floor() {
        // kills `floor()` or truncation mutations in `(chars as f64 / 4.0).ceil() as usize`
        // 5 chars / 4.0 = 1.25 → ceil = 2, floor = 1
        let tool = EstimateTokens::new("/tmp");
        let text = "abcde"; // exactly 5 chars
        let (tokens, chars, _) = tool.estimate(text);
        assert_eq!(chars, 5);
        assert_eq!(tokens, 2, "5 chars must round up to 2 tokens (ceil), not 1 (floor)");
    }
}
