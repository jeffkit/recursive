//! Session query tools (issue #131, borrowed from DSH `tool-session-query`).
//!
//! Five read-only tools expose the derived [`SessionIndex`] to the model:
//!
//! | tool | question it answers |
//! |------|--------------------|
//! | `session_search` | which past session was about X? |
//! | `session_event_search` | which past *message* mentioned X? |
//! | `session_trace` | how did this session's history get compacted? |
//! | `session_event_trace` | what replaced this message / what did it replace? |
//! | `session_event_read` | show me that message with context |
//!
//! **Workspace boundary.** Cross-session search only ever looks inside the
//! bounded workspace the calling session runs in. Every tool takes an optional
//! `cwd`; an explicit `cwd` outside the caller's workspace is refused
//! (`Error::ToolRejected`) rather than silently narrowed — a caller that asked
//! for another project's history must be told no, not handed its own. The
//! parameter is a *claim to check*, not a filter: the index only ever covers
//! one workspace, so a `cwd` at or below it does not narrow the results.
//! Results are capped at [`MAX_SEARCH_RESULTS`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::session::index::{SessionIndex, MAX_SEARCH_RESULTS};
use crate::session::relations::{trace_event, trace_session, Replacement, SessionTrace};
use crate::tools::Tool;

/// Longest transcript entry returned by `session_event_read`, in characters.
/// A tool result is a window into history, not the history itself.
pub const MAX_EVENT_CHARS: usize = 4000;

/// Default number of hits a search returns.
const DEFAULT_LIMIT: usize = 10;

/// Lazily-opened session index shared by the whole tool family: one SQLite
/// connection and one cold-read cache for all five tools.
pub struct SessionQuery {
    workspace: PathBuf,
    index: Mutex<Option<SessionIndex>>,
}

impl SessionQuery {
    /// Build a query surface bound to `workspace`.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            index: Mutex::new(None),
        }
    }

    /// Workspace this surface is allowed to read.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Run `f` against a freshly-opened (or already open) index.
    ///
    /// The index is opened once per process and refreshed on every call: a
    /// refresh only stats the session directories, so "search always sees the
    /// latest sessions" stays cheap.
    fn with_index<R>(&self, f: impl FnOnce(&mut SessionIndex) -> Result<R>) -> Result<R> {
        let mut guard = self.index.lock().map_err(|_| Error::Storage {
            message: "session index lock poisoned".to_string(),
        })?;
        if guard.is_none() {
            *guard = Some(SessionIndex::open(&self.workspace)?);
        }
        let Some(index) = guard.as_mut() else {
            return Err(Error::Storage {
                message: "session index unavailable".to_string(),
            });
        };
        index.refresh()?;
        f(index)
    }

    /// Refuse a `cwd` that is not the caller's workspace or below it.
    fn authorize(&self, tool: &str, cwd: Option<&str>) -> Result<()> {
        let Some(requested) = cwd else {
            return Ok(());
        };
        let caller = normalize(&self.workspace);
        let requested = normalize(Path::new(requested));
        if requested == caller || requested.starts_with(&caller) {
            return Ok(());
        }
        Err(Error::ToolRejected {
            name: tool.to_string(),
            reason: format!(
                "session scope '{}' is outside this session's workspace '{}'",
                requested.display(),
                caller.display()
            ),
        })
    }
}

/// Resolve a path for comparison: canonical when it exists, lexically
/// absolutised otherwise (the requested directory may be gone, and a `..`
/// component must not be allowed to climb back into the workspace's parent).
fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn limit_from(args: &Value, default: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .map(|n| n as usize)
        .unwrap_or(default)
        .min(MAX_SEARCH_RESULTS)
}

fn require_str<'a>(tool: &str, args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::BadToolArgs {
            name: tool.to_string(),
            message: format!("missing required parameter: {key}"),
        })
}

fn cwd_arg(args: &Value) -> Option<&str> {
    args.get("cwd").and_then(Value::as_str)
}

fn to_json(value: &Value) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(Error::from)
}

