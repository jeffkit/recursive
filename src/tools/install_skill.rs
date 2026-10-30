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
//!
//! ## Security posture (issue #172, completing #103)
//!
//! Every input below is remote-controlled:
//!
//! - **Slug allowlist.** A slug must match `[A-Za-z0-9_-]{1,64}`. Search
//!   results with any other slug are dropped (WARN), and the slug returned by
//!   the TUI is re-validated before it reaches a URL or a filesystem path.
//!   `"."` used to resolve to the shared `~/.recursive/skills/` root, which
//!   let an archive overwrite sibling skills.
//! - **Archive integrity, fail-closed.** Before the bytes are parsed,
//!   previewed or written, their SHA-256 must match an expected digest from
//!   the `sha256` tool argument, the search entry's `sha256` field, or the
//!   operator pin `RECURSIVE_SKILL_SHA256=slug=<64-hex>`. With no expected
//!   digest the install is refused — unverified archives are never installed.
//! - **No redirects.** The client uses `redirect(Policy::none())`: a 3xx is
//!   an error, not a second request to a host nobody validated. The API's own
//!   302 is expected, so the fixed-host CDN fallback
//!   (`.../skills/<slug>/<version>.zip`) is tried instead (WARN).
//! - **Bounded sizes.** Search bodies (1 MiB) and archive downloads (32 MiB)
//!   are read with a hard cap; each archive entry (8 MiB) and the whole
//!   uncompressed archive (64 MiB) are capped while decompressing, so a zip
//!   bomb is rejected in [`InstallSkill::parse_zip`] — before the confirmation
//!   modal is ever shown.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::{resolve_within, Tool};

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

/// A downloaded archive plus the URL it came from (audit / diagnostics).
struct DownloadedArchive {
    bytes: Vec<u8>,
    url: String,
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
/// Longest accepted skill slug (URL path component + install directory name).
const MAX_SLUG_LEN: usize = 64;
/// Longest accepted version used to build the CDN fallback URL.
const MAX_VERSION_LEN: usize = 32;
/// Cap on the search-response body.
const MAX_SEARCH_BYTES: usize = 1024 * 1024;
/// Cap on a downloaded skill archive (compressed bytes).
const MAX_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;
/// Cap on one uncompressed archive entry.
const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;
/// Cap on the whole uncompressed archive.
const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

/// Decompression caps, threaded through the zip readers so tests can drive
/// the same enforcement with small numbers instead of allocating megabytes.
#[derive(Clone, Copy)]
struct ArchiveLimits {
    max_entry: u64,
    max_total: u64,
}

/// Caps used in production.
const ARCHIVE_LIMITS: ArchiveLimits = ArchiveLimits {
    max_entry: MAX_ENTRY_BYTES,
    max_total: MAX_TOTAL_UNCOMPRESSED_BYTES,
};

// ── Serde types for the skillhub.cn search API ───────────────────────────────

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<SearchItem>,
}

