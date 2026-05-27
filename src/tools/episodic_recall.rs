//! Episodic recall tool — search past session transcripts.
//!
//! Reads JSONL session transcripts from `<workspace>/.recursive/sessions/`
//! and allows the agent to search through them by content, role, or tool
//! call details. Returns matching entries with surrounding context.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::session::SessionReader;
use crate::tools::Tool;

use std::time::Duration;

/// Episodic recall tool: search past session transcripts.
pub struct EpisodicRecall {
    workspace: PathBuf,
}

impl EpisodicRecall {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

#[async_trait]
impl Tool for EpisodicRecall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "episodic_recall".into(),
            description: "Search past session transcripts for messages matching a query. Reads JSONL session files from the workspace's session directory. Returns matching entries with surrounding context.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query (case-insensitive substring match against message content, role, and tool call names/arguments)"
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional session ID to restrict the search to a single session. If omitted, searches all sessions."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of matching entries to return (default 5)",
                        "default": 5
                    },
                    "context_lines": {
                        "type": "integer",
                        "description": "Number of surrounding messages to include before and after each match (default 2)",
                        "default": 2
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn is_readonly(&self) -> bool {
        true
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let query = arguments["query"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "episodic_recall".into(),
                message: "missing required parameter: query".to_string(),
            })?;

        let session_id = arguments["session_id"].as_str().map(|s| s.to_string());
        let limit = arguments["limit"].as_i64().unwrap_or(5) as usize;
        let context_lines = arguments["context_lines"].as_i64().unwrap_or(2) as usize;

        // List all session directories
        let all_sessions = match SessionReader::list_sessions(&self.workspace) {
            Ok(sessions) => sessions,
            Err(e) => {
                return Ok(format!("failed to list sessions: {e}"));
            }
        };

        if all_sessions.is_empty() {
            return Ok("no past sessions found".to_string());
        }

        // Filter by session_id if provided
        let sessions: Vec<&PathBuf> = if let Some(ref sid) = session_id {
            all_sessions
                .iter()
                .filter(|dir| {
                    // Match the session directory name exactly (or as a suffix)
                    let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    dir_name == sid.as_str() || dir_name.ends_with(sid.as_str())
                })
                .collect()
        } else {
            all_sessions.iter().collect()
        };

        if sessions.is_empty() {
            return Ok(format!(
                "no sessions found matching '{}'",
                session_id.unwrap_or_default()
            ));
        }

        let query_lower = query.to_lowercase();
        let mut results: Vec<String> = Vec::new();

        for session_dir in sessions {
            let entries = match SessionReader::load_transcript(session_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            if entries.is_empty() {
                continue;
            }

            // Find matching indices
            let mut match_indices: Vec<usize> = Vec::new();
            for (i, entry) in entries.iter().enumerate() {
                // Search in role
                if entry.role.to_lowercase().contains(&query_lower) {
                    match_indices.push(i);
                    continue;
                }
                // Search in content
                if entry.content.to_lowercase().contains(&query_lower) {
                    match_indices.push(i);
                    continue;
                }
                // Search in tool_calls
                for tc in &entry.tool_calls {
                    if tc.name.to_lowercase().contains(&query_lower)
                        || tc
                            .arguments
                            .to_string()
                            .to_lowercase()
                            .contains(&query_lower)
                    {
                        match_indices.push(i);
                        break;
                    }
                }
            }

            if match_indices.is_empty() {
                continue;
            }

            // Deduplicate and sort match indices
            match_indices.sort();
            match_indices.dedup();

            // Build session header
            let session_name = session_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            results.push(format!("--- Session: {} ---", session_name));

            // Collect context windows around each match
            let mut rendered_indices: std::collections::BTreeSet<usize> =
                std::collections::BTreeSet::new();
            for &idx in &match_indices {
                let start = if idx >= context_lines {
                    idx - context_lines
                } else {
                    0
                };
                let end = (idx + context_lines + 1).min(entries.len());
                for i in start..end {
                    rendered_indices.insert(i);
                }
            }

            // Render the context window
            for &i in &rendered_indices {
                let entry = &entries[i];
                let marker = if match_indices.contains(&i) {
                    " >>> "
                } else {
                    "     "
                };
                let role_padded = format!("{:>9}", entry.role);
                let content_preview = if entry.content.len() > 200 {
                    format!("{}...", crate::truncate_str(&entry.content, 197))
                } else {
                    entry.content.clone()
                };
                let tool_info = if !entry.tool_calls.is_empty() {
                    let names: Vec<&str> =
                        entry.tool_calls.iter().map(|tc| tc.name.as_str()).collect();
                    format!(" [tool_calls: {}]", names.join(", "))
                } else {
                    String::new()
                };
                results.push(format!(
                    "{}[{}] {}{} {}",
                    marker, entry.id, role_padded, tool_info, content_preview
                ));
            }

            results.push(String::new()); // blank line between sessions
        }

        if results.is_empty() {
            return Ok(format!("no matches found for '{}'", query));
        }

        // Truncate to limit (limit applies to number of match entries, not context)
        // We'll limit the output by counting actual match entries rendered
        let mut output_lines: Vec<String> = Vec::new();
        let mut match_count = 0;
        for line in results {
            if line.starts_with(" >>> ") {
                match_count += 1;
                if match_count > limit {
                    output_lines.push(format!(
                        "... (truncated, showing {}/{} matches)",
                        limit, match_count
                    ));
                    break;
                }
            }
            output_lines.push(line);
        }

        Ok(output_lines.join("\n"))
    }
}

