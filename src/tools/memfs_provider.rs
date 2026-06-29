//! In-memory virtual filesystem [`ToolSetProvider`] (L0).
//!
//! [`MemFsToolSetProvider`] provides a fully in-process virtual execution
//! environment: all file reads, writes, and basic shell commands operate on
//! an in-memory `HashMap` without touching the real filesystem or spawning
//! any processes.
//!
//! # Use cases
//!
//! * **Unit tests** — set up a file tree in memory without tempdir overhead.
//! * **Lightweight SaaS cells** — KB-level overhead per agent session.
//! * **Dry-run / preview** — let the model "do work" and inspect the result
//!   before committing to a real execution environment.
//!
//! # Limitations
//!
//! Only basic shell commands are simulated (`ls`, `pwd`, `cd`, `cat`, `echo`,
//! `mkdir -p`, `rm`, `which`, `true`, `false`). Commands that need real process
//! execution (e.g. `cargo build`) return an explicit error. The caller should
//! upgrade to a higher sandbox tier (Docker L2 / E2B L3) for those workloads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tool_set_provider::{SandboxMode, ToolSetProvider};
use crate::tools::{Tool, ToolRegistry, ToolSideEffect};

// ─────────────────────────────────────────────────────────────────────────────
// MemFs
// ─────────────────────────────────────────────────────────────────────────────

/// In-memory virtual filesystem.
///
/// Keys are canonical absolute paths (always `/`-prefixed). The "virtual root"
/// is `/workspace`. Tools transparently prepend it when they receive relative
/// paths, mirroring how the real tools behave under a workspace root.
#[derive(Debug, Default, Clone)]
pub struct MemFs {
    /// file path → raw bytes
    files: HashMap<PathBuf, Vec<u8>>,
    /// current working directory (absolute)
    cwd: PathBuf,
}