#[derive(Clone, Deserialize)]
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
    /// Optional manifest digest for the downloadable archive (hex, any case).
    /// Used as the expected digest when present; when absent an out-of-band
    /// pin (tool argument / `RECURSIVE_SKILL_SHA256`) is required.
    #[serde(default)]
    sha256: Option<String>,
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
    ///
    /// `redirect(Policy::none())` is the issue-#172 rule: a 3xx must never
    /// turn into a request to a host that was not validated here (the same
    /// posture as `HttpSkillSource` in `src/skills.rs`).
    fn http_client() -> reqwest::Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("recursive-agent/install_skill")
            .redirect(reqwest::redirect::Policy::none())
            .build()
    }

    /// Uniform tool error constructor.
    fn tool_error(message: impl Into<String>) -> Error {
        Error::Tool {
            name: "install_skill".into(),
            call_id: None,
            message: message.into(),
        }
    }

    /// A slug is a URL path segment and an install-directory name, so it is
    /// restricted to `[A-Za-z0-9_-]{1,64}`. This rejects `"."` (which would
    /// resolve to the shared `~/.recursive/skills/` root), `".."`, path
    /// separators, whitespace and non-ASCII input.
    fn validate_slug(slug: &str) -> Result<()> {
        let valid = !slug.is_empty()
            && slug.len() <= MAX_SLUG_LEN
            && slug
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if valid {
            Ok(())
        } else {
            Err(Self::tool_error(format!(
                "unsafe skill slug '{slug}': must match [A-Za-z0-9_-]{{1,{MAX_SLUG_LEN}}}"
            )))
        }
    }

    /// Version components come from the same untrusted search response and
    /// are interpolated into the CDN fallback URL, so they get a whitelist
    /// too (`.` is allowed inside a version, but `"."` / `".."` are not).
    fn is_safe_version(version: &str) -> bool {
        !version.is_empty()
            && version.len() <= MAX_VERSION_LEN
            && version != "."
            && version != ".."
            && version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    }

    /// Primary download URL (`/download?slug=…`), built through the URL API
    /// so the slug is percent-encoded rather than string-spliced.
    fn download_url(slug: &str) -> Result<String> {
        let mut url = reqwest::Url::parse(&format!("{SKILLHUB_BASE}/download"))
            .map_err(|e| Self::tool_error(format!("invalid skillhub download URL: {e}")))?;
        url.query_pairs_mut().append_pair("slug", slug);
        Ok(url.to_string())
    }

    /// Fixed-host CDN fallback URL (`/skills/<slug>/<version>.zip`).
    fn cdn_url(slug: &str, version: &str) -> Result<String> {
        let mut url = reqwest::Url::parse(SKILLHUB_CDN)
            .map_err(|e| Self::tool_error(format!("invalid skillhub CDN URL: {e}")))?;
        url.path_segments_mut()
            .map_err(|()| Self::tool_error("skillhub CDN URL cannot be a base URL"))?
            .push(slug)
            .push(&format!("{version}.zip"));
        Ok(url.to_string())
    }

    /// Call skillhub.cn search API and return the parsed (slug-vetted) items.
    ///
    /// Results whose slug is not a safe path/URL component are dropped with a
    /// WARN: they can never be installed, and offering them would only teach
    /// the user a slug that fails later.
    async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<SearchItem>> {
        let mut url = reqwest::Url::parse(&format!("{SKILLHUB_BASE}/search"))
            .map_err(|e| Self::tool_error(format!("invalid skillhub search URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("limit", &SEARCH_LIMIT.to_string());

        let body = Self::fetch_bytes_bounded(client, url.as_str(), MAX_SEARCH_BYTES).await?;
        let body: SearchResponse = serde_json::from_slice(&body).map_err(|e| {
            Self::tool_error(format!("failed to parse skillhub.cn search response: {e}"))
        })?;

        Ok(body
            .results
            .into_iter()
            .filter(|item| {
                if let Err(e) = Self::validate_slug(&item.slug) {
                    tracing::warn!(
                        slug = %item.slug,
                        "install_skill: dropping skillhub search result: {e}"
                    );
                    return false;
                }
                true
            })
            .collect())
    }

    /// Download the skill zip from skillhub.cn (with CDN fallback).
    ///
    /// The primary endpoint answers with a 302 to its CDN. Because redirects
    /// are not followed, that 302 is an error; the explicit fallback below
    /// requests the same object from the fixed CDN host instead. Every
    /// download is capped by [`MAX_ARCHIVE_BYTES`].
    async fn download_zip(
        client: &reqwest::Client,
        slug: &str,
        version: Option<&str>,
    ) -> Result<DownloadedArchive> {
        Self::validate_slug(slug)?;
        let primary = Self::download_url(slug)?;
        match Self::fetch_bytes_bounded(client, &primary, MAX_ARCHIVE_BYTES).await {
            Ok(bytes) => Ok(DownloadedArchive {
                bytes,
                url: primary,
            }),
            Err(primary_err) => {
                let Some(version) = version.filter(|v| Self::is_safe_version(v)) else {
                    return Err(primary_err);
                };
                // Degradation must be observable (owner ruling): the fallback
                // is a different host and is never silent.
                tracing::warn!(
                    slug = %slug,
                    error = %primary_err,
                    "install_skill: primary download failed; trying CDN fallback"
                );
                let fallback = Self::cdn_url(slug, version)?;
                let bytes = Self::fetch_bytes_bounded(client, &fallback, MAX_ARCHIVE_BYTES)
                    .await
                    .map_err(|e| {
                        Self::tool_error(format!(
                        "failed to download skill '{slug}': primary: {primary_err}; fallback: {e}"
                    ))
                    })?;
                Ok(DownloadedArchive {
                    bytes,
                    url: fallback,
                })
            }
        }
    }

    /// GET `url` and return at most `max_bytes`, failing closed when the body
    /// (declared or streamed) is larger. A 3xx is an error, never a follow.
    async fn fetch_bytes_bounded(
        client: &reqwest::Client,
        url: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let mut resp = client.get(url).send().await.map_err(|e| {
            Self::tool_error(format!("request to {url} failed: {}", e.without_url()))
        })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::tool_error(format!(
                "HTTP {status} for {url} (redirects are not followed)"
            )));
        }
        if resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|len| len > max_bytes)
        {
            return Err(Self::tool_error(format!(
                "response from {url} exceeds {max_bytes} bytes"
            )));
        }

        let mut body = Vec::new();
        loop {
            let chunk = resp.chunk().await.map_err(|e| {
                Self::tool_error(format!(
                    "failed to read response body from {url}: {}",
                    e.without_url()
                ))
            })?;
            let Some(chunk) = chunk else { break };
            if body.len() + chunk.len() > max_bytes {
                return Err(Self::tool_error(format!(
                    "response from {url} exceeds {max_bytes} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Lowercase-hex SHA-256 of `bytes`.
    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Normalize a declared digest (trim, lowercase) and require 64 hex chars.
    fn normalize_sha256(raw: &str) -> Option<String> {
        let hex = raw.trim().to_ascii_lowercase();
        (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
    }

    /// Operator-side pin from `RECURSIVE_SKILL_SHA256` (`slug=hex[,slug=hex…]`).
    ///
    /// This is the same trust anchor as `HttpSkillSource::with_pinned_sha256`:
    /// a digest obtained out-of-band survives a compromised skill host, which
    /// supplies both the archive and any digest it declares. A malformed
    /// configuration is an error, not a skipped pin.
    fn env_pinned_sha256(slug: &str) -> Result<Option<String>> {
        const ENV: &str = "RECURSIVE_SKILL_SHA256";
        let Ok(raw) = std::env::var(ENV) else {
            return Ok(None);
        };
        let mut found = None;
        for pair in raw.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let Some((key, value)) = pair.split_once('=') else {
                return Err(Self::tool_error(format!(
                    "malformed {ENV} entry '{pair}': expected slug=<64-hex>"
                )));
            };
            let key = key.trim();
            Self::validate_slug(key)
                .map_err(|e| Self::tool_error(format!("malformed {ENV} entry: {e}")))?;
            let digest = Self::normalize_sha256(value).ok_or_else(|| {
                Self::tool_error(format!(
                    "malformed {ENV} digest for '{key}': expected 64 hex characters"
                ))
            })?;
            if key == slug {
                found = Some(digest);
            }
        }
        Ok(found)
    }

    /// Resolve the digest the archive must match — or refuse to install.
    ///
    /// Sources (all must agree when more than one is present): the `sha256`
    /// tool argument, the search entry's `sha256`, and the operator env pin.
    /// Missing every source is fail-closed: the tool never installs an
    /// archive it cannot verify.
    fn resolve_expected_sha256(
        slug: &str,
        arg: Option<&str>,
        declared: Option<&str>,
    ) -> Result<String> {
        let mut expected: Option<(String, &'static str)> = None;
        for (source, raw) in [("sha256 argument", arg), ("skill entry", declared)] {
            let Some(raw) = raw else { continue };
            let digest = Self::normalize_sha256(raw).ok_or_else(|| {
                Self::tool_error(format!(
                    "invalid sha256 from {source} for skill '{slug}': \
                     expected 64 hex characters"
                ))
            })?;
            if let Some((prev, prev_source)) = &expected {
                if prev != &digest {
                    return Err(Self::tool_error(format!(
                        "conflicting sha256 for skill '{slug}': \
                         {prev_source} says {prev}, {source} says {digest}"
                    )));
                }
            } else {
                expected = Some((digest, source));
            }
        }

        if let Some(env_digest) = Self::env_pinned_sha256(slug)? {
            if let Some((prev, prev_source)) = &expected {
                if prev != &env_digest {
                    return Err(Self::tool_error(format!(
                        "conflicting sha256 for skill '{slug}': \
                         {prev_source} says {prev}, RECURSIVE_SKILL_SHA256 says {env_digest}"
                    )));
                }
            } else {
                expected = Some((env_digest, "RECURSIVE_SKILL_SHA256"));
            }
        }

        expected.map(|(digest, _)| digest).ok_or_else(|| {
            Self::tool_error(format!(
                "refusing to install skill '{slug}': no expected sha256 digest \
                 (fail-closed). Pass the `sha256` argument or set \
                 RECURSIVE_SKILL_SHA256={slug}=<64-hex>."
            ))
        })
    }

    /// Fail-closed archive integrity check. Runs before parsing, previewing
    /// and writing, so unverified bytes never reach the TUI or the disk.
    fn verify_archive_digest(slug: &str, bytes: &[u8], expected: &str) -> Result<()> {
        let expected = Self::normalize_sha256(expected).ok_or_else(|| {
            Self::tool_error(format!(
                "invalid expected sha256 for skill '{slug}': 64 hex characters required"
            ))
        })?;
        let got = Self::sha256_hex(bytes);
        if got != expected {
            return Err(Self::tool_error(format!(
                "skill '{slug}' failed sha256 verification: expected {expected}, got {got}"
            )));
        }
        Ok(())
    }

    /// Parse a zip archive in memory and return a flat list of (path, content) pairs.
    ///
    /// Entry and total decompression caps are enforced **here**, before the
    /// file list is sent to the TUI: a zip bomb must fail before the user is
    /// asked to confirm anything (memory is already spent by then).
    fn parse_zip(data: &[u8]) -> Result<Vec<SkillZipFile>> {
        Self::parse_zip_with_limits(data, ARCHIVE_LIMITS)
    }

    fn parse_zip_with_limits(data: &[u8], limits: ArchiveLimits) -> Result<Vec<SkillZipFile>> {
        let cursor = Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor)
            .map_err(|e| Self::tool_error(format!("failed to open zip archive: {e}")))?;

        let mut files = Vec::new();
        let mut total = 0u64;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| Self::tool_error(format!("failed to read zip entry {i}: {e}")))?;

            if entry.is_dir() {
                continue;
            }

            let path = entry.name().to_string();
            let declared = entry.size();
            let buf = Self::read_entry_bounded(&mut entry, &path, declared, limits, &mut total)?;
            let size = buf.len();
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

    /// Read one archive entry under both caps.
    ///
    /// The declared size is checked first (cheap rejection of a bomb), but the
    /// decompressor is also read through `take(max_entry + 1)` because zip
    /// headers can under-report their real output. `total` accumulates the
    /// bytes actually produced, so the archive-wide cap cannot be evaded by
    /// splitting a bomb across many entries.
    fn read_entry_bounded<R: Read>(
        reader: R,
        name: &str,
        declared: u64,
        limits: ArchiveLimits,
        total: &mut u64,
    ) -> Result<Vec<u8>> {
        if declared > limits.max_entry {
            return Err(Self::tool_error(format!(
                "zip entry '{name}' declares {declared} bytes, over the {} byte entry limit",
                limits.max_entry
            )));
        }

        let mut buf = Vec::new();
        let mut limited = reader.take(limits.max_entry.saturating_add(1));
        limited
            .read_to_end(&mut buf)
            .map_err(|e| Self::tool_error(format!("failed to read zip entry '{name}': {e}")))?;
        if buf.len() as u64 > limits.max_entry {
            return Err(Self::tool_error(format!(
                "zip entry '{name}' exceeds the {} byte entry limit",
                limits.max_entry
            )));
        }

        *total += buf.len() as u64;
        if *total > limits.max_total {
            return Err(Self::tool_error(format!(
                "archive exceeds the {} byte uncompressed limit",
                limits.max_total
            )));
        }
        Ok(buf)
    }

    /// Extract a zip archive (already loaded in memory) to `dest_dir`.
    ///
    /// Every entry path is resolved through [`resolve_within`] against
    /// `dest_dir`, so a hostile archive whose entry climbs out of the install
    /// directory (`a/../../x`, an absolute path after the slug prefix is
    /// stripped, …) is rejected instead of being written outside
    /// `~/.recursive/skills/<slug>/`.
    fn extract_zip(data: &[u8], dest_dir: &std::path::Path) -> Result<()> {
        Self::extract_zip_with_limits(data, dest_dir, ARCHIVE_LIMITS)
    }

    fn extract_zip_with_limits(
        data: &[u8],
        dest_dir: &std::path::Path,
        limits: ArchiveLimits,
    ) -> Result<()> {
        let cursor = Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor)
            .map_err(|e| Self::tool_error(format!("failed to open zip for extraction: {e}")))?;

        let mut total = 0u64;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| Self::tool_error(format!("zip entry {i} read error: {e}")))?;

            // Strip the leading slug-directory component (e.g. "pdf/SKILL.md" → "SKILL.md")
            let raw_name = entry.name().to_string();
            let relative = raw_name
                .split_once('/')
                .map(|(_, rest)| rest)
                .unwrap_or(&raw_name);

            // Containment gate: never `dest_dir.join` an unvalidated entry
            // name. `resolve_within` rejects `..` escapes and absolute paths
            // (both lexical and, for existing paths, symlink-aware).
            let out_path = resolve_within(dest_dir, relative).map_err(|e| {
                Self::tool_error(format!(
                    "zip entry '{raw_name}' escapes the install directory: {e}"
                ))
            })?;

            if entry.is_dir() {
                std::fs::create_dir_all(&out_path).map_err(|e| {
                    Self::tool_error(format!("failed to create dir '{}: {e}", out_path.display()))
                })?;
                continue;
            }

            let declared = entry.size();
            let buf =
                Self::read_entry_bounded(&mut entry, &raw_name, declared, limits, &mut total)?;

            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Self::tool_error(format!("mkdir '{}': {e}", parent.display())))?;
            }

            std::fs::write(&out_path, &buf)
                .map_err(|e| Self::tool_error(format!("write '{}': {e}", out_path.display())))?;
        }
        Ok(())
    }

    /// The shared install root: `~/.recursive/skills/`.
    fn install_root() -> Result<PathBuf> {
        let home =
            dirs::home_dir().ok_or_else(|| Self::tool_error("cannot determine home directory"))?;
        Ok(home.join(".recursive").join("skills"))
    }

    /// Resolve a slug under `root`, validating the slug first.
    ///
    /// Split from [`InstallSkill::install_dir`] so tests can use a temp root.
    /// The slug comes from remote search results, so it is not trusted: a
    /// value such as `"."` or `".."` would relocate the install root outside
    /// `~/.recursive/skills/` — and `"."` would make the install root itself
    /// the destination, letting an archive overwrite sibling skills.
    fn dest_for_slug(root: &Path, slug: &str) -> Result<PathBuf> {
        Self::validate_slug(slug)?;
        Ok(root.join(slug))
    }

    /// Resolve the install directory: `~/.recursive/skills/<slug>/`.
    fn install_dir(slug: &str) -> Result<PathBuf> {
        Self::dest_for_slug(&Self::install_root()?, slug)
    }
}

