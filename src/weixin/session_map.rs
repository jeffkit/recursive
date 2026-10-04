//! Per-user session state for the WeChat channel (issue #105 §3).
//!
//! Before this module every WeChat user shared the daemon's single
//! runtime session, and `/list` / `/c N` / `/reset` answered with
//! placeholder text. The multiplexer is now real:
//!
//! - **Mapping**: `user_id → session_id`, persisted as JSON under the
//!   workspace user dir (`weixin_sessions.json`, atomic write) so a
//!   daemon restart re-binds each user to their conversation.
//! - **`/list N`**: reads the last N turns of the user's session from
//!   disk via [`crate::session::SessionReader`] — the weixin daemon never
//!   needed its own transcript format because a weixin session IS a
//!   native session.
//! - **`/c N`**: rebinds the user to the Nth most-recent workspace
//!   session (same ordering as `recursive resume`).
//! - **`/reset`**: drops the binding; the next message starts a fresh
//!   session (the backend observes the `None` binding and creates one).
//!
//! The map is transport-free: the daemon passes in a workspace path and
//! gets plain data back. All disk layout reuses the session module.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Disk-backed `user_id → session_id` map for one workspace.
pub struct WeixinSessionMap {
    path: PathBuf,
}

/// One user's binding plus cached session metadata for listings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserBinding {
    pub session_id: String,
    /// RFC3339 timestamp of the last rebind (observability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_at: Option<String>,
}

impl UserBinding {
    fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            bound_at: Some(crate::session::chrono_lite_now()),
        }
    }
}

/// One rendered turn of `/list` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub role: String,
    /// Content truncated to the display budget (long tool results would
    /// otherwise flood a phone screen).
    pub content: String,
}

impl WeixinSessionMap {
    /// Default persistence path: `<user_workspace_dir>/weixin_sessions.json`.
    pub fn default_path(workspace: &Path) -> PathBuf {
        crate::paths::user_workspace_dir(workspace)
            .map(|d| d.join("weixin_sessions.json"))
            .unwrap_or_else(|_| PathBuf::from(".recursive").join("weixin_sessions.json"))
    }

    /// Store backed by an explicit path (tests); production uses
    /// [`WeixinSessionMap::for_workspace`].
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Store at the workspace's default location.
    pub fn for_workspace(workspace: &Path) -> Self {
        Self::new(Self::default_path(workspace))
    }

    /// Load all bindings. A missing file is an empty map (first run).
    pub fn load(&self) -> Result<HashMap<String, UserBinding>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HashMap::new());
            }
            Err(e) => return Err(Error::Io(e)),
        };
        serde_json::from_slice(&bytes).map_err(|e| Error::Config {
            message: format!("corrupt weixin session map {}: {e}", self.path.display()),
        })
    }

    /// Atomically persist the whole map.
    pub fn save(&self, bindings: &HashMap<String, UserBinding>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let json = serde_json::to_string_pretty(bindings).map_err(|e| Error::Config {
            message: format!("serialize weixin session map: {e}"),
        })?;
        crate::atomic::atomic_write(&self.path, json.as_bytes()).map_err(Error::Io)
    }

    /// The session a user is currently bound to (`None` = start fresh).
    pub fn session_of(&self, user_id: &str) -> Result<Option<String>> {
        Ok(self.load()?.get(user_id).map(|b| b.session_id.clone()))
    }

    /// Bind a user to a session (create or rebind).
    pub fn bind(&self, user_id: &str, session_id: &str) -> Result<()> {
        let mut all = self.load()?;
        all.insert(user_id.to_string(), UserBinding::new(session_id));
        self.save(&all)
    }

    /// Drop a user's binding (`/reset`). `false` when they had none.
    pub fn unbind(&self, user_id: &str) -> Result<bool> {
        let mut all = self.load()?;
        let removed = all.remove(user_id).is_some();
        if removed {
            self.save(&all)?;
        }
        Ok(removed)
    }

    /// Create a fresh on-disk session and bind `user_id` to it.
    ///
    /// This is the "no binding → start a new conversation" half of the
    /// multiplexer contract: the returned writer lets the caller keep
    /// appending the new conversation's turns, and the binding makes the
    /// id durable so the user's next message (and `/list`) finds it.
    pub fn create_bound_session(
        workspace: &Path,
        user_id: &str,
        goal: &str,
        model: &str,
        provider: &str,
        preset: Option<&str>,
    ) -> Result<crate::session::SessionWriter> {
        let writer = crate::session::SessionWriter::create_with_tools(
            workspace,
            goal,
            model,
            provider,
            &[],
            preset,
        )
        .map_err(Error::Io)?;
        Self::for_workspace(workspace).bind(user_id, writer.session_id())?;
        Ok(writer)
    }
}

