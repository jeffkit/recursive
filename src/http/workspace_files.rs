//! Goal 423 W2b — issue #132: the read-only workspace files endpoint.
//!
//! `GET /sessions/{id}/files` serves a session's workspace over HTTP in three
//! read-only modes, so a client can read what the agent works on without
//! routing every byte through the model:
//!
//! - `lines` (default) — a 1-indexed **inclusive** `start_line`/`end_line`
//!   page of a UTF-8 text file, exactly the `Read` tool's convention
//!   (`str::lines()` semantics: a trailing `\r` per line is stripped, pages
//!   join with `\n` — i.e. CRLF input is normalized the same way). Unlike the
//!   tool, out-of-bounds pages are **rejected, not clamped**: a page must sit
//!   inside the file.
//! - `bytes` — an `offset`/`length` window of the raw file, returned base64.
//!   Also rejected, not clamped, when the window leaves the file.
//! - `list` — a single-level directory listing (`name` / `kind`
//!   `file|dir|symlink` / `size`). Entries are never followed through
//!   symlinks.
//!
//! Red lines (owner ruling + issue #132 acceptance #3):
//!
//! - **Read-only.** GET is the only method routed here; nothing in this
//!   module writes — no `ReadFileState` row, no cache, no side effect.
//! - **Fail-closed paths.** Every path goes through
//!   [`crate::tools::dispatch::resolve_within_any`] (resolved-form
//!   containment, invariant #3, #170 precedent) — never a hand-rolled `..`
//!   filter. On top of that, a **terminal symlink is always rejected**
//!   (whether it points inside or outside the root — DSH parity, strict-safe),
//!   which must happen *before* resolution because the resolver follows
//!   links: the check runs on the joined path via `symlink_metadata`
//!   (non-following), per the resolver's caller-spelling contract.
//! - **Oversized is an error, never a truncation.** A file above
//!   `Config::workspace_file_max_bytes` is an explicit 413 in every mode —
//!   page requests included (a line page needs the full scan anyway).
//! - **Observable refusals.** Every rejection bumps
//!   `Metrics::workspace_file_rejections` by reason and emits a WARN carrying
//!   the session id, the client's relative spelling and the reason; no error
//!   message ever echoes a server-side absolute path.
//! - **Owner gate.** Like every `/sessions/:id*` route, access goes through
//!   `get_or_load_session` → `ensure_access` (issue #85): owner or admin,
//!   403 otherwise, and a session evicted or lost to a restart is cold-loaded
//!   instead of 404ing (the #227 lesson — this endpoint does not reproduce
//!   it).

use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use base64::{engine::general_purpose, Engine as _};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

use crate::tools::dispatch::{resolve_within_any, AccessTier};

use super::{ApiError, AppState, AuthIdentity, SessionState};

/// Upper bound on the number of lines one `lines` page may carry. A page
/// beyond it is an explicit 422 — never a clamp. The per-page **byte** ceiling
/// is `Config::workspace_file_max_bytes` itself: every page is carved out of
/// a file that already passed that gate, so no second knob is needed.
const MAX_LINES_PER_PAGE: u64 = 50_000;

/// Query parameters of `GET /sessions/{id}/files`.
#[derive(serde::Deserialize, Debug, Default)]
pub(super) struct WorkspaceFilesQuery {
    /// Path relative to the workspace root (`mode=list` may omit it for the
    /// root itself). Absolute paths are rejected by containment.
    #[serde(default)]
    pub(super) path: Option<String>,
    /// `lines` (default) / `bytes` / `list`.
    #[serde(default)]
    pub(super) mode: Option<String>,
    /// `mode=lines`: 1-indexed inclusive first line (default 1).
    #[serde(default)]
    pub(super) start_line: Option<u64>,
    /// `mode=lines`: 1-indexed inclusive last line (default: last line).
    #[serde(default)]
    pub(super) end_line: Option<u64>,
    /// `mode=bytes`: first byte of the window (default 0).
    #[serde(default)]
    pub(super) offset: Option<u64>,
    /// `mode=bytes`: window length (default: through end of file).
    #[serde(default)]
    pub(super) length: Option<u64>,
    /// Expected `version` (from a previous response). A mismatch answers 412
    /// — the file changed under the client; it re-reads explicitly instead of
    /// being served a silent mix of versions.
    #[serde(default)]
    pub(super) version: Option<String>,
}