fn replacement_json(r: &Replacement) -> Value {
    json!({
        "summary_index": r.summary_index,
        "first_replaced": r.first_replaced,
        "replaced": r.replaced,
        "turn": r.turn,
    })
}

fn trace_json(trace: &SessionTrace) -> Value {
    json!({
        "session_id": trace.session_id,
        "goal": trace.goal,
        "status": trace.status,
        "message_count": trace.message_count,
        "replacements": trace.replacements.iter().map(replacement_json).collect::<Vec<_>>(),
        "same_origin": trace.same_origin,
    })
}

/// `session_search` — find past sessions by goal or display name.
pub struct SessionSearch {
    query: Arc<SessionQuery>,
}

impl SessionSearch {
    pub fn new(query: Arc<SessionQuery>) -> Self {
        Self { query }
    }
}

#[async_trait]
impl Tool for SessionSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "session_search".into(),
            description: "Search past sessions in THIS workspace by goal or name. \
                          Returns session ids and headers; use session_event_search to \
                          search message contents. The workspace boundary is enforced: \
                          a `cwd` outside the calling session's workspace is refused."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Text to look for in session goals and names"},
                    "limit": {"type": "integer", "description": "Maximum sessions to return (default 10, max 100)", "default": 10},
                    "cwd": {"type": "string", "description": "Optional directory the caller claims to be searching from. It is only checked against the workspace boundary — a directory outside the calling session's workspace is refused, and a directory inside it does not narrow the results (the index covers the whole workspace)."}
                },
                "required": ["query"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let query = require_str("session_search", &arguments, "query")?;
        self.query
            .authorize("session_search", cwd_arg(&arguments))?;
        let limit = limit_from(&arguments, DEFAULT_LIMIT);
        let hits = self
            .query
            .with_index(|index| index.search_sessions(query, limit))?;
        let sessions: Vec<Value> = hits
            .iter()
            .map(|hit| {
                json!({
                    "session_id": hit.session_id,
                    "goal": hit.goal,
                    "name": hit.name,
                    "model": hit.model,
                    "status": hit.status,
                    "created_at": hit.created_at,
                    "updated_at": hit.updated_at,
                    "message_count": hit.message_count,
                })
            })
            .collect();
        to_json(&json!({"query": query, "sessions": sessions}))
    }
}

/// `session_event_search` — full-text search over transcript entries.
pub struct SessionEventSearch {
    query: Arc<SessionQuery>,
}

impl SessionEventSearch {
    pub fn new(query: Arc<SessionQuery>) -> Self {
        Self { query }
    }
}

#[async_trait]
impl Tool for SessionEventSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "session_event_search".into(),
            description: "Full-text search over the transcript entries of past sessions in \
                          THIS workspace (message content and tool names). Returns a snippet \
                          per hit; use session_event_read to see one in full. Two-character \
                          queries (e.g. Chinese words) fall back to a substring match."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Text to look for in transcript entries"},
                    "session_id": {"type": "string", "description": "Optional session id to restrict the search to one session"},
                    "limit": {"type": "integer", "description": "Maximum hits to return (default 10, max 100)", "default": 10},
                    "cwd": {"type": "string", "description": "Optional directory the caller claims to be searching from. It is only checked against the workspace boundary — a directory outside the calling session's workspace is refused, and a directory inside it does not narrow the results (the index covers the whole workspace)."}
                },
                "required": ["query"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let query = require_str("session_event_search", &arguments, "query")?;
        self.query
            .authorize("session_event_search", cwd_arg(&arguments))?;
        let limit = limit_from(&arguments, DEFAULT_LIMIT);
        let session_id = arguments.get("session_id").and_then(Value::as_str);
        let hits = self
            .query
            .with_index(|index| index.search_events(query, limit, session_id))?;
        let events: Vec<Value> = hits
            .iter()
            .map(|hit| {
                json!({
                    "session_id": hit.session_id,
                    "index": hit.index,
                    "role": hit.role,
                    "tool": hit.tool_name,
                    "timestamp": hit.timestamp,
                    "snippet": hit.snippet,
                    "active": hit.active,
                })
            })
            .collect();
        to_json(&json!({"query": query, "hits": events}))
    }
}

