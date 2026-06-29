//! `install_skill`: search skillhub.cn for a skill, let the user review it
//! in the TUI, and install it to `~/.recursive/skills/<slug>/`.
//!
//! This tool is **TUI-only**.  When called without a TUI side-channel
//! (`skill_tx.is_none()`) it returns an informative error rather than
//! blocking forever.
//!
//! ## Communication flow
//!
//! ```text
//! Agent calls install_skill(query)
//!   → HTTP GET skillhub.cn/search?q=…
//!   → sends SkillInstallEvent::Search to TUI side-channel
//!   → awaits oneshot reply (selected slug or None)
//! TUI shows results modal, user picks a skill or presses Esc
//!   → oneshot reply: Some(slug) or None
//! If slug: HTTP GET skillhub.cn/download?slug=…  (zip)
//!   → parse zip → collect SkillZipFile list
//!   → sends SkillInstallEvent::Files to TUI side-channel
//!   → awaits oneshot reply (bool: confirm)
//! TUI shows files preview; user presses y or Esc
//!   → oneshot reply: true or false
//! If confirmed: extract zip to ~/.recursive/skills/<slug>/
//! ```

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::Tool;

// ── Skill-hub install side-channel types ─────────────────────────────────────

/// One search result returned by `skillhub.cn`.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillSearchResult {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub downloads: u64,
    pub stars: u32,
    pub version: String,
}

/// A single file from inside a skill zip archive.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillZipFile {
    /// Relative path inside the archive, e.g. `"pdf/SKILL.md"`.
    pub path: String,
    /// UTF-8 text content; binary files are represented as `"<binary>"`.
    pub content: String,
    /// Original file size in bytes (0 for directories).
    pub size: usize,
}

/// Phase 1: tool → TUI. User selects a slug or cancels.
///
/// `reply` carries `Some(slug)` if the user confirmed a choice, or `None`
/// to cancel. Not `PartialEq` because `oneshot::Sender` is not `PartialEq`.
pub struct SkillSearchRequest {
    pub query: String,
    pub results: Vec<SkillSearchResult>,
    pub reply: tokio::sync::oneshot::Sender<Option<String>>,
}

/// Phase 2: tool → TUI. User reviews files and confirms installation.
///
/// `reply` carries `true` to install, `false` to cancel.
pub struct SkillFilesRequest {
    pub slug: String,
    pub files: Vec<SkillZipFile>,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// Events from the `install_skill` tool to the TUI side-channel.
/// Carried on a dedicated `mpsc::UnboundedReceiver<SkillInstallEvent>` in
/// the TUI backend, separate from `event_rx`, because the payloads contain
/// `oneshot::Sender` values which are not `PartialEq`.
pub enum SkillInstallEvent {
    Search(SkillSearchRequest),
    Files(SkillFilesRequest),
}

/// skillhub.cn base URL.
const SKILLHUB_BASE: &str = "https://api.skillhub.cn/api/v1";
/// CDN fallback for downloads.
const SKILLHUB_CDN: &str = "https://skillhub-1388575217.cos.ap-guangzhou.myqcloud.com/skills";
/// How many results to request from the search API.
const SEARCH_LIMIT: usize = 8;
/// Request timeout for all skillhub.cn calls.
const HTTP_TIMEOUT_SECS: u64 = 30;

// ── Serde types for the skillhub.cn search API ───────────────────────────────

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    slug: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    stars: u32,
    #[serde(default)]
    version: String,
}

impl From<SearchItem> for SkillSearchResult {
    fn from(item: SearchItem) -> Self {
        SkillSearchResult {
            slug: item.slug,
            name: item.name,
            description: item.description,
            downloads: item.downloads,
            stars: item.stars,
            version: item.version,
        }
    }
}

// ── Tool struct ───────────────────────────────────────────────────────────────

pub struct InstallSkill {
    /// Present only when running inside the TUI.  `None` in headless mode.
    skill_tx: Option<Arc<mpsc::UnboundedSender<SkillInstallEvent>>>,
}

impl InstallSkill {
    pub fn new(skill_tx: Option<mpsc::UnboundedSender<SkillInstallEvent>>) -> Self {
        Self {
            skill_tx: skill_tx.map(Arc::new),
        }
    }

