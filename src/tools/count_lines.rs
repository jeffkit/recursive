//! Count lines tool: returns the number of lines in a text file.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

use super::{resolve_within, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

#[derive(Debug, Clone)]
pub struct CountLines {
    pub root: PathBuf,
}

impl CountLines {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait]
impl Tool for CountLines {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "count_lines".into(),
            description: "Count the number of lines in a text file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root"}
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "count_lines".into(),
            message: "missing `path`".into(),
        })?;
        let abs = resolve_within(&self.root, path)?;
        let contents = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| Error::Tool {
                name: "count_lines".into(),
                message: format!("{}: {e}", abs.display()),
            })?;
        let line_count = contents.lines().count();
        Ok(line_count.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn count_lines_returns_correct_count() {
        let tmp = TempDir::new().unwrap();
        // Write a file with exactly 5 lines
        let path = tmp.path().join("test.txt");
        tokio::fs::write(&path, "line1\nline2\nline3\nline4\nline5")
            .await
            .unwrap();

        let tool = CountLines::new(tmp.path());
        let result = tool.execute(json!({"path": "test.txt"})).await.unwrap();

        assert_eq!(result, "5");
    }

    #[tokio::test]
    async fn count_lines_rejects_escape() {
        let tmp = TempDir::new().unwrap();
        let tool = CountLines::new(tmp.path());
        let err = tool
            .execute(json!({"path": "../etc/passwd"}))
            .await
            .unwrap_err();

        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[tokio::test]
    async fn count_lines_handles_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.txt");
        tokio::fs::write(&path, "").await.unwrap();

        let tool = CountLines::new(tmp.path());
        let result = tool.execute(json!({"path": "empty.txt"})).await.unwrap();

        // An empty file has 0 lines (lines() returns an empty iterator)
        assert_eq!(result, "0");
    }

    #[tokio::test]
    async fn count_lines_handles_single_line_no_newline() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("single.txt");
        tokio::fs::write(&path, "just one line").await.unwrap();

        let tool = CountLines::new(tmp.path());
        let result = tool.execute(json!({"path": "single.txt"})).await.unwrap();

        assert_eq!(result, "1");
    }
}
