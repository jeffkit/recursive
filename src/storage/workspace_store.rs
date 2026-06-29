//! Agent-scoped, persistent workspace file storage.
//!
//! [`WorkspaceStore`] is a synchronous, thread-safe abstraction for
//! file-system-like operations stored in a durable backend.
//!
//! [`SqliteWorkspaceStore`] implements it with a bundled SQLite database
//! (via `rusqlite`). The same database that already backs vector memory
//! can be reused — each sub-feature uses distinct table names.
//!
//! # Multi-tenancy
//!
//! All operations are scoped by an `agent_id` string, so multiple agents
//! can share a single SQLite file without cross-contamination.
//!
//! # Usage in `FirecrackerVm`
//!
//! ```text
//! SqliteWorkspaceStore  (persists to ~/recursive/workspaces.db)
//!   └─ WorkspaceFuse    (mounts as FUSE dir at /tmp/ws-<agent>)
//!        └─ virtiofsd   (shares FUSE dir into the Firecracker VM)
//!             └─ VM /workspace
//! ```

use std::path::Path;
use std::sync::{Arc, Mutex};

#[cfg(feature = "workspace-store")]
use rusqlite::{params, Connection};

use crate::error::{Error, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// A directory entry returned by [`WorkspaceStore::list_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    /// File or directory name (not a full path).
    pub name: String,
    /// `true` for directories, `false` for regular files.
    pub is_dir: bool,
    /// File size in bytes. Always `0` for directories.
    pub size: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// WorkspaceStore trait
// ─────────────────────────────────────────────────────────────────────────────

/// Persistent, agent-scoped workspace file storage.
///
/// All operations are synchronous to allow use from FUSE callback threads,
/// which cannot call async code. Implementations must be `Send + Sync`.
pub trait WorkspaceStore: Send + Sync + 'static {
    /// Read the full content of a file.
    ///
    /// Returns [`Error::Tool`] with an `ENOENT`-like message when the file
    /// does not exist.
    fn read_file(&self, agent_id: &str, path: &Path) -> Result<Vec<u8>>;

    /// Write (create or overwrite) a file with the given content.
    fn write_file(&self, agent_id: &str, path: &Path, data: &[u8]) -> Result<()>;

    /// List the immediate children of a directory.
    ///
    /// Returns an empty `Vec` when the directory exists but is empty.
    /// Returns [`Error::Tool`] when the directory does not exist.
    fn list_dir(&self, agent_id: &str, dir: &Path) -> Result<Vec<PathEntry>>;

    /// Delete a file. Idempotent (no error if the file does not exist).
    fn remove_file(&self, agent_id: &str, path: &Path) -> Result<()>;

    /// Create a directory (and any missing parents, like `mkdir -p`).
    fn mkdir(&self, agent_id: &str, dir: &Path) -> Result<()>;

    /// Return the byte length of a file.
    ///
    /// Returns [`Error::Tool`] when the file does not exist.
    fn file_len(&self, agent_id: &str, path: &Path) -> Result<u64>;
}

// ─────────────────────────────────────────────────────────────────────────────
// SqliteWorkspaceStore
// ─────────────────────────────────────────────────────────────────────────────

/// SQLite-backed [`WorkspaceStore`].
///
/// Files and directories are stored in a single table:
/// ```sql
/// workspace_files(agent_id TEXT, path TEXT, content BLOB, is_dir BOOL,
///                 created_at INTEGER, updated_at INTEGER,
///                 PRIMARY KEY (agent_id, path))
/// ```
///
/// `rusqlite::Connection` is not `Send`, so it is wrapped in
/// `Arc<Mutex<Connection>>` to allow concurrent access from FUSE callbacks.
#[cfg(feature = "workspace-store")]
pub struct SqliteWorkspaceStore {
    db: Arc<Mutex<Connection>>,
}