/// The three access modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Lines,
    Bytes,
    List,
}

impl Mode {
    fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        match raw.unwrap_or("lines") {
            "" | "lines" => Ok(Self::Lines),
            "bytes" => Ok(Self::Bytes),
            "list" => Ok(Self::List),
            other => Err(ApiError::bad_request(format!(
                "unknown mode '{other}' (expected lines | bytes | list)"
            ))),
        }
    }
}

/// The workspace root a session's files resolve against.
///
/// Today the root is the server-wide `config.workspace` — this build has no
/// user/account notion, so there is nothing per-session to consult. The
/// helper exists as the single choke point: a future per-session root changes
/// exactly this function, never the path logic below.
fn session_workspace_root<'a>(state: &'a AppState, _session: &SessionState) -> &'a std::path::Path {
    state.config.workspace.as_path()
}

/// The joined path a client spelling denotes — the same join rule
/// [`resolve_within_any`] applies (an absolute candidate is taken as-is), so
/// the pre-resolution symlink check below inspects exactly the path the
/// resolver would start from.
fn joined_path(root: &std::path::Path, rel: &str) -> PathBuf {
    let candidate = std::path::Path::new(rel);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    }
}

/// A file's version token: `"{mtime_ms}-{len}"` (the `get_file_mtime`
/// precedent). Cheap, stable across identical content, and different the
/// moment size or mtime moves.
fn version_of(path: &std::path::Path, len: u64) -> String {
    let mtime_ms = crate::tools::execution::fs::get_file_mtime(path);
    format!("{mtime_ms}-{len}")
}

/// Record one rejection (metric by reason + WARN) and build its typed
/// [`ApiError`]. Messages quote the client's relative spelling only.
fn reject(
    state: &AppState,
    session_id: &str,
    rel: &str,
    reason: &str,
    status: axum::http::StatusCode,
    message: String,
) -> ApiError {
    state.metrics.workspace_file_rejections.inc(&[reason]);
    tracing::warn!(
        session_id = %session_id,
        path = %rel,
        reason,
        "workspace file request rejected"
    );
    ApiError::new(status, message)
}

/// GET /sessions/:id/files — read-only access to the session's workspace.
///
/// See the [module docs](self) for the mode semantics and the red lines.
pub(super) async fn session_workspace_files(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
    Query(query): Query<WorkspaceFilesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Owner/admin gate first, cold-loading included: after a restart or an
    // eviction the first touch may well be this endpoint, and it must serve —
    // not 404 (issue #227). A foreign identity materializes nothing.
    let session = super::cold_load::get_or_load_session(&state, &id, &identity).await?;

    let mode = Mode::parse(query.mode.as_deref())?;
    let rel = query.path.clone().unwrap_or_default();
    if rel.is_empty() && mode != Mode::List {
        return Err(ApiError::bad_request(
            "missing 'path' (required for mode=lines and mode=bytes)",
        ));
    }

    let root = session_workspace_root(&state, &session).to_path_buf();
    let limit = state.config.workspace_file_max_bytes;

    // Terminal-symlink gate — BEFORE resolution. `resolve_within_any` follows
    // symlinks (that is how it catches escapes), so by its output the leaf
    // link is already dereferenced; issue #132 requires rejecting the link
    // itself, inside the root or not. `symlink_metadata` stats the joined
    // path without following the final component.
    let joined = joined_path(&root, &rel);
    if tokio::fs::symlink_metadata(&joined)
        .await
        .map(|md| md.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(reject(
            &state,
            &id,
            &rel,
            "symlink",
            axum::http::StatusCode::BAD_REQUEST,
            format!(
                "path '{rel}' is a symlink; the files endpoint never follows one \
                 (rejected as: symlink)"
            ),
        ));
    }

    // Containment on the resolved form (invariant #3, #170 precedent). The
    // error already quotes the client's spelling, never the server's paths.
    let resolved =
        resolve_within_any(&[(root, AccessTier::ReadOnly)], &rel, false).map_err(|e| {
            reject(
                &state,
                &id,
                &rel,
                "escape",
                axum::http::StatusCode::BAD_REQUEST,
                format!("path rejected as escape: {e}"),
            )
        })?;

    let meta = tokio::fs::metadata(&resolved).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found(format!("no such entry: '{rel}'"))
        } else {
            ApiError::internal(format!("stat '{rel}' failed: {e}"))
        }
    })?;

    if meta.is_dir() {
        if mode != Mode::List {
            return Err(ApiError::bad_request(format!(
                "'{rel}' is a directory; use mode=list"
            )));
        }
        let version = version_of(&resolved, meta.len());
        assert_version(&state, &id, &rel, &query.version, &version)?;
        return Ok(Json(list_dir(&resolved, &rel, &version).await?));
    }
    if !meta.is_file() {
        return Err(ApiError::bad_request(format!(
            "'{rel}' is neither a regular file nor a directory"
        )));
    }
    if mode == Mode::List {
        return Err(ApiError::bad_request(format!(
            "'{rel}' is a file; mode=list lists directories"
        )));
    }

    // Size gate, before any mode work: an oversized file is an error, not a
    // truncation — page requests included (acceptance #3 of issue #132).
    let size = meta.len();
    if size > limit as u64 {
        return Err(reject(
            &state,
            &id,
            &rel,
            "too_large",
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "file '{rel}' is too large: {size} bytes (max {limit}); \
                 no mode truncates — split the file instead"
            ),
        ));
    }

    let version = version_of(&resolved, size);
    assert_version(&state, &id, &rel, &query.version, &version)?;

    match mode {
        Mode::Lines => Ok(Json(
            read_lines(&state, &id, &rel, &resolved, &query, &version, size).await?,
        )),
        Mode::Bytes => Ok(Json(
            read_bytes(&state, &id, &rel, &resolved, &query, &version, size).await?,
        )),
        // A list request for a file was refused above; a directory took the
        // list branch before the file gate.
        Mode::List => Err(ApiError::bad_request(format!(
            "'{rel}' is a file; mode=list lists directories"
        ))),
    }
}