/// Render the last `count` turns of a session as `/list` text.
///
/// Reads the session's **full on-disk history** (not the post-compaction
/// seed — the user asked for the conversation they remember, including
/// anything compaction folded away). Tool-result messages are rendered
/// compactly (`tool ▸ …`) and truncated so a phone screen survives.
pub fn render_session_tail(workspace: &Path, session_id: &str, count: usize) -> String {
    let dir = match find_session_dir(workspace, session_id) {
        Some(d) => d,
        None => return format!("会话 {session_id} 不存在。"),
    };
    let Ok(entries) = crate::session::SessionReader::load_full_history(&dir) else {
        return format!("会话 {session_id} 无法读取。");
    };
    let turns: Vec<TurnSummary> = entries
        .into_iter()
        .filter_map(|entry| match entry {
            crate::session::LoadedEntry::Message(boxed) => {
                let role = match boxed.role.as_str() {
                    "user" => "用户",
                    "assistant" => "助手",
                    "tool" => "工具",
                    _ => "系统",
                };
                let content: String = boxed.content.chars().take(200).collect();
                let content = if boxed.content.chars().count() > 200 {
                    format!("{content}…")
                } else {
                    content
                };
                Some(TurnSummary {
                    role: role.to_string(),
                    content,
                })
            }
            crate::session::LoadedEntry::CompactBoundary { removed, .. } => {
                // Surface compaction honestly instead of silently hiding it.
                (removed > 0).then(|| TurnSummary {
                    role: "系统".to_string(),
                    content: format!("（此处已压缩早期 {removed} 条历史）"),
                })
            }
        })
        .collect();
    if turns.is_empty() {
        return "该会话暂无对话记录。".to_string();
    }
    let start = turns.len().saturating_sub(count);
    let mut lines = vec![format!("📜 最近 {} 条对话：", turns.len() - start)];
    for t in &turns[start..] {
        lines.push(format!("[{}] {}", t.role, t.content));
    }
    lines.join("\n")
}

/// List the workspace's most recent sessions in `/c N` choice format.
///
/// Same ordering as `recursive resume` (`updated_at` desc). Returns
/// `(index, session_id, label)` triples; `index` is 1-based and
/// `session_id` is the **full** on-disk session id — [`resolve_change`]
/// binds it directly and [`find_session_dir`] matches it against the
/// session directory name, so a display-truncated id would never resolve.
/// Rendering truncates for the phone screen; the id itself must not be.
pub fn list_recent_sessions(workspace: &Path, limit: usize) -> Vec<(usize, String, String)> {
    let Ok(sessions) = crate::session::SessionReader::list_sessions_sorted_by_updated_at(workspace)
    else {
        return Vec::new();
    };
    sessions
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(i, (dir, meta))| {
            let label = meta
                .name
                .clone()
                .or_else(|| meta.first_prompt.clone())
                .unwrap_or_else(|| meta.goal.clone());
            let label: String = label.chars().take(40).collect();
            let _ = dir;
            (i + 1, meta.session_id.clone(), label)
        })
        .collect()
}

/// Render `/s` output (numbered list, matching `/c N` indices).
pub fn render_sessions_list(workspace: &Path) -> String {
    let sessions = list_recent_sessions(workspace, 10);
    if sessions.is_empty() {
        return "暂无会话记录。".to_string();
    }
    let mut lines = vec!["📋 会话列表：".to_string()];
    for (idx, session_id, label) in &sessions {
        let short: String = session_id.chars().take(12).collect();
        lines.push(format!("[{idx}] {short} — {label}"));
    }
    lines.push("发送 /c N 切换会话，/r 重置当前会话".to_string());
    lines.join("\n")
}

/// Resolve `/c N` against the recent-sessions list: `Ok(session_id)` or
/// `Err(user-facing error)`.
pub fn resolve_change(workspace: &Path, index: usize) -> std::result::Result<String, String> {
    let sessions = list_recent_sessions(workspace, 10);
    let total = sessions.len();
    sessions
        .into_iter()
        .find(|(i, _, _)| *i == index)
        .map(|(_, id, _)| id)
        .ok_or_else(|| format!("没有第 {index} 个会话。发送 /s 查看列表（1-{total}）。"))
}

