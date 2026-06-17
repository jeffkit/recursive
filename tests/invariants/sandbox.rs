// Why this test exists:
// .dev/AGENTS.md invariant #3: "Sandbox. Every fs / shell tool resolves paths
// through `tools::resolve_within`. Never bypass it."
//
// This test verifies:
// - `resolve_within` rejects `../` path traversal
// - `resolve_within` rejects absolute paths outside the workspace
// - `resolve_within` rejects symlink escapes
// - The fs tools (Read, Write, Edit) all route through `resolve_within`
//
// Run with: `cargo test --test invariants`

use std::path::PathBuf;

use recursive::Tool;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ── Test helpers ───────────────────────────────────────────────────────────

/// Create a temporary directory with a symlink pointing outside.
/// Returns (temp_dir, symlink_name, outside_target).
#[cfg(unix)]
fn setup_symlink_trap() -> (tempfile::TempDir, String, PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let outside = tmp.path().join("../outside_target");
    std::fs::create_dir_all(&outside).unwrap();
    let symlink_name = "escape_link";
    let symlink_path = tmp.path().join(symlink_name);
    std::os::unix::fs::symlink(&outside, &symlink_path).unwrap();
    (tmp, symlink_name.to_string(), outside)
}

// ── Invariant #3: Sandbox ──────────────────────────────────────────────────

/// `resolve_within` must reject `../` path traversal — even if the
/// path does not exist on disk yet.
#[test]
fn resolve_within_rejects_parent_traversal() {
    let root = std::path::Path::new("/workspace/project");
    let result = recursive::tools::resolve_within(root, "../etc/passwd");
    assert!(
        result.is_err(),
        "resolve_within must reject `../` traversal; got {:?}",
        result
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("escapes"),
        "error must mention 'escapes': {err}"
    );
}

/// `resolve_within` must reject absolute paths outside the root.
#[test]
fn resolve_within_rejects_absolute_outside() {
    let root = std::path::Path::new("/workspace/project");
    let result = recursive::tools::resolve_within(root, "/etc/passwd");
    assert!(
        result.is_err(),
        "resolve_within must reject absolute paths outside root"
    );
}

/// `resolve_within` must reject paths that canonicalize via symlink to
/// a location outside the root.
#[cfg(unix)]
#[test]
fn resolve_within_rejects_symlink_escape() {
    let (tmp, symlink_name, _outside) = setup_symlink_trap();
    // symlink points to ../outside_target, which is outside the temp dir.
    let result = recursive::tools::resolve_within(tmp.path(), &symlink_name);
    assert!(
        result.is_err(),
        "resolve_within must reject symlink escape; got {:?}",
        result
    );
}

/// `resolve_within` must accept normal relative paths inside the root.
#[test]
fn resolve_within_allows_normal_relative_path() {
    let root = workspace_root();
    let result = recursive::tools::resolve_within(&root, "src/lib.rs");
    assert!(
        result.is_ok(),
        "resolve_within must allow normal relative paths; got {:?}",
        result
    );
    let resolved = result.unwrap();
    assert!(
        resolved.ends_with("src/lib.rs"),
        "resolved path must end with src/lib.rs: {resolved:?}"
    );
}

/// `resolve_within` must handle a relative workspace root (e.g. `.`).
#[test]
fn resolve_within_handles_relative_root() {
    let cwd = std::env::current_dir().unwrap();
    let result = recursive::tools::resolve_within(std::path::Path::new("."), "src/lib.rs");
    assert!(result.is_ok(), "resolve_within must handle relative root");
    let resolved = result.unwrap();
    assert!(
        resolved.starts_with(&cwd),
        "resolved path must start with cwd: {resolved:?}"
    );
    assert!(
        resolved.ends_with("src/lib.rs"),
        "resolved path must end with src/lib.rs: {resolved:?}"
    );
}

/// `resolve_within` must reject deep parent traversal like `../../../../etc/passwd`.
#[test]
fn resolve_within_rejects_deep_traversal() {
    let root = std::path::Path::new("/workspace/project");
    let result = recursive::tools::resolve_within(root, "../../../../etc/passwd");
    assert!(
        result.is_err(),
        "resolve_within must reject deep `../` traversal"
    );
}

/// `resolve_within` must reject traversal with mixed components like
/// `subdir/../../../etc/passwd`.
#[test]
fn resolve_within_rejects_mixed_traversal() {
    let root = std::path::Path::new("/workspace/project");
    let result = recursive::tools::resolve_within(root, "subdir/../../../etc/passwd");
    assert!(
        result.is_err(),
        "resolve_within must reject mixed-path traversal"
    );
}

// ── Tool-level sandbox tests ───────────────────────────────────────────────

/// The `ReadFile` tool must reject paths that escape the workspace.
#[tokio::test]
async fn readfile_rejects_escape() {
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();
    let tool = recursive::tools::ReadFile::new(tmp.path());
    let args = serde_json::json!({"path": "../outside_file.txt"});
    let result = tool.execute(args).await;
    assert!(
        result.is_err(),
        "ReadFile must reject ../ escape; got {:?}",
        result
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("escapes") || err.contains("outside") || err.contains("path"),
        "ReadFile error must mention path violation: {err}"
    );
}

/// The `WriteFile` tool must reject paths that escape the workspace.
#[tokio::test]
async fn writefile_rejects_escape() {
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();
    let tool = recursive::tools::WriteFile::new(tmp.path());
    let args = serde_json::json!({
        "path": "../outside_file.txt",
        "contents": "malicious content"
    });
    let result = tool.execute(args).await;
    assert!(
        result.is_err(),
        "WriteFile must reject ../ escape; got {:?}",
        result
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("escapes") || err.contains("outside") || err.contains("path"),
        "WriteFile error must mention path violation: {err}"
    );
}

/// The `Edit` tool must reject paths that escape the workspace.
#[tokio::test]
async fn edit_rejects_escape() {
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();
    let tool = recursive::tools::EditTool::new(tmp.path());
    let args = serde_json::json!({
        "file_path": "../outside_file.txt",
        "old_string": "foo",
        "new_string": "bar"
    });
    let result = tool.execute(args).await;
    assert!(
        result.is_err(),
        "Edit must reject ../ escape; got {:?}",
        result
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("escapes") || err.contains("outside") || err.contains("path"),
        "Edit error must mention path violation: {err}"
    );
}

/// The `Glob` tool must reject paths that escape the workspace.
#[tokio::test]
async fn glob_rejects_escape() {
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();
    let tool = recursive::tools::GlobTool::new(tmp.path());
    let args = serde_json::json!({"path": "../outside", "pattern": "*.rs"});
    let result = tool.execute(args).await;
    assert!(
        result.is_err(),
        "Glob must reject ../ escape; got {:?}",
        result
    );
}

/// The `CountLines` tool must reject paths that escape the workspace.
#[tokio::test]
async fn countlines_rejects_escape() {
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();
    let tool = recursive::tools::CountLines::new(tmp.path());
    let args = serde_json::json!({"path": "../outside_file.txt"});
    let result = tool.execute(args).await;
    assert!(
        result.is_err(),
        "CountLines must reject ../ escape; got {:?}",
        result
    );
}
