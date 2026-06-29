//! FUSE filesystem backed by a [`WorkspaceStore`].
//!
//! [`WorkspaceFuse`] exposes a [`WorkspaceStore`] as a host directory that
//! other processes (e.g. `virtiofsd`) can read and write. File operations
//! on the mounted directory are transparently persisted to the store.
//!
//! # Design
//!
//! The FUSE filesystem maintains an inode ↔ path mapping in memory. Inodes
//! are allocated monotonically starting from 2 (1 is always the root `/`).
//! The mapping is not persisted — on remount inodes are rebuilt from the
//! store on demand (via `lookup`).
//!
//! # Platform
//!
//! Linux only (`#[cfg(all(target_os = "linux", feature = "workspace-fuse"))]`).

#![cfg(all(target_os = "linux", feature = "workspace-fuse"))]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyWrite, Request,
};

use super::workspace_store::WorkspaceStore;
use crate::error::{Error, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

const ROOT_INO: u64 = 1;
const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 512;

// ─────────────────────────────────────────────────────────────────────────────
// Inode table
// ─────────────────────────────────────────────────────────────────────────────

struct InodeTable {
    /// inode → path (relative to root, starts with `/`)
    ino_to_path: HashMap<u64, PathBuf>,
    /// path → inode
    path_to_ino: HashMap<PathBuf, u64>,
    next_ino: u64,
}

impl InodeTable {
    fn new() -> Self {
        let mut t = Self {
            ino_to_path: HashMap::new(),
            path_to_ino: HashMap::new(),
            next_ino: ROOT_INO + 1,
        };
        // Root is always inode 1.
        t.ino_to_path.insert(ROOT_INO, PathBuf::from("/"));
        t.path_to_ino.insert(PathBuf::from("/"), ROOT_INO);
        t
    }

    fn get_or_alloc(&mut self, path: &Path) -> u64 {
        if let Some(&ino) = self.path_to_ino.get(path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.ino_to_path.insert(ino, path.to_path_buf());
        self.path_to_ino.insert(path.to_path_buf(), ino);
        ino
    }

    fn path_of(&self, ino: u64) -> Option<&PathBuf> {
        self.ino_to_path.get(&ino)
    }

    fn remove(&mut self, path: &Path) {
        if let Some(ino) = self.path_to_ino.remove(path) {
            self.ino_to_path.remove(&ino);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WorkspaceFuse
// ─────────────────────────────────────────────────────────────────────────────

/// FUSE filesystem backed by a [`WorkspaceStore`].
pub struct WorkspaceFuse<S: WorkspaceStore> {
    store: Arc<S>,
    agent_id: String,
    inodes: Mutex<InodeTable>,
}

impl<S: WorkspaceStore> WorkspaceFuse<S> {
    pub fn new(store: Arc<S>, agent_id: impl Into<String>) -> Self {
        Self {
            store,
            agent_id: agent_id.into(),
            inodes: Mutex::new(InodeTable::new()),
        }
    }

    /// Mount at `mountpoint` in a background thread.
    ///
    /// Returns a [`WorkspaceFuseHandle`] that unmounts on drop.
    pub fn mount_background(self, mountpoint: &Path) -> Result<WorkspaceFuseHandle> {
        let mountpoint = mountpoint.to_path_buf();
        std::fs::create_dir_all(&mountpoint).map_err(|e| Error::Config {
            message: format!("create mountpoint {}: {e}", mountpoint.display()),
        })?;

        let mp = mountpoint.clone();
        let options = vec![
            MountOption::FSName("recursive-workspace".into()),
            MountOption::DefaultPermissions,
            MountOption::AutoUnmount,
        ];

        let thread = std::thread::spawn(move || {
            if let Err(e) = fuser::mount2(self, &mp, &options) {
                tracing::warn!("WorkspaceFuse mount exited: {e}");
            }
        });

        Ok(WorkspaceFuseHandle {
            _thread: thread,
            mountpoint,
        })
    }

    // ─── helpers ─────────────────────────────────────────────────────────────

    fn lock_inodes(&self) -> std::sync::MutexGuard<'_, InodeTable> {
        self.inodes.lock().expect("inode table mutex poisoned")
    }

    fn make_attr(&self, ino: u64, path: &Path, is_dir: bool) -> FileAttr {
        let size = if is_dir {
            4096
        } else {
            self.store.file_len(&self.agent_id, path).unwrap_or(0)
        };
        let blocks = size.div_ceil(BLOCK_SIZE as u64);
        let now = SystemTime::now();
        FileAttr {
            ino,
            size,
            blocks,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: UNIX_EPOCH,
            kind: if is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    /// Look up a path in the store and return (is_dir, size) if it exists.
    fn stat_path(&self, path: &Path) -> Option<(bool, u64)> {
        // Try as file first.
        if let Ok(len) = self.store.file_len(&self.agent_id, path) {
            return Some((false, len));
        }
        // Try as directory: list succeeds only when the dir exists.
        if self.store.list_dir(&self.agent_id, path).is_ok() {
            return Some((true, 4096));
        }
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// fuser::Filesystem impl
// ─────────────────────────────────────────────────────────────────────────────

impl<S: WorkspaceStore> Filesystem for WorkspaceFuse<S> {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_path = {
            let t = self.lock_inodes();
            match t.path_of(parent) {
                Some(p) => p.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        let child_path = if parent_path == Path::new("/") {
            PathBuf::from(format!("/{}", name.to_string_lossy()))
        } else {
            parent_path.join(name)
        };

        match self.stat_path(&child_path) {
            Some((is_dir, _)) => {
                let ino = self.lock_inodes().get_or_alloc(&child_path);
                let attr = self.make_attr(ino, &child_path, is_dir);
                reply.entry(&TTL, &attr, 0);
            }
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let path = {
            let t = self.lock_inodes();
            match t.path_of(ino) {
                Some(p) => p.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        if ino == ROOT_INO {
            let attr = self.make_attr(ROOT_INO, Path::new("/"), true);
            reply.attr(&TTL, &attr);
            return;
        }

        match self.stat_path(&path) {
            Some((is_dir, _)) => {
                let attr = self.make_attr(ino, &path, is_dir);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(libc::ENOENT),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let path = {
            let t = self.lock_inodes();
            match t.path_of(ino) {
                Some(p) => p.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        match self.store.read_file(&self.agent_id, &path) {
            Ok(data) => {
                let offset = offset as usize;
                let end = std::cmp::min(offset + size as usize, data.len());
                if offset >= data.len() {
                    reply.data(&[]);
                } else {
                    reply.data(&data[offset..end]);
                }
            }
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let path = {
            let t = self.lock_inodes();
            match t.path_of(ino) {
                Some(p) => p.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        // Read-modify-write to support partial writes at offset.
        let mut existing = self
            .store
            .read_file(&self.agent_id, &path)
            .unwrap_or_default();
        let offset = offset as usize;
        if offset + data.len() > existing.len() {
            existing.resize(offset + data.len(), 0);
        }
        existing[offset..offset + data.len()].copy_from_slice(data);

        match self.store.write_file(&self.agent_id, &path, &existing) {
            Ok(()) => reply.written(data.len() as u32),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let dir_path = {
            let t = self.lock_inodes();
            match t.path_of(ino) {
                Some(p) => p.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        let entries = match self.store.list_dir(&self.agent_id, &dir_path) {
            Ok(e) => e,
            Err(_) => {
                // Root always appears as directory even if empty in the store.
                if ino == ROOT_INO {
                    vec![]
                } else {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        let mut all: Vec<(u64, FileType, String)> = vec![(ino, FileType::Directory, ".".into())];
        // Parent of root is also root.
        all.push((ROOT_INO, FileType::Directory, "..".into()));

        for e in entries {
            let child_path = if dir_path == Path::new("/") {
                PathBuf::from(format!("/{}", e.name))
            } else {
                dir_path.join(&e.name)
            };
            let child_ino = self.lock_inodes().get_or_alloc(&child_path);
            let kind = if e.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            all.push((child_ino, kind, e.name));
        }

        for (i, (child_ino, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(child_ino, (i + 1) as i64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let parent_path = {
            let t = self.lock_inodes();
            match t.path_of(parent) {
                Some(p) => p.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        let child_path = if parent_path == Path::new("/") {
            PathBuf::from(format!("/{}", name.to_string_lossy()))
        } else {
            parent_path.join(name)
        };

        match self.store.write_file(&self.agent_id, &child_path, &[]) {
            Ok(()) => {
                let ino = self.lock_inodes().get_or_alloc(&child_path);
                let attr = self.make_attr(ino, &child_path, false);
                reply.created(&TTL, &attr, 0, 0, 0);
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent_path = {
            let t = self.lock_inodes();
            match t.path_of(parent) {
                Some(p) => p.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        let child_path = if parent_path == Path::new("/") {
            PathBuf::from(format!("/{}", name.to_string_lossy()))
        } else {
            parent_path.join(name)
        };

        match self.store.mkdir(&self.agent_id, &child_path) {
            Ok(()) => {
                let ino = self.lock_inodes().get_or_alloc(&child_path);
                let attr = self.make_attr(ino, &child_path, true);
                reply.entry(&TTL, &attr, 0);
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = {
            let t = self.lock_inodes();
            match t.path_of(parent) {
                Some(p) => p.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        let child_path = if parent_path == Path::new("/") {
            PathBuf::from(format!("/{}", name.to_string_lossy()))
        } else {
            parent_path.join(name)
        };

        let _ = self.store.remove_file(&self.agent_id, &child_path);
        self.lock_inodes().remove(&child_path);
        reply.ok();
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // Handle truncation (size = Some(0) when a file is truncated).
        if let Some(new_size) = _size {
            let path = {
                let t = self.lock_inodes();
                match t.path_of(ino) {
                    Some(p) => p.clone(),
                    None => {
                        reply.error(libc::ENOENT);
                        return;
                    }
                }
            };
            let mut data = self
                .store
                .read_file(&self.agent_id, &path)
                .unwrap_or_default();
            data.resize(new_size as usize, 0);
            if self.store.write_file(&self.agent_id, &path, &data).is_err() {
                reply.error(libc::EIO);
                return;
            }
        }

        // Refresh attr.
        let (path, is_dir) = {
            let t = self.lock_inodes();
            match t.path_of(ino) {
                Some(p) => {
                    let is_dir = self.stat_path(p).map(|(d, _)| d).unwrap_or(false);
                    (p.clone(), is_dir)
                }
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };
        let attr = self.make_attr(ino, &path, is_dir);
        reply.attr(&TTL, &attr);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WorkspaceFuseHandle
// ─────────────────────────────────────────────────────────────────────────────

/// Active FUSE mount. Unmounts when dropped.
pub struct WorkspaceFuseHandle {
    _thread: std::thread::JoinHandle<()>,
    mountpoint: PathBuf,
}

impl Drop for WorkspaceFuseHandle {
    fn drop(&mut self) {
        // Try graceful unmount via fusermount3 / fusermount.
        for bin in &["fusermount3", "fusermount"] {
            if std::process::Command::new(bin)
                .args(["-u", self.mountpoint.to_str().unwrap_or("/")])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                return;
            }
        }
        // Fallback: umount2 via nix (best-effort).
        let _ = std::process::Command::new("umount")
            .arg(self.mountpoint.to_str().unwrap_or("/"))
            .output();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::workspace_store::SqliteWorkspaceStore;

    #[test]
    fn workspace_fuse_new_does_not_panic() {
        let store = Arc::new(SqliteWorkspaceStore::in_memory().unwrap());
        let _fuse = WorkspaceFuse::new(store, "agent-test");
    }

    #[test]
    fn inode_table_root_is_one() {
        let t = InodeTable::new();
        assert_eq!(*t.path_of(ROOT_INO).unwrap(), PathBuf::from("/"));
    }

    #[test]
    fn inode_table_alloc_monotonic() {
        let mut t = InodeTable::new();
        let a = t.get_or_alloc(Path::new("/a"));
        let b = t.get_or_alloc(Path::new("/b"));
        assert!(a > ROOT_INO);
        assert!(b > a);
    }

    #[test]
    fn inode_table_idempotent() {
        let mut t = InodeTable::new();
        let a1 = t.get_or_alloc(Path::new("/foo"));
        let a2 = t.get_or_alloc(Path::new("/foo"));
        assert_eq!(a1, a2);
    }
}