#[async_trait]
impl Tool for InstallSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "install_skill".into(),
            description: "Search skillhub.cn for skills matching a query, let the user review the skill files in the TUI, and install the chosen skill to ~/.recursive/skills/. Only available in TUI mode — requires user confirmation before installing anything. The download is refused unless its SHA-256 can be checked against an expected digest.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords to search for on skillhub.cn"
                    },
                    "sha256": {
                        "type": "string",
                        "description": "Optional expected SHA-256 of the skill archive (64 hex characters). Installs are refused without an expected digest; the operator-side alternative is RECURSIVE_SKILL_SHA256=slug=<hex>."
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

        let sha256_arg = arguments
            .get("sha256")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(raw) = sha256_arg.as_deref() {
            if Self::normalize_sha256(raw).is_none() {
                return Err(Error::BadToolArgs {
                    name: "install_skill".into(),
                    message: "sha256 must be 64 hex characters".to_string(),
                });
            }
        }

        // ── Phase 1: search ───────────────────────────────────────────────────
        let client = Self::http_client()
            .map_err(|e| Self::tool_error(format!("failed to build HTTP client: {e}")))?;

        let items = Self::search(&client, &query).await?;

        if items.is_empty() {
            return Ok(format!(
                "No skills found on skillhub.cn for query \"{query}\".\n\
                 Try a different keyword, or browse https://skillhub.cn manually."
            ));
        }

        let results: Vec<SkillSearchResult> = items.iter().cloned().map(Into::into).collect();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Option<String>>();
        tx.send(SkillInstallEvent::Search(SkillSearchRequest {
            query: query.clone(),
            results,
            reply: reply_tx,
        }))
        .map_err(|_| Self::tool_error("TUI skill-install channel closed unexpectedly"))?;

        let selected_slug = reply_rx.await.map_err(|_| {
            Self::tool_error("TUI did not reply to skill selection (channel dropped)")
        })?;

        let slug = match selected_slug {
            Some(s) => s,
            None => return Ok("Installation cancelled.".to_string()),
        };

        // The slug arrived from the TUI and is about to become a URL path
        // segment and a directory name: validate it, and require it to be one
        // of the results we actually showed (that lookup also yields the
        // version used by the CDN fallback).
        Self::validate_slug(&slug)?;
        let selected = items.iter().find(|item| item.slug == slug).ok_or_else(|| {
            Self::tool_error(format!(
                "TUI selected a skill slug that was not in the search results: '{slug}'"
            ))
        })?;

        // Integrity, fail-closed: no expected digest means no download.
        let expected_sha256 = Self::resolve_expected_sha256(
            &slug,
            sha256_arg.as_deref(),
            selected.sha256.as_deref(),
        )?;

        // ── Phase 2: download zip and send file listing to TUI ────────────────
        let version = Self::is_safe_version(&selected.version).then_some(selected.version.as_str());
        let archive = Self::download_zip(&client, &slug, version).await?;

        // Verify BEFORE parsing/previewing: the TUI must never show — and the
        // disk must never receive — bytes that failed the digest check.
        Self::verify_archive_digest(&slug, &archive.bytes, &expected_sha256)?;

        let files = Self::parse_zip(&archive.bytes)?;

        let (confirm_tx, confirm_rx) = tokio::sync::oneshot::channel::<bool>();
        tx.send(SkillInstallEvent::Files(SkillFilesRequest {
            slug: slug.clone(),
            files,
            reply: confirm_tx,
        }))
        .map_err(|_| Self::tool_error("TUI skill-install channel closed unexpectedly (phase 2)"))?;

        let confirmed = confirm_rx.await.map_err(|_| {
            Self::tool_error("TUI did not reply to install confirmation (channel dropped)")
        })?;

        if !confirmed {
            return Ok("Installation cancelled.".to_string());
        }

        // ── Install ───────────────────────────────────────────────────────────
        let dest = Self::install_dir(&slug)?;
        std::fs::create_dir_all(&dest).map_err(|e| {
            Self::tool_error(format!(
                "failed to create install directory '{}': {e}",
                dest.display()
            ))
        })?;

        Self::extract_zip(&archive.bytes, &dest)?;

        // Audit event mirrors HttpSkillSource (src/skills.rs): which skill,
        // which version, the verified digest, and the URL it came from.
        tracing::info!(
            target: "recursive::skills::audit",
            skill = %slug,
            version = %selected.version,
            sha256 = %expected_sha256,
            source = %archive.url,
            "installed remote skill"
        );

        Ok(format!(
            "Skill '{slug}' installed to {} (sha256 {expected_sha256})",
            dest.display()
        ))
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

    #[test]
    fn install_dir_rejects_slug_that_relocates_the_install_root() {
        // The slug is remote-supplied; anything that climbs out of
        // ~/.recursive/skills/ would also move extract_zip's containment root.
        for slug in ["", ".", "..", "../../etc", "a/b", "a\\b", "sub/.."] {
            assert!(
                InstallSkill::install_dir(slug).is_err(),
                "slug '{slug}' must be rejected"
            );
        }
    }

    /// Issue #172 negative control: `"."` resolves to the *shared*
    /// `~/.recursive/skills/` root, where an archive could overwrite sibling
    /// skills. The allowlist must reject it (and every other non-`[A-Za-z0-9_-]`
    /// form) before it reaches a path or a URL.
    #[test]
    fn slug_dot_is_rejected() {
        let root = tempfile::TempDir::new().unwrap();
        for slug in [".", "..", ".hidden", "", "a/b", "a\\b", "a b", "ü"] {
            let err = InstallSkill::dest_for_slug(root.path(), slug)
                .expect_err("unsafe slug must be rejected");
            assert!(
                err.to_string().contains("unsafe skill slug"),
                "unexpected error for '{slug}': {err}"
            );
        }
        assert!(
            InstallSkill::dest_for_slug(root.path(), "pdf-extract_2").is_ok(),
            "valid slugs must still resolve"
        );
        // Bounded: 64 bytes pass, 65 do not.
        let ok = "a".repeat(MAX_SLUG_LEN);
        let long = "a".repeat(MAX_SLUG_LEN + 1);
        assert!(InstallSkill::dest_for_slug(root.path(), &ok).is_ok());
        assert!(InstallSkill::dest_for_slug(root.path(), &long).is_err());
    }

    /// Test-only guard for `RECURSIVE_SKILL_SHA256`: holds the process-wide
    /// env lock and restores the previous value on drop.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let lock = crate::test_util::env_lock();
            let prev = std::env::var_os(key);
            // SAFETY: the env lock is held for this guard's lifetime.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self {
                key,
                prev,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: still holding the env lock (dropped last).
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Issue #172 negative control: without an expected digest the install is
    /// refused outright (fail-closed) — no download, no preview, no write.
    #[test]
    fn resolve_expected_sha256_rejects_missing_digest() {
        let _env = EnvVarGuard::set("RECURSIVE_SKILL_SHA256", None);
        let err = InstallSkill::resolve_expected_sha256("pdf-extract", None, None)
            .expect_err("missing digest must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to install"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("RECURSIVE_SKILL_SHA256"),
            "error must tell the operator how to pin: {msg}"
        );
    }

    #[test]
    fn resolve_expected_sha256_honors_the_env_pin() {
        let digest = "AB".repeat(32);
        let raw = format!("other-skill={}, pdf-extract={digest}", "0".repeat(64));
        let _env = EnvVarGuard::set("RECURSIVE_SKILL_SHA256", Some(raw.as_str()));
        let got = InstallSkill::resolve_expected_sha256("pdf-extract", None, None).unwrap();
        assert_eq!(got, digest.to_ascii_lowercase(), "hex must be normalized");

        // A pin that disagrees with an explicit argument is a hard error, not
        // a silent preference for one of them.
        let err = InstallSkill::resolve_expected_sha256("pdf-extract", Some(&"1".repeat(64)), None)
            .expect_err("conflicting digests must fail");
        assert!(err.to_string().contains("conflicting"), "unexpected: {err}");
    }

    #[test]
    fn resolve_expected_sha256_rejects_malformed_env_pins() {
        let _env = EnvVarGuard::set("RECURSIVE_SKILL_SHA256", Some("pdf-extract=nothex"));
        let err = InstallSkill::resolve_expected_sha256("pdf-extract", None, None)
            .expect_err("malformed pin must fail closed");
        assert!(err.to_string().contains("malformed"), "unexpected: {err}");
    }

    /// Issue #172 negative control: a digest mismatch aborts before anything
    /// is written, and no install directory is created.
    #[test]
    fn hash_mismatch_aborts_install() {
        let bytes = make_zip(&[("SKILL.md", "verified content")]);
        let expected = "0".repeat(64);
        let err = InstallSkill::verify_archive_digest("my-skill", &bytes, &expected)
            .expect_err("mismatch must fail closed");
        assert!(
            err.to_string().contains("failed sha256 verification"),
            "unexpected error: {err}"
        );

        // The real digest (any hex case) passes.
        let good = InstallSkill::sha256_hex(&bytes);
        InstallSkill::verify_archive_digest("my-skill", &bytes, &good.to_uppercase()).unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let dest = InstallSkill::dest_for_slug(tmp.path(), "my-skill").unwrap();
        assert!(!dest.exists(), "an unverified archive must not create dest");
    }

    #[test]
    fn download_urls_encode_the_slug_component() {
        let url = InstallSkill::download_url("pdf-extract").unwrap();
        assert_eq!(url, format!("{SKILLHUB_BASE}/download?slug=pdf-extract"));
        let url = InstallSkill::cdn_url("pdf-extract", "1.0.0").unwrap();
        assert_eq!(url, format!("{SKILLHUB_CDN}/pdf-extract/1.0.0.zip"));
    }

    #[test]
    fn unsafe_versions_are_not_used_for_the_fallback() {
        for version in ["", ".", "..", "a/b", "1.0 0", "../x"] {
            assert!(
                !InstallSkill::is_safe_version(version),
                "version '{version}' must be rejected"
            );
        }
        let long = "9".repeat(MAX_VERSION_LEN + 1);
        assert!(!InstallSkill::is_safe_version(&long), "overlong version");
        assert!(InstallSkill::is_safe_version("1.0.0"));
    }

    /// Issue #172 negative control: a 3xx must not be followed, even to a
    /// host that would happily serve the archive.
    #[tokio::test]
    async fn redirect_is_not_followed() {
        use mockito::Server;

        let mut target = Server::new_async().await;
        let target_mock = target
            .mock("GET", "/evil.zip")
            .with_status(200)
            .with_body("pwned")
            .expect(0)
            .create_async()
            .await;

        let mut redirector = Server::new_async().await;
        let redirect_mock = redirector
            .mock("GET", "/download")
            .with_status(302)
            .with_header("location", &format!("{}/evil.zip", target.url()))
            .expect(1)
            .create_async()
            .await;

        let client = InstallSkill::http_client().unwrap();
        let err = InstallSkill::fetch_bytes_bounded(
            &client,
            &format!("{}/download", redirector.url()),
            1024,
        )
        .await
        .expect_err("a redirect must surface as an error");
        assert!(err.to_string().contains("302"), "unexpected error: {err}");

        redirect_mock.assert_async().await;
        target_mock.assert_async().await;
    }

    /// Issue #172 negative control: a declared `Content-Length` over the cap
    /// fails before any body is read.
    #[tokio::test]
    async fn oversized_content_length_is_rejected() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/big.zip")
            .with_status(200)
            .with_body(vec![0u8; 2048])
            .create_async()
            .await;

        let client = InstallSkill::http_client().unwrap();
        let err =
            InstallSkill::fetch_bytes_bounded(&client, &format!("{}/big.zip", server.url()), 1024)
                .await
                .expect_err("oversized body must be rejected");
        assert!(err.to_string().contains("exceeds"), "unexpected: {err}");
    }

    /// Issue #172 negative control: a chunked (undeclared-length) body is
    /// cut off at the cap while streaming.
    #[tokio::test]
    async fn oversized_streamed_body_is_rejected() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/stream.zip")
            .with_status(200)
            .with_chunked_body(|w| w.write_all(&vec![0u8; 4096]))
            .create_async()
            .await;

        let client = InstallSkill::http_client().unwrap();
        let err = InstallSkill::fetch_bytes_bounded(
            &client,
            &format!("{}/stream.zip", server.url()),
            1024,
        )
        .await
        .expect_err("streamed body over the cap must be rejected");
        assert!(err.to_string().contains("exceeds"), "unexpected: {err}");
    }

    /// Issue #172 negative control: a zip bomb fails in `parse_zip` — that is
    /// *before* `SkillInstallEvent::Files`, so the user never sees a
    /// confirmation modal for it.
    #[test]
    fn zip_bomb_is_rejected_before_confirmation() {
        use std::io::Write;

        let cursor = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("bomb/SKILL.md", opts).unwrap();
        let zeros = vec![0u8; (MAX_ENTRY_BYTES + 1) as usize];
        zip.write_all(&zeros).unwrap();
        let data = zip.finish().unwrap().into_inner();

        let err = InstallSkill::parse_zip(&data).expect_err("zip bomb must be rejected");
        assert!(
            err.to_string().contains("entry limit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn zip_entry_limit_is_enforced() {
        let big = "x".repeat(2048);
        let data = make_zip(&[("a/SKILL.md", big.as_str())]);
        let limits = ArchiveLimits {
            max_entry: 1024,
            max_total: 10_000,
        };
        let err = InstallSkill::parse_zip_with_limits(&data, limits)
            .expect_err("oversized entry must be rejected");
        assert!(
            err.to_string().contains("entry limit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn zip_total_uncompressed_limit_is_enforced() {
        let first = "a".repeat(900);
        let second = "b".repeat(900);
        let data = make_zip(&[
            ("a/SKILL.md", first.as_str()),
            ("a/notes.md", second.as_str()),
        ]);
        // Each entry fits under max_entry, but together they exceed max_total.
        let limits = ArchiveLimits {
            max_entry: 1024,
            max_total: 1500,
        };
        let err = InstallSkill::parse_zip_with_limits(&data, limits)
            .expect_err("archive total must be capped");
        assert!(
            err.to_string().contains("uncompressed limit"),
            "unexpected error: {err}"
        );
    }

    /// Issue #172: the `"."` sibling-overwrite vector, end to end. `"."` is
    /// rejected as a slug, and the same hostile archive installed under a
    /// valid slug stays inside that slug's directory.
    #[test]
    fn zip_entry_overwriting_sibling_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("other-skill")).unwrap();
        std::fs::write(root.join("other-skill/SKILL.md"), "original").unwrap();

        assert!(
            InstallSkill::dest_for_slug(root, ".").is_err(),
            "slug '.' must never resolve to the shared skills root"
        );

        let data = make_zip(&[("x/other-skill/SKILL.md", "pwned")]);
        let dest = InstallSkill::dest_for_slug(root, "my-skill").unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        InstallSkill::extract_zip(&data, &dest).unwrap();

        let sibling = std::fs::read_to_string(root.join("other-skill/SKILL.md")).unwrap();
        assert_eq!(sibling, "original", "sibling skill must be untouched");
        let nested = std::fs::read_to_string(dest.join("other-skill/SKILL.md")).unwrap();
        assert_eq!(
            nested, "pwned",
            "entry must land inside the installed skill"
        );
    }

    /// Build a minimal in-memory zip with the given entries.
    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, content) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn parse_zip_returns_file_entries() {
        // kills `is_dir()` guard removal and function-level replacement
        let data = make_zip(&[("SKILL.md", "# My Skill"), ("README.md", "readme")]);
        let files = InstallSkill::parse_zip(&data).unwrap();
        assert_eq!(files.len(), 2, "must parse both files");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"SKILL.md"));
        assert!(paths.contains(&"README.md"));
    }

    #[test]
    fn parse_zip_sorts_by_path() {
        // kills mutation of sort_by direction (a.path.cmp vs b.path.cmp)
        let data = make_zip(&[("Z_tool.md", "z"), ("A_skill.md", "a")]);
        let files = InstallSkill::parse_zip(&data).unwrap();
        assert_eq!(
            files[0].path, "A_skill.md",
            "files must be sorted ascending by path"
        );
        assert_eq!(files[1].path, "Z_tool.md");
    }

    #[test]
    fn parse_zip_captures_content_and_size() {
        let data = make_zip(&[("SKILL.md", "hello world")]);
        let files = InstallSkill::parse_zip(&data).unwrap();
        assert_eq!(files[0].content, "hello world");
        assert_eq!(files[0].size, 11);
    }

    #[test]
    fn byte_caps_keep_their_documented_magnitudes() {
        // The two fetch caps are pure arithmetic consts and the fixed
        // skillhub.cn hosts cannot be intercepted in tests, so their
        // documented magnitudes (module doc: search bodies 1 MiB, archives
        // 32 MiB) are pinned literally. Any operator flip (`*`→`/`/`+`)
        // silently collapses or shifts the cap while still compiling.
        assert_eq!(MAX_SEARCH_BYTES, 1_048_576);
        assert_eq!(MAX_ARCHIVE_BYTES, 33_554_432);
    }

    #[test]
    fn extract_zip_strips_leading_slug_component() {
        // kills mutation of split_once('/') path stripping logic
        use std::io::Write;
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        // Simulate a zip from skillhub.cn where every file is under "my-skill/"
        zip.start_file("my-skill/SKILL.md", opts).unwrap();
        zip.write_all(b"# My Skill content").unwrap();
        let zip_data = zip.finish().unwrap().into_inner();

        let dest = tempfile::TempDir::new().unwrap();
        InstallSkill::extract_zip(&zip_data, dest.path()).unwrap();

        // The slug prefix "my-skill/" must be stripped; file lands as SKILL.md
        let output = std::fs::read_to_string(dest.path().join("SKILL.md")).unwrap();
        assert_eq!(output, "# My Skill content");
    }

    #[test]
    fn parse_zip_replaces_binary_content_with_placeholder() {
        // kills `unwrap_or_else(|_| "<binary>".to_string())` mutations
        use std::io::Write;
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        // Write raw non-UTF8 bytes (invalid UTF-8 sequence)
        zip.start_file("binary.bin", opts).unwrap();
        zip.write_all(&[0xFF, 0xFE, 0x00, 0x01]).unwrap();
        let data = zip.finish().unwrap().into_inner();

        let files = InstallSkill::parse_zip(&data).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].content, "<binary>",
            "non-UTF8 file content must become '<binary>'"
        );
    }

    #[test]
    fn extract_zip_file_without_slug_prefix_uses_full_name() {
        // kills mutations of `unwrap_or(&raw_name)` fallback in extract_zip
        use std::io::Write;
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        // File name has no '/' component — no slug to strip
        zip.start_file("SKILL.md", opts).unwrap();
        zip.write_all(b"no slug here").unwrap();
        let data = zip.finish().unwrap().into_inner();

        let dest = tempfile::TempDir::new().unwrap();
        InstallSkill::extract_zip(&data, dest.path()).unwrap();

        let output = std::fs::read_to_string(dest.path().join("SKILL.md")).unwrap();
        assert_eq!(output, "no slug here");
    }

    #[test]
    fn extract_zip_rejects_parent_dir_traversal() {
        // Hostile archive: the slug prefix is stripped, leaving
        // "../../install_escape_poc.md" — one directory above the install root.
        let data = make_zip(&[("my-skill/../../install_escape_poc.md", "pwned")]);
        let dest = tempfile::TempDir::new().unwrap();
        let slug_dir = dest.path().join("my-skill");
        std::fs::create_dir_all(&slug_dir).unwrap();

        let err = InstallSkill::extract_zip(&data, &slug_dir)
            .expect_err("traversal entry must be rejected");
        assert!(
            err.to_string().contains("escapes the install directory"),
            "unexpected error: {err}"
        );
        assert!(
            !dest.path().join("install_escape_poc.md").exists(),
            "no file may be written outside the install directory"
        );
    }

    #[test]
    fn extract_zip_rejects_absolute_entry_after_strip() {
        // "a//etc/escape" → split_once('/') leaves "/etc/escape" (absolute),
        // which `Path::join` would treat as a root replacement.
        let data = make_zip(&[("a//etc/install_escape_poc", "pwned")]);
        let dest = tempfile::TempDir::new().unwrap();
        let slug_dir = dest.path().join("my-skill");
        std::fs::create_dir_all(&slug_dir).unwrap();

        let err = InstallSkill::extract_zip(&data, &slug_dir)
            .expect_err("absolute entry must be rejected");
        assert!(
            err.to_string().contains("escapes the install directory"),
            "unexpected error: {err}"
        );
    }
}
