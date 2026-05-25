//! Filesystem tools: `read_file`, `write_file`, `list_dir`.
//!
//! All paths are sandboxed to a workspace root. Reads/writes outside the
//! root are rejected at the tool layer, so the model can't (accidentally
//! or otherwise) touch the rest of the disk.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

use super::{resolve_within, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

#[derive(Debug, Clone)]
pub struct ReadFile {
    pub root: PathBuf,
    pub max_bytes: usize,
}

impl ReadFile {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), max_bytes: 256 * 1024 }
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a UTF-8 text file under the workspace. Returns the file contents.".into(),
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
            name: "read_file".into(),
            message: "missing `path`".into(),
        })?;
        let abs = resolve_within(&self.root, path)?;
        let bytes = tokio::fs::read(&abs).await.map_err(|e| Error::Tool {
            name: "read_file".into(),
            message: format!("{}: {e}", abs.display()),
        })?;
        if bytes.len() > self.max_bytes {
            return Err(Error::Tool {
                name: "read_file".into(),
                message: format!("file too large: {} bytes (max {})", bytes.len(), self.max_bytes),
            });
        }
        String::from_utf8(bytes).map_err(|e| Error::Tool {
            name: "read_file".into(),
            message: format!("not utf-8: {e}"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct WriteFile {
    pub root: PathBuf,
}

impl WriteFile {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Write/overwrite a UTF-8 text file under the workspace. Parent directories are created.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root"},
                    "contents": {"type": "string", "description": "Full new contents of the file"}
                },
                "required": ["path", "contents"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "write_file".into(),
            message: "missing `path`".into(),
        })?;
        let contents = args["contents"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "write_file".into(),
            message: "missing `contents`".into(),
        })?;
        let abs = resolve_within(&self.root, path)?;
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| Error::Tool {
                name: "write_file".into(),
                message: format!("mkdir {}: {e}", parent.display()),
            })?;
        }
        tokio::fs::write(&abs, contents).await.map_err(|e| Error::Tool {
            name: "write_file".into(),
            message: format!("{}: {e}", abs.display()),
        })?;
        Ok(format!("wrote {} bytes to {}", contents.len(), path))
    }
}

#[derive(Debug, Clone)]
pub struct ListDir {
    pub root: PathBuf,
}

impl ListDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait]
impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description: "List entries of a directory under the workspace. Returns one path per line, `/` suffix for dirs.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory relative to the workspace root", "default": "."}
                }
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path = args["path"].as_str().unwrap_or(".");
        let abs = resolve_within(&self.root, path)?;
        let mut entries = tokio::fs::read_dir(&abs).await.map_err(|e| Error::Tool {
            name: "list_dir".into(),
            message: format!("{}: {e}", abs.display()),
        })?;
        let mut lines = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(|e| Error::Tool {
            name: "list_dir".into(),
            message: e.to_string(),
        })? {
            let name = entry.file_name().to_string_lossy().to_string();
            let kind = entry.file_type().await.ok();
            let suffix = if kind.is_some_and(|k| k.is_dir()) { "/" } else { "" };
            lines.push(format!("{name}{suffix}"));
        }
        lines.sort();
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let w = WriteFile::new(tmp.path());
        let r = ReadFile::new(tmp.path());
        w.execute(json!({"path":"hello.txt","contents":"world"})).await.unwrap();
        let got = r.execute(json!({"path":"hello.txt"})).await.unwrap();
        assert_eq!(got, "world");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        WriteFile::new(tmp.path())
            .execute(json!({"path":"a/b/c.txt","contents":"x"}))
            .await
            .unwrap();
        assert!(tmp.path().join("a/b/c.txt").exists());
    }

    #[tokio::test]
    async fn list_dir_sorts_and_marks_dirs() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let out = ListDir::new(tmp.path()).execute(json!({"path":"."})).await.unwrap();
        assert_eq!(out, "a.txt\nsub/");
    }

    #[tokio::test]
    async fn rejects_escape() {
        let tmp = TempDir::new().unwrap();
        let r = ReadFile::new(tmp.path());
        let err = r.execute(json!({"path":"../etc/passwd"})).await.unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }
}
