//! `search_files`: substring/regex search across workspace files.
//!
//! Candidate discovery and file reads go through the [`ToolTransport`]
//! (Goal 402), so Grep searches inside whatever execution environment the
//! registry is bound to — not on the host.

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::transport::{retryable_prefix, ToolTransport, WalkOptions};
use crate::tools::{resolve_within_any, AccessTier, SessionToolState, SharedSandboxRoots, Tool};

const DEFAULT_MAX_RESULTS: usize = 50;
const DEFAULT_MAX_LINE_LEN: usize = 240;

/// Maximum file size Grep will read into memory. Larger files are skipped
/// (silently, matching the binary-extension skip) to avoid OOM on logs /
/// data dumps / bundled artifacts. 1 MiB covers virtually all source files.
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SearchFiles {
    pub root: PathBuf,
    pub extra_roots: Vec<(PathBuf, AccessTier)>,
    pub session_roots: Option<SharedSandboxRoots>,
    pub max_results: usize,
    /// Execution environment this tool searches in. Defaults to
    /// `LocalTransport`; builders inject the registry's transport so the
    /// tool follows the session's environment binding (Goal 402).
    pub transport: Arc<dyn ToolTransport>,
}

impl SearchFiles {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            extra_roots: Vec::new(),
            session_roots: None,
            max_results: DEFAULT_MAX_RESULTS,
            transport: Arc::new(crate::tools::transport::LocalTransport),
        }
    }

    /// Search inside a specific execution environment.
    pub fn with_transport(mut self, transport: Arc<dyn ToolTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Append additional allowed sandbox roots. See
    /// [`crate::tools::fs::ReadFile::with_extra_roots`].
    pub fn with_extra_roots(
        mut self,
        extra: impl IntoIterator<Item = (PathBuf, AccessTier)>,
    ) -> Self {
        self.extra_roots.extend(extra);
        self
    }

    /// Attach the shared, session-mutable roots slot. See [`SharedSandboxRoots`].
    pub fn with_session_roots(mut self, slot: SharedSandboxRoots) -> Self {
        self.session_roots = Some(slot);
        self
    }

    /// Convenience: attach the shared slot only when `Some`.
    pub fn with_session_roots_opt(mut self, slot: Option<SharedSandboxRoots>) -> Self {
        if let Some(s) = slot {
            self.session_roots = Some(s);
        }
        self
    }

    fn relativise(&self, path: &std::path::Path) -> std::path::PathBuf {
        if let Ok(rel) = path.strip_prefix(&self.root) {
            return rel.to_path_buf();
        }
        path.to_path_buf()
    }

    fn all_roots(&self) -> Vec<(PathBuf, AccessTier)> {
        let mut v: Vec<(PathBuf, AccessTier)> = Vec::with_capacity(self.extra_roots.len() + 1);
        v.push((self.root.clone(), AccessTier::ReadWrite));
        v.extend(self.extra_roots.iter().cloned());
        if let Some(slot) = &self.session_roots {
            if let Ok(roots) = slot.read() {
                v.extend(roots.iter().cloned());
            }
        }
        v
    }
}

#[async_trait]
impl Tool for SearchFiles {
    /// Goal 394: session-level fork — see [`Tool::fork_box`] on `GlobTool`.
    fn fork_box(&self, state: &SessionToolState) -> Option<Arc<dyn Tool>> {
        let mut forked = self.clone();
        if let (Some(_), Some(fresh)) = (&self.session_roots, &state.session_roots) {
            forked.session_roots = Some(fresh.clone());
        }
        Some(Arc::new(forked))
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Grep".into(),
            description:
                "Find lines containing a pattern (literal substring or regex) across files in the workspace. Returns up to N matches as 'path:line: text'."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern":   { "type": "string", "description": "Pattern to search for. Literal substring by default; use `regex: true` for regex mode." },
                    "path":      { "type": "string", "description": "Optional subdirectory (workspace-relative) to scope the search to. Defaults to workspace root." },
                    "max_results": { "type": "integer", "description": "Cap on results (default 50, max 200)." },
                    "regex":     { "type": "boolean", "description": "If true, interpret `pattern` as a regular expression (Rust regex crate syntax). Default false (literal substring)." },
                    "case_insensitive": { "type": "boolean", "description": "If true, matching ignores ASCII case. Works in both literal and regex modes; in regex mode this is equivalent to wrapping the pattern in `(?i:...)`. Default false." }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let pattern = args["pattern"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Grep".into(),
            message: "missing `pattern`".into(),
        })?;
        if pattern.is_empty() {
            return Err(Error::BadToolArgs {
                name: "Grep".into(),
                message: "`pattern` must not be empty".into(),
            });
        }