/// Compare a client-supplied expected version against the current one.
fn assert_version(
    state: &AppState,
    session_id: &str,
    rel: &str,
    expected: &Option<String>,
    current: &str,
) -> Result<(), ApiError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if expected == current {
        return Ok(());
    }
    Err(reject(
        state,
        session_id,
        rel,
        "version_mismatch",
        axum::http::StatusCode::PRECONDITION_FAILED,
        format!(
            "file '{rel}' changed since the client last read it (version_mismatch): \
             expected '{expected}', now '{current}' — re-read before retrying"
        ),
    ))
}

/// `mode=lines`: a 1-indexed inclusive page of a UTF-8 text file.
///
/// Line splitting is `str::lines()` — the `Read` tool's convention — so a
/// CRLF file yields LF-normalized page content. Every out-of-range request is
/// an explicit error; nothing is clamped.
#[allow(clippy::too_many_arguments)]
async fn read_lines(
    state: &AppState,
    session_id: &str,
    rel: &str,
    resolved: &std::path::Path,
    query: &WorkspaceFilesQuery,
    version: &str,
    size: u64,
) -> Result<serde_json::Value, ApiError> {
    let bytes = tokio::fs::read(resolved)
        .await
        .map_err(|e| ApiError::internal(format!("read '{rel}' failed: {e}")))?;
    let content = String::from_utf8(bytes).map_err(|_| {
        reject(
            state,
            session_id,
            rel,
            "not_utf8",
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "file '{rel}' is not valid UTF-8 (not_utf8); mode=lines refuses to \
                 substitute characters — use mode=bytes"
            ),
        )
    })?;
    let total_lines = content.lines().count() as u64;

    // An empty file has no pages; the default (no explicit range) full read
    // is the empty content itself.
    if total_lines == 0 && query.start_line.is_none() && query.end_line.is_none() {
        return Ok(json!({
            "path": rel,
            "mode": "lines",
            "version": version,
            "size": size,
            "total_lines": 0,
            "start_line": 1,
            "end_line": 0,
            "content": "",
        }));
    }

    // 1-indexed inclusive bounds — `Read` tool semantics. Defaults: from the
    // first line through the last.
    let start = query.start_line.unwrap_or(1);
    let end = query.end_line.unwrap_or(total_lines);
    let range_err = |msg: String| {
        reject(
            state,
            session_id,
            rel,
            "range",
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            format!("out-of-range: {msg}"),
        )
    };
    if start == 0 || end == 0 {
        return Err(range_err(
            "start_line and end_line are 1-indexed and must be >= 1".into(),
        ));
    }
    if start > end {
        return Err(range_err(format!(
            "start_line ({start}) must be <= end_line ({end})"
        )));
    }
    // Explicit bounds outside the file are refused, not clamped.
    if start > total_lines || end > total_lines {
        return Err(range_err(format!(
            "line range {start}-{end} exceeds the file: '{rel}' has {total_lines} lines"
        )));
    }
    if end - start + 1 > MAX_LINES_PER_PAGE {
        return Err(range_err(format!(
            "page of {} lines exceeds the {}-line page limit; request a smaller \
             range",
            end - start + 1,
            MAX_LINES_PER_PAGE
        )));
    }

    let page: String = content
        .lines()
        .skip(start as usize - 1)
        .take(end as usize - start as usize + 1)
        .collect::<Vec<_>>()
        .join("\n");

    Ok(json!({
        "path": rel,
        "mode": "lines",
        "version": version,
        "size": size,
        "total_lines": total_lines,
        "start_line": start,
        "end_line": end,
        "content": page,
    }))
}