/// Build a summary of recent sessions for injection into the system prompt.
/// Returns the top N most recent session goals as a formatted block.
pub fn episodic_recall_summary(workspace: &std::path::Path, limit: usize) -> String {
    let sessions = match SessionReader::list_sessions(workspace) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };

    if sessions.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "# Recent Sessions (last {}; use `episodic_recall` to search)",
        limit
    ));

    // Sessions are sorted by name (timestamp-prefixed), so most recent are last
    let recent: Vec<&PathBuf> = sessions.iter().rev().take(limit).collect();
    for session_dir in recent {
        let meta = match SessionReader::load_meta(session_dir) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let session_name = session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let goal_preview = if meta.goal.len() > 80 {
            format!("{}...", crate::truncate_str(&meta.goal, 77))
        } else {
            meta.goal.clone()
        };
        lines.push(format!(
            "- {} ({} msgs, {}): {}",
            session_name, meta.message_count, meta.status, goal_preview
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::session::SessionWriter;

    /// Helper: create a temporary workspace and write some session data.
    fn setup_session_with_messages(
        ws: &std::path::Path,
        goal: &str,
        messages: &[(&str, &str)],
    ) -> String {
        let mut writer = SessionWriter::create(ws, goal, "test-model", "test-provider").unwrap();
        let session_dir = writer.session_dir().to_path_buf();
        for (role, content) in messages {
            let msg = match *role {
                "user" => Message::user(content.to_string()),
                "assistant" => Message::assistant(content.to_string()),
                "system" => Message::system(content.to_string()),
                _ => Message::user(content.to_string()),
            };
            writer.append(&msg).unwrap();
        }
        writer.finish("completed").unwrap();
        session_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn test_a_episodic_recall_finds_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        setup_session_with_messages(
            ws,
            "test goal",
            &[
                ("system", "You are a helpful assistant."),
                ("user", "Hello, how are you?"),
                ("assistant", "I'm doing great, thanks!"),
                ("user", "What is Rust?"),
                ("assistant", "Rust is a systems programming language."),
            ],
        );

        let tool = EpisodicRecall::new(ws);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"query": "Rust"})));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("Rust"), "output: {output}");
        assert!(output.contains("systems programming"), "output: {output}");
    }

    #[test]
    fn test_b_episodic_recall_with_session_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        // Create two sessions with different goals so we can distinguish them
        let sid1 = setup_session_with_messages(
            ws,
            "first-session-unique-goal",
            &[("user", "Hello from session 1")],
        );

        // Sleep 1 second to ensure different timestamp in session_id
        std::thread::sleep(Duration::from_secs(1));
        let sid2 = setup_session_with_messages(
            ws,
            "second-session-unique-goal",
            &[("user", "Hello from session 2")],
        );

        // Verify we got different session IDs
        assert_ne!(sid1, sid2, "session IDs should differ");

        let tool = EpisodicRecall::new(ws);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({
                "query": "Hello",
                "session_id": &sid2
            })));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("session 2"), "output: {output}");
        assert!(!output.contains("session 1"), "output: {output}");
    }

    #[test]
    fn test_c_episodic_recall_empty_when_no_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        let tool = EpisodicRecall::new(ws);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(json!({"query": "anything"})));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output, "no past sessions found");
    }

    #[test]
    fn test_d_episodic_recall_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        // No sessions yet
        let summary = episodic_recall_summary(ws, 5);
        assert_eq!(summary, "");

        // Create a session
        setup_session_with_messages(
            ws,
            "fix the bug in parser",
            &[("user", "Please fix the parser bug")],
        );

        let summary = episodic_recall_summary(ws, 5);
        assert!(!summary.is_empty(), "summary should not be empty");
        assert!(
            summary.contains("fix the bug in parser"),
            "summary: {summary}"
        );
        assert!(summary.contains("1 msgs"), "summary: {summary}");
    }
}