    /// Build a reqwest client with sensible timeouts.
    fn http_client() -> reqwest::Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("recursive-agent/install_skill")
            .build()
    }

    /// Call skillhub.cn search API and return parsed results.
    async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<SkillSearchResult>> {
        let url = format!("{SKILLHUB_BASE}/search");
        let url = format!("{url}?q={}&limit={SEARCH_LIMIT}", query.replace(' ', "+"));
        let resp = client.get(&url).send().await.map_err(|e| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: format!("skillhub.cn search request failed: {e}"),
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(Error::Tool {
                name: "install_skill".into(),
                call_id: None,
                message: format!("skillhub.cn search returned HTTP {status}"),
            });
        }

        let body: SearchResponse = resp.json().await.map_err(|e| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: format!("failed to parse skillhub.cn search response: {e}"),
        })?;

        Ok(body.results.into_iter().map(Into::into).collect())
    }

    /// Download the skill zip from skillhub.cn (with CDN fallback).
    async fn download_zip(client: &reqwest::Client, slug: &str) -> Result<Vec<u8>> {
        let primary_url = format!("{SKILLHUB_BASE}/download?slug={slug}");
        let fallback_url = format!("{SKILLHUB_CDN}/{slug}.zip");

        match Self::fetch_bytes(client, &primary_url).await {
            Ok(b) => Ok(b),
            Err(_) => Self::fetch_bytes(client, &fallback_url)
                .await
                .map_err(|e| Error::Tool {
                    name: "install_skill".into(),
                    call_id: None,
                    message: format!("failed to download skill '{slug}': {e}"),
                }),
        }
    }

    async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
        let resp = client.get(url).send().await.map_err(|e| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: e.to_string(),
        })?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(Error::Tool {
                name: "install_skill".into(),
                call_id: None,
                message: format!("HTTP {status} for {url}"),
            });
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| Error::Tool {
                name: "install_skill".into(),
                call_id: None,
                message: e.to_string(),
            })
    }

    /// Parse a zip archive in memory and return a flat list of (path, content) pairs.
    fn parse_zip(data: &[u8]) -> Result<Vec<SkillZipFile>> {
        let cursor = Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor).map_err(|e| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: format!("failed to open zip archive: {e}"),
        })?;

        let mut files = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| Error::Tool {
                name: "install_skill".into(),
                call_id: None,
                message: format!("failed to read zip entry {i}: {e}"),
            })?;

            if entry.is_dir() {
                continue;
            }

            let path = entry.name().to_string();
            let size = entry.size() as usize;

            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| Error::Tool {
                name: "install_skill".into(),
                call_id: None,
                message: format!("failed to read zip entry '{path}': {e}"),
            })?;

            let content = String::from_utf8(buf).unwrap_or_else(|_| "<binary>".to_string());

            files.push(SkillZipFile {
                path,
                content,
                size,
            });
        }

        // Sort by path for deterministic display.
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// Extract a zip archive (already loaded in memory) to `dest_dir`.
    fn extract_zip(data: &[u8], dest_dir: &std::path::Path) -> Result<()> {
        let cursor = Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor).map_err(|e| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: format!("failed to open zip for extraction: {e}"),
        })?;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| Error::Tool {
                name: "install_skill".into(),
                call_id: None,
                message: format!("zip entry {i} read error: {e}"),
            })?;

            // Strip the leading slug-directory component (e.g. "pdf/SKILL.md" → "SKILL.md")
            let raw_name = entry.name().to_string();
            let relative = raw_name
                .split_once('/')
                .map(|(_, rest)| rest)
                .unwrap_or(&raw_name);

            let out_path = dest_dir.join(relative);

            if entry.is_dir() {
                std::fs::create_dir_all(&out_path).map_err(|e| Error::Tool {
                    name: "install_skill".into(),
                    call_id: None,
                    message: format!("failed to create dir '{}: {e}", out_path.display()),
                })?;
                continue;
            }

            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::Tool {
                    name: "install_skill".into(),
                    call_id: None,
                    message: format!("mkdir '{}': {e}", parent.display()),
                })?;
            }

            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| Error::Tool {
                name: "install_skill".into(),
                call_id: None,
                message: format!("read zip entry '{raw_name}': {e}"),
            })?;

            std::fs::write(&out_path, &buf).map_err(|e| Error::Tool {
                name: "install_skill".into(),
                call_id: None,
                message: format!("write '{}': {e}", out_path.display()),
            })?;
        }
        Ok(())
    }

    /// Resolve the install directory: `~/.recursive/skills/<slug>/`.
    fn install_dir(slug: &str) -> Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: "cannot determine home directory".to_string(),
        })?;
        Ok(home.join(".recursive").join("skills").join(slug))
    }
}

