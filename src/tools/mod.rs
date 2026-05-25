//! Tool abstraction: any side effect the model can request.
//!
//! Tools are orthogonal to the agent and to each other. To add a capability
//! you implement `Tool` and register it; no other file changes.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;

pub mod apply_patch;
pub mod count_lines;
pub mod fs;
pub mod shell;

pub use apply_patch::ApplyPatch;
pub use count_lines::CountLines;
pub use fs::{ListDir, ReadFile, WriteFile};
pub use shell::RunShell;

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String>;
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(mut self, tool: Arc<dyn Tool>) -> Self {
        let name = tool.spec().name;
        self.tools.insert(name, tool);
        self
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub async fn invoke(&self, name: &str, arguments: Value) -> Result<String> {
        let tool = self
            .get(name)
            .ok_or_else(|| Error::UnknownTool(name.into()))?;
        tool.execute(arguments).await.map_err(|e| match e {
            Error::Tool { .. } | Error::BadToolArgs { .. } | Error::UnknownTool(_) => e,
            other => Error::Tool {
                name: name.into(),
                message: other.to_string(),
            },
        })
    }
}

/// Resolve a possibly-relative path against the workspace root.
///
/// Both the root and the candidate are normalised to an absolute, dot-free
/// form before comparison so that `--workspace .` works exactly the same as
/// `--workspace /abs/path`.
pub(crate) fn resolve_within(root: &std::path::Path, path: &str) -> Result<std::path::PathBuf> {
    let candidate = std::path::Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let abs_root = absolutise(root);
    let abs_joined = absolutise(&joined);
    if !abs_joined.starts_with(&abs_root) {
        return Err(Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!(
                "path `{}` escapes workspace root `{}`",
                path,
                abs_root.display()
            ),
        });
    }
    Ok(abs_joined)
}

/// Turn a path into an absolute, normalised form. Does not touch the disk,
/// so it works for files that don't yet exist (needed by `write_file`).
fn absolutise(p: &std::path::Path) -> std::path::PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(p)
    };
    normalise(&abs)
}

fn normalise(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        use std::path::Component::*;
        match c {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                parameters: serde_json::json!({"type":"object","properties":{"msg":{"type":"string"}}}),
            }
        }
        async fn execute(&self, args: Value) -> Result<String> {
            Ok(args["msg"].as_str().unwrap_or("").into())
        }
    }

    #[tokio::test]
    async fn registry_dispatches_and_errors_on_unknown() {
        let reg = ToolRegistry::new().register(Arc::new(Echo));
        let out = reg
            .invoke("echo", serde_json::json!({"msg":"hi"}))
            .await
            .unwrap();
        assert_eq!(out, "hi");
        let err = reg.invoke("nope", serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, Error::UnknownTool(_)));
    }

    #[test]
    fn resolve_within_rejects_escape() {
        let root = std::path::Path::new("/work");
        assert!(resolve_within(root, "../etc/passwd").is_err());
        assert!(resolve_within(root, "/elsewhere").is_err());
        assert!(resolve_within(root, "src/lib.rs").is_ok());
    }

    #[test]
    fn resolve_within_handles_relative_root() {
        // Regression: `--workspace .` (relative) used to fail the prefix check.
        let cwd = std::env::current_dir().unwrap();
        let resolved = resolve_within(std::path::Path::new("."), "src/lib.rs").unwrap();
        assert!(resolved.starts_with(&cwd));
        assert!(resolved.ends_with("src/lib.rs"));
    }
}