/// `session_trace` — the relation picture of one session.
pub struct SessionTraceTool {
    query: Arc<SessionQuery>,
}

impl SessionTraceTool {
    pub fn new(query: Arc<SessionQuery>) -> Self {
        Self { query }
    }
}

#[async_trait]
impl Tool for SessionTraceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "session_trace".into(),
            description: "Explain how a past session's history came to look the way it does: \
                          every compaction that replaced older messages with a summary, plus \
                          sessions that start from the same origin transcript."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "Session id (from session_search)"}
                },
                "required": ["session_id"]
            }),
        }
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let session_id = require_str("session_trace", &arguments, "session_id")?;
        let trace = self
            .query
            .with_index(|index| trace_session(index, session_id))?;
        to_json(&trace_json(&trace))
    }
}

/// `session_event_trace` — relations of one transcript entry.
pub struct SessionEventTraceTool {
    query: Arc<SessionQuery>,
}

impl SessionEventTraceTool {
    pub fn new(query: Arc<SessionQuery>) -> Self {
        Self { query }
    }
}

#[async_trait]
impl Tool for SessionEventTraceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "session_event_trace".into(),
            description: "For one transcript entry (by session id + index): which later \
                          compaction summary replaced it, and — when the entry is itself a \
                          summary — which earlier messages it replaced."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "Session id"},
                    "index": {"type": "integer", "description": "Entry index within the session (0-based, as reported by session_event_search)"}
                },
                "required": ["session_id", "index"]
            }),
        }
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let session_id = require_str("session_event_trace", &arguments, "session_id")?;
        let index = arguments
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::BadToolArgs {
                name: "session_event_trace".to_string(),
                message: "missing required parameter: index".to_string(),
            })?;
        let trace = self
            .query
            .with_index(|index_handle| trace_event(index_handle, session_id, index as usize))?;
        to_json(&json!({
            "session_id": trace.session_id,
            "index": trace.index,
            "entry_id": trace.entry_id,
            "role": trace.role,
            "superseded_by": trace.superseded_by.as_ref().map(replacement_json),
            "supersedes": trace.supersedes.as_ref().map(replacement_json),
        }))
    }
}

/// `session_event_read` — read one entry (plus context) in full.
pub struct SessionEventRead {
    query: Arc<SessionQuery>,
}

impl SessionEventRead {
    pub fn new(query: Arc<SessionQuery>) -> Self {
        Self { query }
    }
}

#[async_trait]
impl Tool for SessionEventRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "session_event_read".into(),
            description: "Read transcript entries around one index of a past session. Long \
                          entries are truncated; use this after session_event_search to see \
                          the match in context."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "Session id"},
                    "index": {"type": "integer", "description": "Entry index to centre on (0-based)"},
                    "context_lines": {"type": "integer", "description": "Entries to include before and after the index (default 2)", "default": 2}
                },
                "required": ["session_id", "index"]
            }),
        }
    }

    fn is_deferred(&self) -> bool {
        true
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let session_id = require_str("session_event_read", &arguments, "session_id")?;
        let index = arguments
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::BadToolArgs {
                name: "session_event_read".to_string(),
                message: "missing required parameter: index".to_string(),
            })? as usize;
        let context = arguments
            .get("context_lines")
            .and_then(Value::as_i64)
            .filter(|n| *n >= 0)
            .unwrap_or(2) as usize;

        let events = self.query.with_index(|handle| {
            let total = handle.event_count(session_id)?;
            if total == 0 || index >= total {
                return Err(Error::NotFound(format!(
                    "event {index} of session {session_id}"
                )));
            }
            let from = index.saturating_sub(context);
            let to = (index + context).min(total.saturating_sub(1));
            handle.read_events(session_id, from, to)
        })?;

        let entries: Vec<Value> = events
            .iter()
            .map(|entry| {
                json!({
                    "index": entry.index,
                    "role": entry.role,
                    "tool": entry.tool_name,
                    "timestamp": entry.timestamp,
                    "content": truncate(&entry.content),
                })
            })
            .collect();
        to_json(&json!({
            "session_id": session_id,
            "index": index,
            "entries": entries,
        }))
    }
}

