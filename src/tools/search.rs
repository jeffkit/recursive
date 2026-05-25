//! `search_files`: substring/regex search across workspace files.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use walkdir::WalkDir;

use super::{resolve_within, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

const DEFAULT_MAX_RESULTS: usize = 50;
const DEFAULT_MAX_LINE_LEN: usize = 240;

#[derive(Debug, Clone)]
pub struct SearchFiles {
    pub root: PathBuf,
    pub max_results: usize,
}

impl SearchFiles {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_results: DEFAULT_MAX_RESULTS,
        }
    }
}

#[async_trait]
impl Tool for SearchFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_files".into(),
            description:
                "Find lines containing a pattern across files in the workspace. Returns up to N matches as 'path:line: text'."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern":   { "type": "string", "description": "Substring to search for (literal, case-sensitive)." },
                    "path":      { "type": "string", "description": "Optional subdirectory (workspace-relative) to scope the search to. Defaults to workspace root." },
                    "max_results": { "type": "integer", "description": "Cap on results (default 50, max 200)." }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let pattern = args["pattern"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "search_files".into(),
            message: "missing `pattern`".into(),
        })?;
        if pattern.is_empty() {
            return Err(Error::BadToolArgs {
                name: "search_files".into(),
                message: "`pattern` must not be empty".into(),
            });
        }

        let scope = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => resolve_within(&self.root, p).map_err(|e| Error::BadToolArgs {
                name: "search_files".into(),
                message: format!("path: {e}"),
            })?,
            None => self.root.clone(),
        };

        let cap = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(200))
            .unwrap_or(self.max_results);

        let mut hits: Vec<String> = Vec::new();
        'outer: for entry in WalkDir::new(&scope)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            // Skip obvious binaries / large files by name. Cheap heuristic.
            if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| {
                    matches!(
                        e,
                        "png" | "jpg" | "jpeg" | "gif" | "pdf" | "zip" | "gz" | "tar" | "bin"
                    )
                })
                .unwrap_or(false)
            {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(path) else {
                continue;
            };
            let rel = path.strip_prefix(&self.root).unwrap_or(path);
            for (line_no, line) in contents.lines().enumerate() {
                if line.contains(pattern) {
                    let truncated = if line.len() > DEFAULT_MAX_LINE_LEN {
                        format!("{}…", &line[..DEFAULT_MAX_LINE_LEN])
                    } else {
                        line.to_string()
                    };
                    hits.push(format!("{}:{}: {}", rel.display(), line_no + 1, truncated));
                    if hits.len() >= cap {
                        break 'outer;
                    }
                }
            }
        }

        if hits.is_empty() {
            Ok(format!("no matches for `{pattern}`"))
        } else {
            Ok(hits.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, body: &str) {
        std::fs::write(dir.path().join(name), body).unwrap();
    }

    #[tokio::test]
    async fn finds_matches_with_path_and_line_number() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "a.txt", "foo\nbar\nbaz");
        write(&tmp, "b.txt", "bar quux");
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "bar"}))
            .await
            .unwrap();
        assert!(out.contains("a.txt:2: bar"));
        assert!(out.contains("b.txt:1: bar quux"));
    }

    #[tokio::test]
    async fn empty_pattern_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let err = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": ""}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[tokio::test]
    async fn returns_no_match_message_when_empty() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "a.txt", "hello world");
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "zzzz"}))
            .await
            .unwrap();
        assert!(out.contains("no matches"));
    }

    #[tokio::test]
    async fn respects_max_results_cap() {
        let tmp = TempDir::new().unwrap();
        let body: String = (0..10).map(|_| "needle\n").collect();
        write(&tmp, "many.txt", &body);
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "needle", "max_results": 3}))
            .await
            .unwrap();
        assert_eq!(out.lines().count(), 3);
    }

    #[tokio::test]
    async fn path_argument_scopes_search() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        write(&tmp, "outside.txt", "hit");
        std::fs::write(tmp.path().join("sub/inside.txt"), "hit").unwrap();
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "hit", "path": "sub"}))
            .await
            .unwrap();
        assert!(out.contains("inside.txt"));
        assert!(!out.contains("outside.txt"));
    }
}