impl MemFs {
    const VIRTUAL_ROOT: &'static str = "/workspace";

    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            cwd: PathBuf::from(Self::VIRTUAL_ROOT),
        }
    }

    /// Pre-populate with `(path, content)` pairs. Paths starting with `/` are
    /// used as-is; relative paths are placed under `/workspace/`.
    pub fn with_files<P, C>(files: impl IntoIterator<Item = (P, C)>) -> Self
    where
        P: Into<PathBuf>,
        C: Into<Vec<u8>>,
    {
        let mut fs = Self::new();
        for (p, c) in files {
            let p: PathBuf = p.into();
            let abs = fs.resolve(&p);
            fs.files.insert(abs, c.into());
        }
        fs
    }

    /// Resolve a path to an absolute path under the virtual root.
    fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    /// Read a file. Returns `Err(Tool)` if it does not exist.
    pub fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let abs = self.resolve(path);
        self.files.get(&abs).cloned().ok_or_else(|| Error::Tool {
            name: "Read".into(),
            call_id: None,
            message: format!("file not found: {}", abs.display()),
        })
    }

    /// Write a file, creating any implicit parent "directories".
    pub fn write(&mut self, path: &Path, content: Vec<u8>) -> Result<()> {
        let abs = self.resolve(path);
        self.files.insert(abs, content);
        Ok(())
    }

    /// List entries whose path starts with `dir/`.
    pub fn list(&self, dir: &Path) -> Result<Vec<String>> {
        let abs_dir = self.resolve(dir);
        let prefix = format!("{}/", abs_dir.display());
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for p in self.files.keys() {
            let s = p.display().to_string();
            if s.starts_with(&prefix) {
                // Take only the first path component after the directory.
                let rest = &s[prefix.len()..];
                let component = rest.split('/').next().unwrap_or(rest);
                if !component.is_empty() {
                    names.insert(component.to_string());
                }
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Returns `true` if the path exists as a file.
    pub fn exists(&self, path: &Path) -> bool {
        let abs = self.resolve(path);
        self.files.contains_key(&abs)
    }

    /// Delete a file (no error if not found).
    pub fn delete(&mut self, path: &Path) {
        let abs = self.resolve(path);
        self.files.remove(&abs);
    }

    /// Find all file paths matching a glob pattern relative to the virtual root.
    /// Pattern is matched against the path relative to `/workspace`.
    pub fn glob_match(&self, pattern: &str) -> Vec<PathBuf> {
        let root = PathBuf::from(Self::VIRTUAL_ROOT);
        let mut results = Vec::new();
        for path in self.files.keys() {
            if let Ok(rel) = path.strip_prefix(&root) {
                let rel_str = rel.display().to_string().replace('\\', "/");
                if glob_matches(pattern, &rel_str) {
                    results.push(rel.to_path_buf());
                }
            }
        }
        results.sort();
        results
    }

    /// Search file contents with a regex. Returns `(path, matching_line_numbers)`.
    pub fn grep_content(
        &self,
        pattern: &str,
        case_insensitive: bool,
    ) -> Result<Vec<(PathBuf, Vec<usize>)>> {
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| Error::BadToolArgs {
                name: "Grep".into(),
                message: format!("invalid regex: {e}"),
            })?;
        let root = PathBuf::from(Self::VIRTUAL_ROOT);
        let mut results = Vec::new();
        for (path, bytes) in &self.files {
            let text = String::from_utf8_lossy(bytes);
            let mut lines = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    lines.push(i + 1);
                }
            }
            if !lines.is_empty() {
                let rel = path.strip_prefix(&root).unwrap_or(path).to_path_buf();
                results.push((rel, lines));
            }
        }
        results.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(results)
    }

    /// Current working directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Change directory.
    pub fn cd(&mut self, path: &Path) {
        self.cwd = self.resolve(path);
    }

    /// Execute a simulated shell command. Returns `(stdout, exit_code)`.
    pub fn exec_shell(&mut self, command: &str) -> (String, i32) {
        let cmd = command.trim();

        // Strip leading env assignments (VAR=val cmd ...).
        let cmd = {
            let mut rest = cmd;
            loop {
                // Check if the token looks like VAR=value.
                let token = rest.split_whitespace().next().unwrap_or("");
                if token.contains('=')
                    && !token.starts_with('-')
                    && token.split('=').next().is_some_and(|k| {
                        !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_')
                    })
                {
                    rest = rest[token.len()..].trim_start();
                } else {
                    break;
                }
            }
            rest
        };

        // Parse command and arguments (very basic, no quoting support beyond
        // simple cases).
        let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
        let prog = parts[0];
        let args_str = parts.get(1).copied().unwrap_or("");

        match prog {
            "true" | ":" => ("".into(), 0),
            "false" => ("".into(), 1),
            "echo" => (format!("{}\n", args_str), 0),
            "pwd" => (format!("{}\n", self.cwd.display()), 0),

            "cd" => {
                let target = PathBuf::from(args_str.trim());
                self.cd(&target);
                ("".into(), 0)
            }

            "which" => {
                let bin = args_str.trim();
                (format!("/usr/bin/{bin}\n"), 0)
            }

            "mkdir" => {
                // Support `mkdir -p <dir>` (we just record a `.keep` placeholder).
                let dir_arg = args_str.trim_start_matches("-p").trim();
                let keep = PathBuf::from(dir_arg).join(".keep");
                let _ = self.write(&keep, b"".to_vec());
                ("".into(), 0)
            }

            "rm" => {
                let target_arg = args_str.trim_start_matches("-f").trim();
                let target = PathBuf::from(target_arg);
                self.delete(&target);
                ("".into(), 0)
            }

            "cat" => {
                let file = PathBuf::from(args_str.trim());
                match self.read(&file) {
                    Ok(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), 0),
                    Err(e) => (format!("cat: {e}\n"), 1),
                }
            }

            "ls" => {
                // Parse flags and optional directory argument.
                // Tokens starting with '-' are flags; others are the directory path.
                let dir_arg = args_str
                    .split_whitespace()
                    .find(|t| !t.starts_with('-'))
                    .unwrap_or("");
                let dir = if dir_arg.is_empty() {
                    self.cwd.clone()
                } else {
                    PathBuf::from(dir_arg)
                };
                match self.list(&dir) {
                    Ok(entries) => {
                        if entries.is_empty() {
                            ("".into(), 0)
                        } else {
                            (entries.join("\n") + "\n", 0)
                        }
                    }
                    Err(e) => (format!("ls: {e}\n"), 1),
                }
            }

            _ => (
                format!(
                    "memfs-shell: command not supported in virtual environment: {prog}\n\
                     Tip: upgrade to DockerToolSetProvider or E2bToolSetProvider for real execution.\n"
                ),
                127,
            ),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Glob matching (adapted from src/tools/glob.rs)
// ─────────────────────────────────────────────────────────────────────────────

fn glob_matches(pattern: &str, rel_path: &str) -> bool {
    let rel = rel_path.replace('\\', "/");
    let pat = pattern.replace('\\', "/");
    // Prepend **/ if the pattern has no slash (match in any subdir).
    let pat = if pat.contains('/') {
        pat
    } else {
        format!("**/{pat}")
    };
    let pp: Vec<&str> = pat.split('/').collect();
    let rp: Vec<&str> = rel.split('/').collect();
    match_path(&pp, &rp)
}

fn match_path(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern, path) {
        ([], []) => true,
        (["**"], _) => true,
        (["**", rest @ ..], path) => {
            // ** can consume zero or more components.
            for i in 0..=path.len() {
                if match_path(rest, &path[i..]) {
                    return true;
                }
            }
            false
        }
        ([p, pr @ ..], [h, hr @ ..]) => match_component(p, h) && match_path(pr, hr),
        _ => false,
    }
}

