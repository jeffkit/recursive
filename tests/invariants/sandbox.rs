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

/// `resolve_within` must reject a *new* file reached through a symlinked
/// parent directory. The link itself is inside the root; following it lands
/// outside. Before the ancestor fix only the already-existing file was caught.
#[cfg(unix)]
#[test]
fn resolve_within_rejects_new_file_through_symlinked_parent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), tmp.path().join("docs")).unwrap();

    let result = recursive::tools::resolve_within(tmp.path(), "docs/new.txt");
    assert!(
        result.is_err(),
        "a new file under a symlinked parent must be rejected; got {:?}",
        result
    );
}

/// The same link, but the target file already exists — this case was already
/// rejected and must stay that way.
#[cfg(unix)]
#[test]
fn resolve_within_rejects_existing_file_through_symlinked_parent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("exists.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(outside.path(), tmp.path().join("docs")).unwrap();

    let result = recursive::tools::resolve_within(tmp.path(), "docs/exists.txt");
    assert!(
        result.is_err(),
        "an existing file under a symlinked parent must be rejected; got {:?}",
        result
    );
}

/// A *dangling* symlink as the final component must be rejected: the link is
/// a directory entry, its target is not. `exists()` is blind to this (it
/// follows the link), so a write through it would create the target outside.
#[cfg(unix)]
#[test]
fn resolve_within_rejects_dangling_symlink_leaf() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    let target = outside.path().join("created_by_agent.txt");
    std::os::unix::fs::symlink(&target, tmp.path().join("notes")).unwrap();
    assert!(
        !target.exists(),
        "precondition: the link target must be absent"
    );

    let result = recursive::tools::resolve_within(tmp.path(), "notes");
    assert!(
        result.is_err(),
        "a dangling symlink leaf must be rejected; got {:?}",
        result
    );
}

/// A root that does not exist on disk yet is not an escape: it must resolve to
/// itself (through any symlinked parent) just like the candidate does, instead
/// of being rejected as a symlink escape.
#[cfg(unix)]
#[test]
fn resolve_within_allows_uncreated_root_under_symlinked_parent() {
    let holder = tempfile::TempDir::new().unwrap();
    let real = holder.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = holder.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let root = link.join("not_created_yet");
    assert!(!root.exists(), "precondition: the root must not exist");

    let result = recursive::tools::resolve_within(&root, "a.txt");
    assert!(
        result.is_ok(),
        "an un-materialised root must not be rejected; got {:?}",
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

/// The `WriteFile` tool must not write through a symlinked parent directory —
/// neither the file nor any parent directory may appear outside the sandbox.
#[cfg(unix)]
#[tokio::test]
async fn writefile_rejects_symlinked_parent_dir() {
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), tmp.path().join("docs")).unwrap();

    let tool = recursive::tools::WriteFile::new(tmp.path());
    let args = serde_json::json!({"path": "docs/new.txt", "contents": "escaped"});
    let result = tool.execute(args).await;
    assert!(
        result.is_err(),
        "WriteFile must reject a symlinked parent dir; got {:?}",
        result
    );
    assert!(
        !outside.path().join("new.txt").exists(),
        "no file may be created outside the sandbox"
    );

    let args = serde_json::json!({"path": "docs/.ssh/newdir/x", "contents": "escaped"});
    let result = tool.execute(args).await;
    assert!(
        result.is_err(),
        "WriteFile must reject nested creation under a symlinked parent; got {:?}",
        result
    );
    assert!(
        !outside.path().join(".ssh").exists(),
        "no directory may be created outside the sandbox"
    );
}

/// A dangling symlink *leaf* must not let `Write` create the target outside
/// the sandbox — `Path::exists` is false for such a link, so this used to slip
/// past the symlink check and leave a file outside the workspace.
#[cfg(unix)]
#[tokio::test]
async fn writefile_rejects_dangling_symlink_leaf() {
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let target = outside.path().join("created_by_agent.txt");
    std::os::unix::fs::symlink(&target, tmp.path().join("notes")).unwrap();

    let tool = recursive::tools::WriteFile::new(tmp.path());
    let args = serde_json::json!({"path": "notes", "contents": "escaped"});
    let result = tool.execute(args).await;
    assert!(
        result.is_err(),
        "Write must reject a dangling symlink leaf; got {:?}",
        result
    );
    assert!(
        !target.exists(),
        "no file may be created outside the sandbox"
    );
}

/// The same dangling leaf must not let `Edit` write outside the sandbox.
#[cfg(unix)]
#[tokio::test]
async fn edit_rejects_dangling_symlink_leaf() {
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let target = outside.path().join("created_by_agent.txt");
    std::os::unix::fs::symlink(&target, tmp.path().join("notes")).unwrap();

    let tool = recursive::tools::EditTool::new(tmp.path());
    let args = serde_json::json!({
        "file_path": "notes",
        "old_string": "foo",
        "new_string": "bar"
    });
    let result = tool.execute(args).await;
    assert!(
        result.is_err(),
        "Edit must reject a dangling symlink leaf; got {:?}",
        result
    );
    assert!(
        !target.exists(),
        "no file may be created outside the sandbox"
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