        let use_regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
        let case_insensitive = args
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let re_opt: Option<Regex> = if use_regex {
            let regex = if case_insensitive {
                regex::RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
            } else {
                Regex::new(pattern)
            };
            Some(regex.map_err(|e| Error::BadToolArgs {
                name: "Grep".into(),
                message: format!("invalid regex: {e}"),
            })?)
        } else {
            None
        };

        let scope = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => {
                resolve_within_any(&self.all_roots(), p, false).map_err(|e| Error::BadToolArgs {
                    name: "Grep".into(),
                    message: format!("path: {e}"),
                })?
            }
            None => self.root.clone(),
        };

        let cap = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(200))
            .unwrap_or(self.max_results);

        let mut hits: Vec<String> = Vec::new();
        // One walk round-trip through the execution environment (Goal 402).
        // Default WalkOptions: unbounded depth, no symlink following, and the
        // shared ignore set (.git / target / node_modules).
        let entries = self
            .transport
            .walk(&scope, &WalkOptions::default())
            .await
            .map_err(|e| Error::Tool {
                name: "Grep".into(),
                call_id: None,
                message: format!("{}walk {}: {e}", retryable_prefix(&e), scope.display()),
            })?;
        'outer: for entry in entries {
            if !entry.is_file {
                continue;
            }
            // Reconstruct the absolute (environment) path so `relativise`
            // sees the same shape the old in-tool walkdir produced.
            let path = scope.join(&entry.path);
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
            // Skip files that would OOM if read wholesale. Source files are well
            // under 1 MiB; anything larger is a log/data/artifact that grep
            // shouldn't slurp. Silent skip, matching the binary-extension skip.
            // (WalkEntry::size comes from the transport's traversal; the
            // depth-limited default walk reports 0 = unknown, which never
            // trips this skip.)
            if entry.size > MAX_GREP_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = self.transport.read_file(&path).await else {
                continue;
            };
            let Ok(contents) = String::from_utf8(bytes) else {
                continue;
            };
            let rel = self.relativise(&path);
            for (line_no, line) in contents.lines().enumerate() {
                let is_match = match &re_opt {
                    Some(re) => re.is_match(line),
                    None => {
                        if case_insensitive {
                            line.to_ascii_lowercase()
                                .contains(&pattern.to_ascii_lowercase())
                        } else {
                            line.contains(pattern)
                        }
                    }
                };
                if is_match {
                    let truncated = if line.len() > DEFAULT_MAX_LINE_LEN {
                        let mut end = DEFAULT_MAX_LINE_LEN;
                        while !line.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}…", &line[..end])
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
    async fn grep_skips_files_larger_than_cap() {
        let tmp = TempDir::new().unwrap();
        // File larger than MAX_GREP_FILE_BYTES (1 MiB + 1) that WOULD match.
        let big = format!("{}\nmatch\n", "a".repeat(MAX_GREP_FILE_BYTES as usize));
        assert!(big.len() as u64 > MAX_GREP_FILE_BYTES);
        write(&tmp, "big.log", &big);
        // Small file that also matches.
        write(&tmp, "small.txt", "match\n");
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "match"}))
            .await
            .unwrap();
        assert!(out.contains("small.txt:1: match"));
        assert!(!out.contains("big.log"));
    }

    /// The size cap is "larger than": a file sitting exactly on
    /// `MAX_GREP_FILE_BYTES` is still searched.
    #[tokio::test]
    async fn grep_reads_a_file_exactly_at_the_size_cap() {
        let tmp = TempDir::new().unwrap();
        let needle = "match-at-cap";
        let mut body = format!("{needle}\n");
        body.push_str(&"a".repeat(MAX_GREP_FILE_BYTES as usize - body.len() - 1));
        body.push('\n');
        assert_eq!(body.len() as u64, MAX_GREP_FILE_BYTES);
        write(&tmp, "edge.log", &body);
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": needle}))
            .await
            .unwrap();
        assert!(out.contains("edge.log:1: match-at-cap"), "output: {out}");
    }

    /// The line cap is "longer than": a line of exactly `DEFAULT_MAX_LINE_LEN`
    /// bytes comes through verbatim, one byte more is cut to the cap and
    /// marked with the ellipsis.
    #[tokio::test]
    async fn grep_truncates_only_lines_longer_than_the_cap() {
        let tmp = TempDir::new().unwrap();
        let at_cap = format!("hit{}", "x".repeat(DEFAULT_MAX_LINE_LEN - 3));
        let over_cap = format!("hit{}", "y".repeat(DEFAULT_MAX_LINE_LEN - 2));
        assert_eq!(at_cap.len(), DEFAULT_MAX_LINE_LEN);
        assert_eq!(over_cap.len(), DEFAULT_MAX_LINE_LEN + 1);
        write(&tmp, "at.txt", &at_cap);
        write(&tmp, "over.txt", &over_cap);
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "hit"}))
            .await
            .unwrap();
        let expect_at = format!("at.txt:1: {at_cap}");
        let expect_over = format!("over.txt:1: {}…", &over_cap[..DEFAULT_MAX_LINE_LEN]);
        assert!(out.lines().any(|l| l == expect_at), "output: {out}");
        assert!(out.lines().any(|l| l == expect_over), "output: {out}");
    }

    /// When the cap lands mid-character the cut backs up to the previous
    /// boundary — it never runs forward past the cap to the next one.
    #[tokio::test]
    async fn grep_truncation_backs_up_to_the_previous_char_boundary() {
        let tmp = TempDir::new().unwrap();
        // `─` (3 bytes) occupies bytes 238..=240, so byte 240 is mid-char.
        let prefix = format!("hit{}", "x".repeat(DEFAULT_MAX_LINE_LEN - 5));
        let line = format!("{prefix}─{}", "y".repeat(20));
        assert_eq!(prefix.len(), DEFAULT_MAX_LINE_LEN - 2);
        assert!(!line.is_char_boundary(DEFAULT_MAX_LINE_LEN));
        write(&tmp, "uni.txt", &line);
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "hit"}))
            .await
            .unwrap();
        assert_eq!(out, format!("uni.txt:1: {prefix}…"));
    }

    /// Goal 394: Grep holds the session-mutable roots slot, so a session fork
    /// must yield a clone bound to the fork's fresh slot — a root granted to
    /// the forked session afterwards is visible to the fork and only to it.
    #[tokio::test]
    async fn fork_box_rewires_grep_onto_the_fresh_session_roots_slot() {
        let workspace = TempDir::new().unwrap();
        let granted = TempDir::new().unwrap();
        std::fs::write(granted.path().join("note.txt"), "needle in granted root\n").unwrap();

        let parent_slot = crate::tools::new_shared_sandbox_roots();
        let tool = SearchFiles::new(workspace.path()).with_session_roots(parent_slot);
        let fresh_slot = crate::tools::new_shared_sandbox_roots();
        let state = SessionToolState {
            read_state: None,
            bg_manager: Arc::new(tokio::sync::Mutex::new(
                crate::tools::BackgroundJobManager::new(),
            )),
            session_roots: Some(fresh_slot.clone()),
            deliverables: None,
        };
        let forked = tool
            .fork_box(&state)
            .expect("Grep carries session roots, so it must fork onto the fresh slot");

        fresh_slot
            .write()
            .unwrap()
            .push((granted.path().to_path_buf(), AccessTier::ReadOnly));
        let scope = granted.path().to_string_lossy().to_string();
        let out = forked
            .execute(json!({"pattern": "needle", "path": scope.as_str()}))
            .await
            .unwrap();
        assert!(out.contains("needle in granted root"), "output: {out}");

        let err = tool
            .execute(json!({"pattern": "needle", "path": scope.as_str()}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::BadToolArgs { .. }),
            "parent must not see the fork's grant: {err:?}"
        );
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
    #[tokio::test]
    async fn regex_mode_matches_pattern() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "lib.rs", "fn foo() {}\nfn bar() {}\nfn foobar() {}");
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "fn f\\w+", "regex": true}))
            .await
            .unwrap();
        assert!(out.contains("foo"));
        assert!(out.contains("foobar"));
        assert!(!out.contains(": fn bar()"));
    }
    #[tokio::test]
    async fn regex_mode_invalid_pattern_is_bad_args() {
        let tmp = TempDir::new().unwrap();
        let err = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "(unclosed", "regex": true}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
        assert!(format!("{err}").contains("invalid regex"));
    }
    #[tokio::test]
    async fn literal_mode_treats_pattern_as_substring() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "data.txt", "abc\nadc");
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "a.c"}))
            .await
            .unwrap();
        assert!(out.contains("no matches"));
    }
    #[tokio::test]
    async fn regex_mode_with_path_scope() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        write(&tmp, "outside.txt", "fn main()");
        std::fs::write(tmp.path().join("src/lib.rs"), "fn helper()\nfn main()").unwrap();
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "fn \\w+", "regex": true, "path": "src"}))
            .await
            .unwrap();
        assert!(out.contains("helper"));
        assert!(out.contains("main"));
        assert!(!out.contains("outside.txt"));
    }

    // Tests for case_insensitive flag (goal-29)
    #[tokio::test]
    async fn literal_mode_case_insensitive_finds_match() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "todo.txt",
            "TODO: fix this
