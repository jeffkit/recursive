//! `count_lines` tool: returns the number of lines in a text file.
//!
//! All paths are sandboxed to a workspace root, same as `ReadFile`.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

use super::{resolve_within, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

// ---------------------------------------------------------------------------
// CountLines
// ---------------------------------------------------------------------------

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
            description: "Return the number of lines in a text file inside the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace root"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "count_lines".into(),
            message: "missing `path`".into(),
        })?;
        let abs = resolve_within(&self.root, path)?;
        let content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| Error::Tool {
                name: "count_lines".into(),
                call_id: None,
                message: format!("{}: {e}", abs.display()),
            })?;
        let count = content.lines().count();
        Ok(count.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn count_lines_happy_path() {
        let tmp = TempDir::new().unwrap();
        let contents = "line1\nline2\nline3\n";
        std::fs::write(tmp.path().join("test.txt"), contents).unwrap();
        let tool = CountLines::new(tmp.path());
        let result = tool.execute(json!({"path": "test.txt"})).await.unwrap();
        assert_eq!(result, "3");
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
}