#[cfg(feature = "workspace-store")]
impl SqliteWorkspaceStore {
    /// Open (or create) a persistent SQLite database at `db_path`.
    pub fn new(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path).map_err(|e| Error::Config {
            message: format!("workspace store open {}: {e}", db_path.display()),
        })?;
        let store = Self {
            db: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database. Useful for tests.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Config {
            message: format!("workspace store open in-memory: {e}"),
        })?;
        let store = Self {
            db: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        let db = self.lock()?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspace_files (
                agent_id   TEXT    NOT NULL,
                path       TEXT    NOT NULL,
                content    BLOB    NOT NULL DEFAULT x'',
                is_dir     INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                PRIMARY KEY (agent_id, path)
            );
            CREATE INDEX IF NOT EXISTS workspace_files_agent_dir
                ON workspace_files(agent_id, path);",
        )
        .map_err(|e| Error::Config {
            message: format!("workspace store init schema: {e}"),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.db.lock().map_err(|_| Error::Config {
            message: "workspace store mutex poisoned".into(),
        })
    }

    /// Normalise a path to a canonical `/`-separated string.
    ///
    /// `/` → `"/"`, `/foo/` → `"/foo"`.
    fn normalise(path: &Path) -> String {
        let s = path.to_string_lossy();
        // Ensure leading slash.
        let s = if s.starts_with('/') {
            s.into_owned()
        } else {
            format!("/{s}")
        };
        // Trim trailing slashes, but keep the root `/` intact.
        let trimmed = s.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    }

    /// Return the parent path string, e.g. `"/a/b/c"` → `"/a/b"`.
    fn parent_path(norm: &str) -> &str {
        match norm.rfind('/') {
            Some(0) | None => "/",
            Some(i) => &norm[..i],
        }
    }

    /// Ensure all ancestor directories exist (like mkdir -p).
    fn ensure_parents(&self, agent_id: &str, norm_path: &str) -> Result<()> {
        let db = self.lock()?;
        let mut current = norm_path;
        loop {
            let parent = Self::parent_path(current);
            if parent == current {
                break;
            }
            db.execute(
                "INSERT OR IGNORE INTO workspace_files(agent_id, path, is_dir, content)
                 VALUES (?1, ?2, 1, x'')",
                params![agent_id, parent],
            )
            .map_err(|e| Error::Config {
                message: format!("workspace ensure parent {parent}: {e}"),
            })?;
            current = parent;
        }
        Ok(())
    }
}

#[cfg(feature = "workspace-store")]
impl WorkspaceStore for SqliteWorkspaceStore {
    fn read_file(&self, agent_id: &str, path: &Path) -> Result<Vec<u8>> {
        let norm = Self::normalise(path);
        let db = self.lock()?;
        db.query_row(
            "SELECT content FROM workspace_files
             WHERE agent_id = ?1 AND path = ?2 AND is_dir = 0",
            params![agent_id, norm],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map_err(|_| Error::Tool {
            name: "WorkspaceStore".into(),
            call_id: None,
            message: format!("file not found: {norm}"),
        })
    }

    fn write_file(&self, agent_id: &str, path: &Path, data: &[u8]) -> Result<()> {
        let norm = Self::normalise(path);
        self.ensure_parents(agent_id, &norm)?;
        let db = self.lock()?;
        db.execute(
            "INSERT INTO workspace_files(agent_id, path, content, is_dir, updated_at)
             VALUES (?1, ?2, ?3, 0, strftime('%s','now'))
             ON CONFLICT(agent_id, path) DO UPDATE SET
               content = excluded.content,
               is_dir  = 0,
               updated_at = strftime('%s','now')",
            params![agent_id, norm, data],
        )
        .map_err(|e| Error::Tool {
            name: "WorkspaceStore".into(),
            call_id: None,
            message: format!("write_file {norm}: {e}"),
        })?;
        Ok(())
    }

    fn list_dir(&self, agent_id: &str, dir: &Path) -> Result<Vec<PathEntry>> {
        let norm = Self::normalise(dir);
        let db = self.lock()?;

        // Check directory exists (root "/" always exists logically).
        if norm != "/" {
            let count: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM workspace_files
                     WHERE agent_id = ?1 AND path = ?2 AND is_dir = 1",
                    params![agent_id, norm],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if count == 0 {
                return Err(Error::Tool {
                    name: "WorkspaceStore".into(),
                    call_id: None,
                    message: format!("directory not found: {norm}"),
                });
            }
        }

        // Select immediate children: paths that start with `norm/` and have
        // no additional `/` after the prefix.
        let prefix = if norm == "/" {
            "/".to_string()
        } else {
            format!("{norm}/")
        };
        let mut stmt = db
            .prepare(
                "SELECT path, is_dir, length(content) as size
                 FROM workspace_files
                 WHERE agent_id = ?1
                   AND path LIKE ?2 ESCAPE '\\'
                   AND path != ?3",
            )
            .map_err(|e| Error::Config {
                message: format!("workspace list_dir prepare: {e}"),
            })?;

        let like_pattern = format!(
            "{}%",
            prefix
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );

        let entries = stmt
            .query_map(params![agent_id, like_pattern, norm], |row| {
                let path_str: String = row.get(0)?;
                let is_dir: bool = row.get(1)?;
                let size: u64 = row.get(2).unwrap_or(0);
                Ok((path_str, is_dir, size))
            })
            .map_err(|e| Error::Config {
                message: format!("workspace list_dir query: {e}"),
            })?;

        let mut results = Vec::new();
        for entry in entries {
            let (path_str, is_dir, size) = entry.map_err(|e| Error::Config {
                message: format!("workspace list_dir row: {e}"),
            })?;
            // Only immediate children: strip prefix, check no more slashes.
            if let Some(rest) = path_str.strip_prefix(&prefix) {
                if !rest.is_empty() && !rest.contains('/') {
                    results.push(PathEntry {
                        name: rest.to_string(),
                        is_dir,
                        size,
                    });
                }
            }
        }
        Ok(results)
    }

    fn remove_file(&self, agent_id: &str, path: &Path) -> Result<()> {
        let norm = Self::normalise(path);
        let db = self.lock()?;
        db.execute(
            "DELETE FROM workspace_files WHERE agent_id = ?1 AND path = ?2 AND is_dir = 0",
            params![agent_id, norm],
        )
        .map_err(|e| Error::Tool {
            name: "WorkspaceStore".into(),
            call_id: None,
            message: format!("remove_file {norm}: {e}"),
        })?;
        Ok(())
    }

    fn mkdir(&self, agent_id: &str, dir: &Path) -> Result<()> {
        let norm = Self::normalise(dir);
        self.ensure_parents(agent_id, &norm)?;
        let db = self.lock()?;
        db.execute(
            "INSERT OR IGNORE INTO workspace_files(agent_id, path, is_dir, content)
             VALUES (?1, ?2, 1, x'')",
            params![agent_id, norm],
        )
        .map_err(|e| Error::Tool {
            name: "WorkspaceStore".into(),
            call_id: None,
            message: format!("mkdir {norm}: {e}"),
        })?;
        Ok(())
    }

    fn file_len(&self, agent_id: &str, path: &Path) -> Result<u64> {
        let norm = Self::normalise(path);
        let db = self.lock()?;
        db.query_row(
            "SELECT length(content) FROM workspace_files
             WHERE agent_id = ?1 AND path = ?2 AND is_dir = 0",
            params![agent_id, norm],
            |row| row.get::<_, u64>(0),
        )
        .map_err(|_| Error::Tool {
            name: "WorkspaceStore".into(),
            call_id: None,
            message: format!("file not found: {norm}"),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "workspace-store"))]
mod tests {
    use super::*;

    fn store() -> SqliteWorkspaceStore {
        SqliteWorkspaceStore::in_memory().unwrap()
    }

    #[test]
    fn sqlite_workspace_store_write_read() {
        let s = store();
        s.write_file("agent1", Path::new("/hello.txt"), b"world")
            .unwrap();
        let content = s.read_file("agent1", Path::new("/hello.txt")).unwrap();
        assert_eq!(content, b"world");
    }

    #[test]
    fn sqlite_workspace_store_overwrite() {
        let s = store();
        s.write_file("a", Path::new("/f.txt"), b"v1").unwrap();
        s.write_file("a", Path::new("/f.txt"), b"v2").unwrap();
        assert_eq!(s.read_file("a", Path::new("/f.txt")).unwrap(), b"v2");
    }

    #[test]
    fn sqlite_workspace_store_list_dir() {
        let s = store();
        s.mkdir("a", Path::new("/src")).unwrap();
        s.write_file("a", Path::new("/src/main.rs"), b"fn main(){}")
            .unwrap();
        s.write_file("a", Path::new("/src/lib.rs"), b"").unwrap();
        let mut entries = s.list_dir("a", Path::new("/src")).unwrap();
        entries.sort_by_key(|e| e.name.clone());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "lib.rs");
        assert_eq!(entries[1].name, "main.rs");
    }

    #[test]
    fn sqlite_workspace_store_agent_isolation() {
        let s = store();
        s.write_file("agent-a", Path::new("/secret.txt"), b"secret")
            .unwrap();
        let result = s.read_file("agent-b", Path::new("/secret.txt"));
        assert!(result.is_err(), "agent-b should not see agent-a's file");
    }

    #[test]
    fn sqlite_workspace_store_in_memory() {
        let s = SqliteWorkspaceStore::in_memory().unwrap();
        s.write_file("x", Path::new("/a"), b"b").unwrap();
        assert_eq!(s.file_len("x", Path::new("/a")).unwrap(), 1);
    }

    #[test]
    fn sqlite_workspace_store_mkdir_and_list() {
        let s = store();
        s.mkdir("a", Path::new("/projects/rust")).unwrap();
        // Parent "/projects" should also have been created.
        let top = s.list_dir("a", Path::new("/")).unwrap();
        assert!(top.iter().any(|e| e.name == "projects" && e.is_dir));
    }

    #[test]
    fn sqlite_workspace_store_remove_file() {
        let s = store();
        s.write_file("a", Path::new("/tmp.txt"), b"x").unwrap();
        s.remove_file("a", Path::new("/tmp.txt")).unwrap();
        assert!(s.read_file("a", Path::new("/tmp.txt")).is_err());
    }

    #[test]
    fn sqlite_workspace_store_file_not_found_error() {
        let s = store();
        let e = s.read_file("a", Path::new("/nonexistent")).unwrap_err();
        assert!(e.to_string().contains("not found") || e.to_string().contains("nonexistent"));
    }
}