/// Locate a session's directory by id (the reader lists directories;
/// this picks the one matching `session_id`).
pub fn find_session_dir(workspace: &Path, session_id: &str) -> Option<PathBuf> {
    let dirs = crate::session::SessionReader::list_sessions(workspace).ok()?;
    dirs.into_iter()
        .find(|d| d.file_name().and_then(|n| n.to_str()) == Some(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::IsolatedWorkspace;

    fn temp_map() -> (tempfile::TempDir, WeixinSessionMap) {
        let dir = tempfile::tempdir().expect("tempdir");
        let map = WeixinSessionMap::new(dir.path().join("weixin_sessions.json"));
        (dir, map)
    }

    #[test]
    fn missing_map_loads_empty() {
        let (_d, map) = temp_map();
        assert!(map.load().expect("load").is_empty());
        assert_eq!(map.session_of("alice").expect("lookup"), None);
    }

    #[test]
    fn bind_lookup_unbind_round_trip() {
        let (_d, map) = temp_map();
        map.bind("alice", "sess-a").expect("bind");
        assert_eq!(
            map.session_of("alice").expect("lookup"),
            Some("sess-a".to_string())
        );
        // Rebind wins.
        map.bind("alice", "sess-b").expect("rebind");
        assert_eq!(
            map.session_of("alice").expect("lookup"),
            Some("sess-b".to_string())
        );
        // Other users unaffected.
        map.bind("bob", "sess-c").expect("bind bob");
        assert_eq!(
            map.session_of("bob").expect("lookup"),
            Some("sess-c".to_string())
        );
        // Unbind.
        assert!(map.unbind("alice").expect("unbind"));
        assert_eq!(map.session_of("alice").expect("lookup"), None);
        assert!(
            !map.unbind("alice").expect("unbind again"),
            "double reset is a no-op"
        );
        assert!(
            map.session_of("bob").expect("lookup").is_some(),
            "bob's binding survives alice's reset"
        );
    }

    #[test]
    fn map_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("weixin_sessions.json");
        {
            let map = WeixinSessionMap::new(path.clone());
            map.bind("alice", "sess-a").expect("bind");
        }
        // Fresh instance over the same file.
        let map = WeixinSessionMap::new(path);
        assert_eq!(
            map.session_of("alice").expect("lookup"),
            Some("sess-a".to_string()),
            "binding is durable across daemon restarts"
        );
    }

    #[test]
    fn corrupt_map_is_an_error_not_silent_reset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("weixin_sessions.json");
        std::fs::write(&path, b"{{{").expect("write");
        let map = WeixinSessionMap::new(path);
        assert!(map.load().is_err(), "corruption must surface");
    }

    /// Rendering against a pinned `RECURSIVE_SESSIONS_DIR`: the env var is
    /// process-global, so `IsolatedWorkspace` holds the cross-module env
    /// lock for the whole test.
    #[test]
    fn session_listing_renders_against_pinned_sessions_dir() {
        let ws = IsolatedWorkspace::new();
        let dir = ws.path().to_path_buf();

        // Missing session → friendly message, no panic.
        let msg = render_session_tail(&dir, "sess-nope", 5);
        assert!(msg.contains("不存在"), "got: {msg}");

        // Empty workspace → empty-list message.
        let msg = render_sessions_list(&dir);
        assert!(msg.contains("暂无会话记录"), "got: {msg}");

        // Out-of-range /c → friendly error mentioning the bound.
        let err = resolve_change(&dir, 7).expect_err("no sessions at all");
        assert!(err.contains("没有第 7 个会话"), "got: {err}");
    }

    /// The `/r` → next-message flow: no binding means "start a fresh
    /// conversation", and that conversation must be real (on disk + bound)
    /// so the following message continues it and `/list` can read it.
    #[test]
    fn create_bound_session_makes_a_real_bound_session() {
        let ws = IsolatedWorkspace::new();
        let workspace = ws.path().to_path_buf();
        let writer = WeixinSessionMap::create_bound_session(
            &workspace,
            "alice",
            "第一条消息",
            "mock-model",
            "mock",
            None,
        )
        .expect("create + bind");
        let id = writer.session_id().to_string();
        assert_eq!(
            WeixinSessionMap::for_workspace(&workspace)
                .session_of("alice")
                .expect("lookup"),
            Some(id.clone()),
            "the creator is bound to the new session"
        );
        assert!(
            find_session_dir(&workspace, &id).is_some(),
            "the new session is on disk"
        );
    }

    /// The `/s` → `/c N` → `/list` flow must actually resolve: `/c N`
    /// binds the **full** session id, because `find_session_dir` compares
    /// the whole directory name. A display-truncated id silently broke
    /// this (every `/list` answered "会话 … 不存在").
    #[test]
    fn change_index_binds_the_full_session_id_and_list_finds_it() {
        let ws = IsolatedWorkspace::new();
        let workspace = ws.path().to_path_buf();
        let writer = crate::session::SessionWriter::create_with_tools(
            &workspace,
            "weixin goal",
            "mock-model",
            "mock",
            &[],
            None,
        )
        .expect("create session");
        let full_id = writer.session_id().to_string();
        drop(writer);

        // /s lists it, displaying only the short prefix.
        let listing = render_sessions_list(&workspace);
        assert!(listing.contains("[1]"), "got: {listing}");
        let short: String = full_id.chars().take(12).collect();
        assert!(
            listing.contains(&short),
            "listing shows the short id: {listing}"
        );

        // /c 1 hands back the full id (not the 12-char display form).
        let resolved = resolve_change(&workspace, 1).expect("index 1 resolves");
        assert_eq!(resolved, full_id, "/c N must bind the full on-disk id");
        assert!(
            full_id.len() > 12,
            "real ids exceed the display budget: {full_id}"
        );

        // /list on that binding reaches the on-disk session.
        let tail = render_session_tail(&workspace, &resolved, 5);
        assert!(
            !tail.contains("不存在"),
            "resolved id must find its directory: {tail}"
        );
        assert!(
            find_session_dir(&workspace, &resolved).is_some(),
            "find_session_dir must match the full id"
        );
    }
}