fn match_component(pattern: &str, name: &str) -> bool {
    let mut pi = pattern.chars().peekable();
    let mut ni = name.chars();
    loop {
        match (pi.next(), ni.next()) {
            (None, None) => return true,
            (Some('*'), _) => {
                // * matches any sequence within a single component.
                let rest_pat: String = pi.collect();
                // Try matching the rest of the pattern against every suffix.
                let rest_name: String = ni.collect();
                for i in 0..=rest_name.len() {
                    // Find valid char boundaries.
                    if rest_name.is_char_boundary(i) && match_component(&rest_pat, &rest_name[i..])
                    {
                        return true;
                    }
                }
                return false;
            }
            (Some('?'), Some(_)) => continue,
            (Some(p), Some(n)) if p == n => continue,
            _ => return false,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tool implementations
// ─────────────────────────────────────────────────────────────────────────────

type SharedMemFs = Arc<Mutex<MemFs>>;

/// `Read` tool backed by in-memory filesystem.
pub struct MemFsReadTool {
    fs: SharedMemFs,
}

#[async_trait]
impl Tool for MemFsReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Read".into(),
            description: "Read a file from the in-memory virtual filesystem.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to read (relative or absolute in virtual FS)"}
                },
                "required": ["path"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path_str = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Read".into(),
            message: "missing `path`".into(),
        })?;
        let fs = self.fs.lock().await;
        let bytes = fs.read(Path::new(path_str))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// `Write` tool backed by in-memory filesystem.
pub struct MemFsWriteTool {
    fs: SharedMemFs,
}

#[async_trait]
impl Tool for MemFsWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Write".into(),
            description: "Write a file to the in-memory virtual filesystem.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to write"},
                    "content": {"type": "string", "description": "Content to write"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::Mutating
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path_str = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Write".into(),
            message: "missing `path`".into(),
        })?;
        let content = args["content"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Write".into(),
            message: "missing `content`".into(),
        })?;
        let mut fs = self.fs.lock().await;
        fs.write(Path::new(path_str), content.as_bytes().to_vec())?;
        Ok(format!("Wrote {} bytes to {path_str}", content.len()))
    }
}