/// Truncate long entry content on a character boundary.
fn truncate(content: &str) -> String {
    if content.chars().count() <= MAX_EVENT_CHARS {
        return content.to_string();
    }
    let mut out: String = content.chars().take(MAX_EVENT_CHARS).collect();
    out.push_str("\n…[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::session::{SessionStatus, SessionWriter};
    use crate::test_util::IsolatedWorkspace;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn query_for(ws: &Path) -> Arc<SessionQuery> {
        Arc::new(SessionQuery::new(ws))
    }

    fn write_session(ws: &Path, goal: &str, user: &str, assistant: &str) -> String {
        let mut writer = SessionWriter::create(ws, goal, "deepseek-chat", "deepseek").unwrap();
        writer
            .append(&Message::user(user.to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::assistant(assistant.to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        writer.session_id().to_string()
    }

    #[test]
    fn session_search_finds_sessions_by_goal() {
        let env = IsolatedWorkspace::new();
        let wanted = write_session(env.path(), "refactor the session index", "a", "b");
        write_session(env.path(), "unrelated", "c", "d");
        let tool = SessionSearch::new(query_for(env.path()));

        let out = block_on(tool.execute(json!({"query": "session index"}))).unwrap();
        assert!(out.contains(&wanted), "output: {out}");
        assert!(!out.contains("unrelated"), "output: {out}");
    }

    #[test]
    fn session_event_search_reports_hits_with_snippets() {
        let env = IsolatedWorkspace::new();
        let wanted = write_session(env.path(), "goal", "please fix the parser", "done");
        let tool = SessionEventSearch::new(query_for(env.path()));

        let out = block_on(tool.execute(json!({"query": "parser"}))).unwrap();
        assert!(out.contains(&wanted), "output: {out}");
        assert!(out.contains("please fix the parser"), "output: {out}");
        assert!(out.contains("\"role\": \"user\""), "output: {out}");
    }

    #[test]
    fn session_event_search_honours_session_filter_and_limit() {
        let env = IsolatedWorkspace::new();
        let first = write_session(env.path(), "one", "needle one", "reply");
        write_session(env.path(), "two", "needle two", "reply");
        let tool = SessionEventSearch::new(query_for(env.path()));

        let out = block_on(tool.execute(json!({"query": "needle", "session_id": first}))).unwrap();
        assert!(out.contains("needle one"), "output: {out}");
        assert!(!out.contains("needle two"), "output: {out}");

        let capped = block_on(tool.execute(json!({"query": "needle", "limit": 1}))).unwrap();
        let hits: Value = serde_json::from_str(&capped).unwrap();
        assert_eq!(hits["hits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn cwd_outside_the_workspace_is_refused() {
        let env = IsolatedWorkspace::new();
        write_session(env.path(), "goal", "body", "reply");
        let outside = env.path().join("..").join("somewhere-else");
        let outside = outside.to_string_lossy().to_string();

        let search = SessionEventSearch::new(query_for(env.path()));
        let err = block_on(search.execute(json!({"query": "body", "cwd": outside}))).unwrap_err();
        assert!(matches!(err, Error::ToolRejected { .. }), "got {err:?}");

        let session_search = SessionSearch::new(query_for(env.path()));
        let err =
            block_on(session_search.execute(json!({"query": "goal", "cwd": outside}))).unwrap_err();
        assert!(matches!(err, Error::ToolRejected { .. }), "got {err:?}");
    }

    #[test]
    fn cwd_at_or_below_the_workspace_is_allowed() {
        let env = IsolatedWorkspace::new();
        write_session(env.path(), "goal", "needle body", "reply");
        let tool = SessionEventSearch::new(query_for(env.path()));

        let here = block_on(tool.execute(json!({"query": "needle", "cwd": env.path()}))).unwrap();
        assert!(here.contains("needle body"), "output: {here}");

        let below = env.path().join("sub").join("dir");
        std::fs::create_dir_all(&below).unwrap();
        let nested =
            block_on(tool.execute(json!({"query": "needle", "cwd": below.to_string_lossy()})))
                .unwrap();
        assert!(nested.contains("needle body"), "output: {nested}");
    }

    #[test]
    fn session_trace_explains_a_compaction() {
        let env = IsolatedWorkspace::new();
        let mut writer = SessionWriter::create(env.path(), "compacted", "m", "p").unwrap();
        for msg in [
            Message::user("u0".to_string()),
            Message::assistant("a0".to_string()),
            Message::user("u1".to_string()),
            Message::assistant("a1".to_string()),
        ] {
            writer.append(&msg, None, None).unwrap();
        }
        writer.write_compact_boundary(2, 3, None).unwrap();
        writer
            .append(&Message::user("[summary]".to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        let session_id = writer.session_id().to_string();

        let tool = SessionTraceTool::new(query_for(env.path()));
        let out = block_on(tool.execute(json!({"session_id": session_id}))).unwrap();
        let trace: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(trace["goal"], "compacted");
        assert_eq!(trace["message_count"], 5);
        assert_eq!(trace["replacements"][0]["replaced"], 3);
        assert_eq!(trace["replacements"][0]["first_replaced"], 0);
        assert_eq!(trace["replacements"][0]["summary_index"], 4);
        assert_eq!(trace["replacements"][0]["turn"], 2);
    }

    #[test]
    fn session_event_trace_reports_the_replacement() {
        let env = IsolatedWorkspace::new();
        let mut writer = SessionWriter::create(env.path(), "compacted", "m", "p").unwrap();
        for msg in [
            Message::user("u0".to_string()),
            Message::assistant("a0".to_string()),
            Message::user("u1".to_string()),
        ] {
            writer.append(&msg, None, None).unwrap();
        }
        writer.write_compact_boundary(1, 2, None).unwrap();
        writer
            .append(&Message::user("[summary]".to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        let session_id = writer.session_id().to_string();

        let tool = SessionEventTraceTool::new(query_for(env.path()));
        let out = block_on(tool.execute(json!({"session_id": session_id, "index": 1}))).unwrap();
        let trace: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(trace["superseded_by"]["first_replaced"], 0);
        assert_eq!(trace["superseded_by"]["summary_index"], 3);
        assert_eq!(trace["supersedes"], Value::Null);

        // The message the compaction kept verbatim is not reported as folded.
        let kept = block_on(tool.execute(json!({"session_id": session_id, "index": 2}))).unwrap();
        let trace: Value = serde_json::from_str(&kept).unwrap();
        assert_eq!(trace["superseded_by"], Value::Null);

        let summary =
            block_on(tool.execute(json!({"session_id": session_id, "index": 3}))).unwrap();
        let trace: Value = serde_json::from_str(&summary).unwrap();
        assert_eq!(trace["supersedes"]["first_replaced"], 0);
    }

    #[test]
    fn session_event_read_returns_context_and_truncates() {
        let env = IsolatedWorkspace::new();
        let mut writer = SessionWriter::create(env.path(), "goal", "m", "p").unwrap();
        writer
            .append(&Message::user("first".to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::assistant("second".to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::user("x".repeat(MAX_EVENT_CHARS + 50)), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        let session_id = writer.session_id().to_string();

        let tool = SessionEventRead::new(query_for(env.path()));
        let out = block_on(tool.execute(json!({"session_id": session_id, "index": 1}))).unwrap();
        let read: Value = serde_json::from_str(&out).unwrap();
        let entries = read["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3, "one before, the entry, one after");
        assert_eq!(entries[0]["content"], "first");
        assert!(entries[2]["content"]
            .as_str()
            .unwrap()
            .ends_with("…[truncated]"));
    }

    #[test]
    fn unknown_session_and_event_are_reported_as_errors() {
        let env = IsolatedWorkspace::new();
        let session_id = write_session(env.path(), "goal", "body", "reply");

        let read = SessionEventRead::new(query_for(env.path()));
        let err =
            block_on(read.execute(json!({"session_id": session_id, "index": 99}))).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "got {err:?}");

        let err = block_on(read.execute(json!({"session_id": "missing", "index": 0}))).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "got {err:?}");
    }

    #[test]
    fn missing_arguments_are_bad_tool_args() {
        let env = IsolatedWorkspace::new();
        let search = SessionSearch::new(query_for(env.path()));
        let err = block_on(search.execute(json!({}))).unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }), "got {err:?}");

        let read = SessionEventRead::new(query_for(env.path()));
        let err = block_on(read.execute(json!({"session_id": "x"}))).unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }), "got {err:?}");

        let trace = SessionEventTraceTool::new(query_for(env.path()));
        let err = block_on(trace.execute(json!({"session_id": "x"}))).unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }), "got {err:?}");
    }

    #[test]
    fn tool_metadata_is_read_only_and_sane() {
        let env = IsolatedWorkspace::new();
        let query = query_for(env.path());
        let search = SessionSearch::new(Arc::clone(&query));
        assert_eq!(search.spec().name, "session_search");
        assert!(!search.is_deferred());
        assert!(search.is_readonly());
        let event = SessionEventSearch::new(Arc::clone(&query));
        assert_eq!(event.spec().name, "session_event_search");
        assert!(!event.is_deferred());
        let trace = SessionTraceTool::new(Arc::clone(&query));
        assert_eq!(trace.spec().name, "session_trace");
        assert!(trace.is_deferred());
        let event_trace = SessionEventTraceTool::new(Arc::clone(&query));
        assert_eq!(event_trace.spec().name, "session_event_trace");
        assert!(event_trace.is_deferred());
        let read = SessionEventRead::new(Arc::clone(&query));
        assert_eq!(read.spec().name, "session_event_read");
        assert!(read.is_deferred());
        assert_eq!(query.workspace(), env.path());
    }

    #[test]
    fn normalize_falls_back_to_lexical_for_missing_paths() {
        // The input has to be *platform-absolute*: a POSIX `/…` literal is
        // rooted but drive-less on Windows, so `normalize` would join it onto
        // the current drive (`D:\definitely\…`) instead of passing it through.
        let (missing, with_parent, resolved) = if cfg!(windows) {
            (
                r"D:\definitely\does\not\exist\anywhere",
                r"D:\definitely\does\..\elsewhere",
                r"D:\definitely\elsewhere",
            )
        } else {
            (
                "/definitely/does/not/exist/anywhere",
                "/definitely/does/../elsewhere",
                "/definitely/elsewhere",
            )
        };
        assert_eq!(
            normalize(Path::new(missing)),
            PathBuf::from(missing),
            "a missing absolute path must be passed through lexically"
        );
        // A `..` in a non-existent path is resolved lexically, so it cannot
        // smuggle the caller out of the workspace.
        assert_eq!(
            normalize(Path::new(with_parent)),
            PathBuf::from(resolved),
            "`..` in a missing absolute path must be resolved lexically"
        );
    }

    #[test]
    fn truncate_leaves_short_content_alone() {
        assert_eq!(truncate("short"), "short");
        assert_eq!(truncate("").len(), 0);
    }

    #[test]
    fn limit_from_defaults_and_clamps() {
        assert_eq!(limit_from(&json!({}), DEFAULT_LIMIT), DEFAULT_LIMIT);
        assert_eq!(
            limit_from(&json!({"limit": 0}), DEFAULT_LIMIT),
            DEFAULT_LIMIT
        );
        assert_eq!(
            limit_from(&json!({"limit": -3}), DEFAULT_LIMIT),
            DEFAULT_LIMIT
        );
        assert_eq!(limit_from(&json!({"limit": 5}), DEFAULT_LIMIT), 5);
        assert_eq!(
            limit_from(&json!({"limit": 9999}), DEFAULT_LIMIT),
            MAX_SEARCH_RESULTS
        );
    }
}