#[async_trait]
impl Tool for InstallSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "install_skill".into(),
            description: "Search skillhub.cn for skills matching a query, let the user review the skill files in the TUI, and install the chosen skill to ~/.recursive/skills/. Only available in TUI mode — requires user confirmation before installing anything.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords to search for on skillhub.cn"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::External
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        // ── Headless guard ────────────────────────────────────────────────────
        let tx = match &self.skill_tx {
            Some(tx) => Arc::clone(tx),
            None => {
                return Ok("Error: install_skill is only available in TUI mode.\n\
                     The skill catalog is listed in the system prompt \
                     (search for \"Available skills\"); call `load_skill` to \
                     activate one.\n\
                     To install manually: skillhub install <slug>"
                    .to_string());
            }
        };

        let query = arguments["query"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "install_skill".into(),
                message: "missing required parameter: query".to_string(),
            })?
            .trim()
            .to_string();

        if query.is_empty() {
            return Err(Error::BadToolArgs {
                name: "install_skill".into(),
                message: "query must not be empty".to_string(),
            });
        }

        // ── Phase 1: search ───────────────────────────────────────────────────
        let client = Self::http_client().map_err(|e| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: format!("failed to build HTTP client: {e}"),
        })?;

        let results = Self::search(&client, &query).await?;

        if results.is_empty() {
            return Ok(format!(
                "No skills found on skillhub.cn for query \"{query}\".\n\
                 Try a different keyword, or browse https://skillhub.cn manually."
            ));
        }

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Option<String>>();
        tx.send(SkillInstallEvent::Search(SkillSearchRequest {
            query: query.clone(),
            results,
            reply: reply_tx,
        }))
        .map_err(|_| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: "TUI skill-install channel closed unexpectedly".to_string(),
        })?;

        let selected_slug = reply_rx.await.map_err(|_| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: "TUI did not reply to skill selection (channel dropped)".to_string(),
        })?;

        let slug = match selected_slug {
            Some(s) => s,
            None => return Ok("Installation cancelled.".to_string()),
        };

        // ── Phase 2: download zip and send file listing to TUI ────────────────
        let zip_bytes = Self::download_zip(&client, &slug).await?;
        let files = Self::parse_zip(&zip_bytes)?;

        let (confirm_tx, confirm_rx) = tokio::sync::oneshot::channel::<bool>();
        tx.send(SkillInstallEvent::Files(SkillFilesRequest {
            slug: slug.clone(),
            files,
            reply: confirm_tx,
        }))
        .map_err(|_| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: "TUI skill-install channel closed unexpectedly (phase 2)".to_string(),
        })?;

        let confirmed = confirm_rx.await.map_err(|_| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: "TUI did not reply to install confirmation (channel dropped)".to_string(),
        })?;

        if !confirmed {
            return Ok("Installation cancelled.".to_string());
        }

        // ── Install ───────────────────────────────────────────────────────────
        let dest = Self::install_dir(&slug)?;
        std::fs::create_dir_all(&dest).map_err(|e| Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: format!(
                "failed to create install directory '{}': {e}",
                dest.display()
            ),
        })?;

        Self::extract_zip(&zip_bytes, &dest)?;

        Ok(format!("Skill '{slug}' installed to {}", dest.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn headless_mode_returns_error_message() {
        let tool = InstallSkill::new(None);
        let result = tool.execute(json!({"query": "pdf"})).await.unwrap();
        assert!(
            result.contains("only available in TUI mode"),
            "expected headless error, got: {result}"
        );
    }

    #[tokio::test]
    async fn empty_query_errors() {
        let (tx, _rx) = mpsc::unbounded_channel::<SkillInstallEvent>();
        let tool = InstallSkill::new(Some(tx));
        let err = tool.execute(json!({"query": ""})).await.unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "expected empty error: {err}"
        );
    }

    #[test]
    fn parse_zip_rejects_bad_data() {
        let result = InstallSkill::parse_zip(b"not a zip");
        assert!(result.is_err(), "expected parse error for bad data");
    }

    #[test]
    fn install_dir_uses_home_recursive_skills() {
        let dir = InstallSkill::install_dir("my-skill").unwrap();
        assert!(
            dir.ends_with(".recursive/skills/my-skill"),
            "unexpected install dir: {}",
            dir.display()
        );
    }
}