/// `Edit` (StrReplace) tool backed by in-memory filesystem.
pub struct MemFsEditTool {
    fs: SharedMemFs,
}

#[async_trait]
impl Tool for MemFsEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Edit".into(),
            description: "Apply a str_replace edit to a file in the in-memory virtual filesystem."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to edit"},
                    "old_string": {"type": "string", "description": "Exact string to replace"},
                    "new_string": {"type": "string", "description": "Replacement string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::Mutating
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path_str = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Edit".into(),
            message: "missing `path`".into(),
        })?;
        let old = args["old_string"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "Edit".into(),
                message: "missing `old_string`".into(),
            })?;
        let new = args["new_string"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "Edit".into(),
                message: "missing `new_string`".into(),
            })?;
        let mut fs = self.fs.lock().await;
        let bytes = fs.read(Path::new(path_str))?;
        let text = String::from_utf8_lossy(&bytes);
        if !text.contains(old) {
            return Err(Error::Tool {
                name: "Edit".into(),
                call_id: None,
                message: format!("old_string not found in {path_str}"),
            });
        }
        let updated = text.replacen(old, new, 1);
        fs.write(Path::new(path_str), updated.into_bytes())?;
        Ok(format!("Applied edit to {path_str}"))
    }
}

/// `Bash` tool backed by in-memory virtual shell.
pub struct MemFsBashTool {
    fs: SharedMemFs,
}

#[async_trait]
impl Tool for MemFsBashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Bash".into(),
            description: "Run a simulated shell command in the in-memory virtual environment. \
                           Supports: ls, pwd, cd, cat, echo, mkdir -p, rm, which, true, false. \
                           Other commands return exit code 127."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to simulate"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::Mutating
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let command = args["command"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Bash".into(),
            message: "missing `command`".into(),
        })?;
        let mut fs = self.fs.lock().await;
        let (output, exit_code) = fs.exec_shell(command);
        if exit_code == 0 {
            Ok(output)
        } else {
            // Non-zero exit: return output but as an error so the model notices.
            Err(Error::Tool {
                name: "Bash".into(),
                call_id: None,
                message: format!("exit {exit_code}: {output}"),
            })
        }
    }
}

/// `Glob` tool backed by in-memory filesystem.
pub struct MemFsGlobTool {
    fs: SharedMemFs,
}

#[async_trait]
impl Tool for MemFsGlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Glob".into(),
            description: "Find files matching a glob pattern in the in-memory virtual filesystem."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern (e.g. \"**/*.rs\")"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let pattern = args["pattern"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Glob".into(),
            message: "missing `pattern`".into(),
        })?;
        let fs = self.fs.lock().await;
        let results = fs.glob_match(pattern);
        if results.is_empty() {
            Ok("(no matches)".into())
        } else {
            Ok(results
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
}

/// `Grep` tool backed by in-memory filesystem.
pub struct MemFsGrepTool {
    fs: SharedMemFs,
}

#[async_trait]
impl Tool for MemFsGrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Grep".into(),
            description: "Search file contents by regex in the in-memory virtual filesystem."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Case-insensitive search (default: false)"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let pattern = args["pattern"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Grep".into(),
            message: "missing `pattern`".into(),
        })?;
        let case_insensitive = args["case_insensitive"].as_bool().unwrap_or(false);
        let fs = self.fs.lock().await;
        let results = fs.grep_content(pattern, case_insensitive)?;
        if results.is_empty() {
            return Ok("(no matches)".into());
        }
        let mut out = String::new();
        for (path, lines) in &results {
            for &ln in lines {
                out.push_str(&format!("{}:{ln}\n", path.display()));
            }
        }
        Ok(out)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MemFsToolSetProvider
// ─────────────────────────────────────────────────────────────────────────────