todo: done",
        );
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "TODO", "case_insensitive": true}))
            .await
            .unwrap();
        assert!(out.contains("TODO: fix this"));
        assert!(out.contains("todo: done"));
    }

    #[tokio::test]
    async fn regex_mode_case_insensitive_finds_match() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "test.txt",
            "foo123
bar456",
        );
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": r"FOO\d+", "regex": true, "case_insensitive": true}))
            .await
            .unwrap();
        assert!(out.contains("foo123"));
        assert!(!out.contains("bar456"));
    }

    #[tokio::test]
    async fn case_sensitive_by_default() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp,
            "mixed.txt",
            "TODO
todo
Todo",
        );
        // Without case_insensitive, should only find exact match
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "TODO"}))
            .await
            .unwrap();
        assert!(out.contains("TODO"));
        assert!(!out.contains("todo"));
        assert!(!out.contains("Todo"));
    }

    /// Regression test: long lines containing multi-byte Unicode characters (like the
    /// box-drawing `─` U+2500, encoded as 3 UTF-8 bytes E2 94 80) must not panic
    /// when the truncation boundary lands in the middle of a multi-byte sequence.
    #[tokio::test]
    async fn long_line_with_multibyte_unicode_does_not_panic() {
        let tmp = TempDir::new().unwrap();

        // Build a line where the 240-byte boundary lands inside `─` (U+2500, 3 bytes).
        // "search_target " is 14 ASCII bytes. Then append `─` chars until line > 240 bytes.
        // `─` takes 3 bytes, so we need (240 - 14) / 3 ≈ 75 chars to land near boundary.
        let mut long_line = String::from("search_target ");
        while long_line.len() <= 240 {
            long_line.push('─');
        }
        // Append a suffix so the file has another match-less line too.
        let body = format!("{long_line}\nno match here\n");
        write(&tmp, "unicode.txt", &body);

        // Must not panic.
        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "search_target"}))
            .await
            .unwrap();
        assert!(out.contains("unicode.txt:1:"));
        assert!(out.contains("search_target"));
    }

    /// Regression test: JSONL files with multi-byte Unicode inside a long JSON line
    /// (mimicking a session transcript that contains box-drawing characters) must not panic.
    #[tokio::test]
    async fn jsonl_line_with_unicode_box_drawing_does_not_panic() {
        let tmp = TempDir::new().unwrap();
        // Construct a long JSONL-like line that contains the target pattern AND box-drawing chars.
        let separator = "─".repeat(60); // 60 × 3 bytes = 180 bytes
        let content_field =
            format!("pub struct AgentRuntime{{}}\\n// {separator}─────────────────────────");
        let line = format!(
            "{{\"role\":\"tool\",\"content\":\"# range: lines 1-50 of 200\\\\n{content_field}\"}}"
        );
        assert!(
            line.len() > 240,
            "test line must exceed truncation limit; got {}",
            line.len()
        );
        write(&tmp, "transcript.jsonl", &line);

        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "pub struct AgentRuntime"}))
            .await
            .unwrap();
        assert!(out.contains("transcript.jsonl:1:"));
    }

    // ── Goal 402: Grep goes through the transport ────────────────────────────

    use std::collections::BTreeMap;
    use std::path::Path;

    /// In-memory "environment": files exist only inside the transport, keyed
    /// by absolute environment paths.
    #[derive(Debug, Default)]
    struct MemoryTransport {
        files: BTreeMap<PathBuf, Vec<u8>>,
    }

    impl MemoryTransport {
        fn with_file(mut self, path: &Path, contents: &[u8]) -> Self {
            self.files.insert(path.to_path_buf(), contents.to_vec());
            self
        }
    }

    #[async_trait]
    impl crate::tools::transport::ToolTransport for MemoryTransport {
        async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "not found"))
        }
        async fn write_file(&self, _path: &Path, _contents: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("unsupported"))
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> std::io::Result<Vec<crate::tools::transport::DirEntry>> {
            Err(std::io::Error::other("unsupported"))
        }
        async fn walk(
            &self,
            root: &Path,
            _opts: &crate::tools::transport::WalkOptions,
        ) -> std::io::Result<Vec<crate::tools::transport::WalkEntry>> {
            Ok(self
                .files
                .iter()
                .filter(|(p, _)| p.starts_with(root) && *p != root)
                .map(|(p, contents)| crate::tools::transport::WalkEntry {
                    path: p.strip_prefix(root).unwrap().to_path_buf(),
                    is_file: true,
                    size: contents.len() as u64,
                })
                .collect())
        }
        async fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }
        async fn exec_shell(
            &self,
            _command: &str,
            _cwd: &Path,
            _env: &[(String, String)],
            _timeout: std::time::Duration,
            _max_output_bytes: usize,
        ) -> std::io::Result<crate::tools::transport::ExecResult> {
            Err(std::io::Error::other("unsupported"))
        }
    }

    /// A transport whose walk fails with a transient (timeout) error.
    #[derive(Debug, Default)]
    struct TimedOutWalkTransport;

    #[async_trait]
    impl crate::tools::transport::ToolTransport for TimedOutWalkTransport {
        async fn read_file(&self, _path: &Path) -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::other("unsupported"))
        }
        async fn write_file(&self, _path: &Path, _contents: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("unsupported"))
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> std::io::Result<Vec<crate::tools::transport::DirEntry>> {
            Err(std::io::Error::other("unsupported"))
        }
        async fn walk(
            &self,
            _root: &Path,
            _opts: &crate::tools::transport::WalkOptions,
        ) -> std::io::Result<Vec<crate::tools::transport::WalkEntry>> {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "environment unreachable",
            ))
        }
        async fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }
        async fn exec_shell(
            &self,
            _command: &str,
            _cwd: &Path,
            _env: &[(String, String)],
            _timeout: std::time::Duration,
            _max_output_bytes: usize,
        ) -> std::io::Result<crate::tools::transport::ExecResult> {
            Err(std::io::Error::other("unsupported"))
        }
    }

    #[tokio::test]
    async fn grep_searches_through_transport_not_host_fs() {
        // Host workspace stays EMPTY — the tree lives only in the transport.
        let host = TempDir::new().unwrap();
        let transport = MemoryTransport::default()
            .with_file(&host.path().join("a.txt"), b"foo\nbar\nbaz")
            .with_file(&host.path().join("b.txt"), b"bar quux");
        let tool = SearchFiles::new(host.path()).with_transport(Arc::new(transport));

        let out = tool.execute(json!({"pattern": "bar"})).await.unwrap();
        assert!(out.contains("a.txt:2: bar"));
        assert!(out.contains("b.txt:1: bar quux"));
        assert!(
            !host.path().join("a.txt").exists(),
            "search must go through the transport — host fs must stay untouched"
        );
    }

    #[tokio::test]
    async fn grep_walk_failure_annotates_retryable() {
        let host = TempDir::new().unwrap();
        let tool = SearchFiles::new(host.path()).with_transport(Arc::new(TimedOutWalkTransport));
        let err = tool.execute(json!({"pattern": "bar"})).await.unwrap_err();
        let msg = match err {
            Error::Tool { message, .. } => message,
            other => panic!("expected Tool error, got {other:?}"),
        };
        assert!(
            msg.starts_with("retryable: "),
            "timeout-classified walk failure must be marked retryable, got: {msg}"
        );
    }

    #[tokio::test]
    async fn grep_ignores_default_dirs_via_local_transport() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::create_dir_all(tmp.path().join("target")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join("src/a.rs"), "needle here\n").unwrap();
        std::fs::write(tmp.path().join("target/gen.rs"), "needle in build\n").unwrap();
        std::fs::write(tmp.path().join(".git/hooks.sample"), "needle in git\n").unwrap();

        let out = SearchFiles::new(tmp.path())
            .execute(json!({"pattern": "needle"}))
            .await
            .unwrap();
        // Path separators are platform-dependent (`\` on Windows) — normalise
        // before asserting on the POSIX-looking fixture paths.
        let norm = out.replace('\\', "/");
        assert!(norm.contains("src/a.rs:1: needle here"));
        assert!(
            !norm.contains("target") && !norm.contains(".git"),
            ".git / target / node_modules must be ignored on both paths (got: {out})"
        );
    }

    /// Format-consistency pin (Goal 402): on a fixture tree with NO ignored
    /// dirs, the transport-backed Grep must produce byte-identical output to
    /// the pre-transport inline walkdir implementation (file visit order,
    /// path separators, line numbers, truncation ellipsis).
    #[tokio::test]
    async fn grep_output_identical_to_legacy_walkdir_on_plain_tree() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.rs"), "fn hit() {}\ncall hit()\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "hit at top\nnope\n").unwrap();
        std::fs::write(
            tmp.path().join("long.txt"),
            format!("{}\nhit\n", "x".repeat(300)),
        )
        .unwrap();

        let tool = SearchFiles::new(tmp.path());
        let new_out = tool.execute(json!({"pattern": "hit"})).await.unwrap();

        // Legacy reference: the exact pre-Goal-402 execute() body.
        let pattern = "hit";
        let mut legacy_hits: Vec<String> = Vec::new();
        'legacy: for entry in walkdir::WalkDir::new(tmp.path())
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
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
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            if meta.len() > MAX_GREP_FILE_BYTES {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(path) else {
                continue;
            };
            let rel = tool.relativise(path);
            for (line_no, line) in contents.lines().enumerate() {
                if line.contains(pattern) {
                    let truncated = if line.len() > DEFAULT_MAX_LINE_LEN {
                        let mut end = DEFAULT_MAX_LINE_LEN;
                        while !line.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}…", &line[..end])
                    } else {
                        line.to_string()
                    };
                    legacy_hits.push(format!("{}:{}: {}", rel.display(), line_no + 1, truncated));
                    if legacy_hits.len() >= tool.max_results {
                        break 'legacy;
                    }
                }
            }
        }
        let legacy_out = if legacy_hits.is_empty() {
            format!("no matches for `{pattern}`")
        } else {
            legacy_hits.join("\n")
        };

        assert_eq!(new_out, legacy_out);
    }
}