/// `mode=bytes`: an offset/length window of the raw file, base64-encoded.
/// The window must sit inside the file — rejected, not clamped — and its
/// length is bounded by the same per-file ceiling (a page is never larger
/// than what the size gate already allows for the whole file).
async fn read_bytes(
    state: &AppState,
    session_id: &str,
    rel: &str,
    resolved: &std::path::Path,
    query: &WorkspaceFilesQuery,
    version: &str,
    size: u64,
) -> Result<serde_json::Value, ApiError> {
    let offset = query.offset.unwrap_or(0);
    let range_err = |msg: String| {
        reject(
            state,
            session_id,
            rel,
            "range",
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            format!("out-of-range: {msg}"),
        )
    };
    if offset > size {
        return Err(range_err(format!(
            "offset {offset} exceeds the file: '{rel}' is {size} bytes"
        )));
    }
    let length = match query.length {
        Some(length) => {
            if offset + length > size {
                return Err(range_err(format!(
                    "byte window {offset}+{length} exceeds the file: \
                     '{rel}' is {size} bytes"
                )));
            }
            length
        }
        None => size - offset,
    };
    if length > state.config.workspace_file_max_bytes as u64 {
        return Err(range_err(format!(
            "window of {length} bytes exceeds the {}-byte page limit",
            state.config.workspace_file_max_bytes
        )));
    }

    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(resolved)
        .await
        .map_err(|e| ApiError::internal(format!("open '{rel}' failed: {e}")))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| ApiError::internal(format!("seek '{rel}' failed: {e}")))?;
    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)
        .await
        .map_err(|e| ApiError::internal(format!("read '{rel}' failed: {e}")))?;

    Ok(json!({
        "path": rel,
        "mode": "bytes",
        "version": version,
        "size": size,
        "offset": offset,
        "length": length,
        "data": general_purpose::STANDARD.encode(buf),
    }))
}

/// `mode=list`: a single-level listing of a directory. Entries are classified
/// by their own type (symlinks reported as `symlink`, never followed);
/// `size` is only meaningful for regular files.
async fn list_dir(
    resolved: &std::path::Path,
    rel: &str,
    version: &str,
) -> Result<serde_json::Value, ApiError> {
    let mut reader = tokio::fs::read_dir(resolved)
        .await
        .map_err(|e| ApiError::internal(format!("list '{rel}' failed: {e}")))?;
    let mut entries = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| ApiError::internal(format!("list '{rel}' failed: {e}")))?
    {
        let file_type = entry
            .file_type()
            .await
            .map_err(|e| ApiError::internal(format!("list '{rel}' failed: {e}")))?;
        let kind = if file_type.is_symlink() {
            "symlink"
        } else if file_type.is_dir() {
            "dir"
        } else {
            "file"
        };
        // `DirEntry::metadata` does not traverse a symlink, so a file's size
        // is the file's own; links and dirs carry no size at all.
        let size = if kind == "file" {
            Some(
                entry
                    .metadata()
                    .await
                    .map_err(|e| ApiError::internal(format!("list '{rel}' failed: {e}")))?
                    .len(),
            )
        } else {
            None
        };
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "kind": kind,
            "size": size,
        }));
    }
    entries.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["name"].as_str().unwrap_or_default())
    });
    Ok(json!({
        "path": rel,
        "mode": "list",
        "version": version,
        "entries": entries,
    }))
}