/// [`ToolSetProvider`] backed by an in-memory virtual filesystem.
///
/// All file and shell operations are simulated in memory; no real files or
/// processes are touched. This is the lightest possible execution tier (L0).
pub struct MemFsToolSetProvider {
    fs: SharedMemFs,
}

impl Default for MemFsToolSetProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MemFsToolSetProvider {
    /// Create an empty in-memory provider.
    pub fn new() -> Self {
        Self {
            fs: Arc::new(Mutex::new(MemFs::new())),
        }
    }

    /// Create a provider pre-populated with the given files.
    pub fn with_files<P, C>(files: impl IntoIterator<Item = (P, C)>) -> Self
    where
        P: Into<PathBuf>,
        C: Into<Vec<u8>>,
    {
        Self {
            fs: Arc::new(Mutex::new(MemFs::with_files(files))),
        }
    }

    /// Expose the underlying [`MemFs`] for inspection (tests / diagnostics).
    pub fn memfs(&self) -> SharedMemFs {
        Arc::clone(&self.fs)
    }
}

impl ToolSetProvider for MemFsToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        let fs = Arc::clone(&self.fs);
        ToolRegistry::default()
            .register(Arc::new(MemFsReadTool {
                fs: Arc::clone(&fs),
            }))
            .register(Arc::new(MemFsWriteTool {
                fs: Arc::clone(&fs),
            }))
            .register(Arc::new(MemFsEditTool {
                fs: Arc::clone(&fs),
            }))
            .register(Arc::new(MemFsBashTool {
                fs: Arc::clone(&fs),
            }))
            .register(Arc::new(MemFsGlobTool {
                fs: Arc::clone(&fs),
            }))
            .register(Arc::new(MemFsGrepTool { fs }))
    }

    fn sandbox_mode(&self) -> SandboxMode {
        // MemFs is not a security sandbox — it's a virtual execution domain.
        SandboxMode::None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_fs() -> MemFs {
        MemFs::with_files([
            ("src/main.rs", "fn main() { println!(\"hello\"); }\n"),
            (
                "src/lib.rs",
                "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            ),
            ("README.md", "# Project\n\nA Rust project.\n"),
        ])
    }

    #[test]
    fn memfs_read_write() {
        let mut fs = MemFs::new();
        fs.write(Path::new("hello.txt"), b"world".to_vec()).unwrap();
        let bytes = fs.read(Path::new("hello.txt")).unwrap();
        assert_eq!(bytes, b"world");
    }

    #[test]
    fn memfs_read_missing_returns_error() {
        let fs = MemFs::new();
        assert!(fs.read(Path::new("missing.txt")).is_err());
    }

    #[test]
    fn memfs_list_dir() {
        let fs = make_fs();
        let mut entries = fs.list(Path::new("src")).unwrap();
        entries.sort();
        assert!(entries.contains(&"main.rs".to_string()));
        assert!(entries.contains(&"lib.rs".to_string()));
    }

    #[test]
    fn memfs_glob_pattern() {
        let fs = make_fs();
        let matches = fs.glob_match("**/*.rs");
        assert_eq!(matches.len(), 2);
        let names: Vec<_> = matches.iter().map(|p| p.display().to_string()).collect();
        assert!(names.iter().any(|n| n.contains("main.rs")));
        assert!(names.iter().any(|n| n.contains("lib.rs")));
    }

    #[test]
    fn memfs_glob_extension_no_match() {
        let fs = make_fs();
        let matches = fs.glob_match("**/*.toml");
        assert!(matches.is_empty());
    }

    #[test]
    fn memfs_grep_content() {
        let fs = make_fs();
        let results = fs.grep_content("fn ", false).unwrap();
        assert!(!results.is_empty());
        let paths: Vec<_> = results
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect();
        assert!(paths
            .iter()
            .any(|p| p.contains("main.rs") || p.contains("lib.rs")));
    }

    #[test]
    fn memfs_grep_case_insensitive() {
        let fs = make_fs();
        let results = fs.grep_content("PROJECT", true).unwrap();
        assert!(!results.is_empty());
    }

    #[test]
    fn memfs_bash_ls() {
        let mut fs = make_fs();
        let (out, code) = fs.exec_shell("ls src");
        assert_eq!(code, 0);
        assert!(out.contains("main.rs"));
        assert!(out.contains("lib.rs"));
    }

    #[test]
    fn memfs_bash_cat() {
        let mut fs = make_fs();
        let (out, code) = fs.exec_shell("cat src/main.rs");
        assert_eq!(code, 0);
        assert!(out.contains("fn main"));
    }

    #[test]
    fn memfs_bash_echo() {
        let mut fs = MemFs::new();
        let (out, code) = fs.exec_shell("echo hello world");
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "hello world");
    }

    #[test]
    fn memfs_bash_pwd() {
        let mut fs = MemFs::new();
        let (out, code) = fs.exec_shell("pwd");
        assert_eq!(code, 0);
        assert!(out.trim().ends_with("workspace"));
    }

    #[test]
    fn memfs_bash_true_false() {
        let mut fs = MemFs::new();
        let (_, code_t) = fs.exec_shell("true");
        let (_, code_f) = fs.exec_shell("false");
        assert_eq!(code_t, 0);
        assert_eq!(code_f, 1);
    }

    #[test]
    fn memfs_bash_unsupported_returns_error() {
        let mut fs = MemFs::new();
        let (out, code) = fs.exec_shell("cargo build");
        assert_eq!(code, 127);
        assert!(out.contains("not supported"));
    }

    #[tokio::test]
    async fn memfs_provider_builds_registry() {
        let provider = MemFsToolSetProvider::new();
        let reg = provider.build_registry();
        let names = reg.names();
        assert!(names.iter().any(|n| n == "Read"), "expected Read");
        assert!(names.iter().any(|n| n == "Write"), "expected Write");
        assert!(names.iter().any(|n| n == "Bash"), "expected Bash");
        assert!(names.iter().any(|n| n == "Glob"), "expected Glob");
        assert!(names.iter().any(|n| n == "Grep"), "expected Grep");
        assert!(names.iter().any(|n| n == "Edit"), "expected Edit");
    }

    #[test]
    fn memfs_provider_sandbox_mode() {
        let provider = MemFsToolSetProvider::new();
        assert_eq!(provider.sandbox_mode(), SandboxMode::None);
    }

    #[tokio::test]
    async fn memfs_tools_have_correct_names() {
        let provider = MemFsToolSetProvider::new();
        let reg = provider.build_registry();
        // Verify that the spec names match the standard tool names exactly.
        let expected = ["Read", "Write", "Edit", "Bash", "Glob", "Grep"];
        for name in &expected {
            assert!(
                reg.find_by_name(name).is_some(),
                "tool {name} not found in registry"
            );
        }
    }

    #[tokio::test]
    async fn memfs_edit_tool_applies_str_replace() {
        let provider = MemFsToolSetProvider::with_files([("test.txt", "hello world")]);
        let reg = provider.build_registry();
        let tool = reg.find_by_name("Edit").expect("Edit tool not found");
        let result = tool
            .execute(json!({"path": "test.txt", "old_string": "world", "new_string": "Rust"}))
            .await;
        assert!(result.is_ok(), "edit failed: {:?}", result);

        // Verify the file was updated.
        let fs = provider.memfs();
        let fs_lock = fs.lock().await;
        let bytes = fs_lock.read(Path::new("test.txt")).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "hello Rust");
    }

    #[tokio::test]
    async fn memfs_read_tool_returns_content() {
        let provider = MemFsToolSetProvider::with_files([("greet.txt", "Hello, MemFs!")]);
        let reg = provider.build_registry();
        let tool = reg.find_by_name("Read").expect("Read tool not found");
        let result = tool.execute(json!({"path": "greet.txt"})).await.unwrap();
        assert_eq!(result, "Hello, MemFs!");
    }
}
