//! Content-addressed whole-file capture.
//!
//! Every captured file body is stored once under a name derived from the
//! bytes themselves (`<root>/blobs/<xx>/<sha1>`), so identical content is
//! deduplicated across paths, turns and sessions. Blob bodies never leave
//! this store: they are read back only to render the change diff and are
//! never injected into a model message.
//!
//! This module is the *only* capture source when the workspace is not a git
//! repository (or `git` is unavailable). When a shadow index is available it
//! supplies both the file list and the blob bodies instead — see
//! [`super::git_index`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Directories the bounded fallback walk never descends into: VCS metadata
/// (would double-count the ledger's own view), dependency/build output
/// (unbounded and never a deliverable), and Recursive's own per-workspace
/// state dir.
pub const WALK_DENY_DIRS: &[&str] = &[
    ".git",
    ".recursive",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
];

/// SHA-1 of `bytes` as lowercase hex.
///
/// SHA-1 is used because content addressing here is a *naming* function
/// shared with git and with the reference implementation (DSH), not a
/// security primitive. Hand-rolled to avoid taking on a new dependency
/// (invariant #6) — verified against the published test vectors in this
/// module's tests.
pub fn sha1_hex(bytes: &[u8]) -> String {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);

    let mut message = bytes.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

/// What the ledger knows about one file at snapshot time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fingerprint {
    /// Body captured; `sha1` addresses it in the [`CaptureStore`].
    Content { sha1: String, bytes: u64 },
    /// Larger than the capture budget — recorded by size only (the change is
    /// still reported, with `coarse = true`).
    Oversized { bytes: u64 },
    /// Read failed (permissions, dangling symlink, …).
    Unreadable { bytes: u64 },
}

impl Fingerprint {
    /// True when two fingerprints provably describe the same content.
    /// `Oversized`/`Unreadable` compare by size only — an in-place edit
    /// that keeps the size is invisible, which is the documented cost of
    /// skipping the capture.
    pub fn same_content(&self, other: &Fingerprint) -> bool {
        match (self, other) {
            (Fingerprint::Content { sha1: a, .. }, Fingerprint::Content { sha1: b, .. }) => a == b,
            (Fingerprint::Oversized { bytes: a }, Fingerprint::Oversized { bytes: b }) => a == b,
            (Fingerprint::Unreadable { bytes: a }, Fingerprint::Unreadable { bytes: b }) => a == b,
            _ => false,
        }
    }

    pub fn bytes(&self) -> u64 {
        match self {
            Fingerprint::Content { bytes, .. }
            | Fingerprint::Oversized { bytes }
            | Fingerprint::Unreadable { bytes } => *bytes,
        }
    }
}

/// A bounded, content-addressed view of the workspace at one instant.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Workspace-relative path (`/`-separated) → fingerprint.
    pub files: BTreeMap<String, Fingerprint>,
    /// The snapshot is not known to be complete: the `max_files` budget was
    /// hit, or an entry/directory could not be enumerated. Callers must
    /// surface this instead of presenting a partial view as the whole truth.
    pub truncated: bool,
}

/// Write-once, content-addressed blob store.
#[derive(Debug, Clone)]
pub struct CaptureStore {
    root: PathBuf,
}

impl CaptureStore {
    /// Open (creating if necessary) the store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("blobs")).map_err(Error::Io)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, sha1: &str) -> PathBuf {
        let (prefix, rest) = sha1.split_at(sha1.len().min(2));
        self.root.join("blobs").join(prefix).join(rest)
    }

    /// Store `bytes` and return their SHA-1. Storing the same bytes twice
    /// writes one file: the name is the content.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let sha1 = sha1_hex(bytes);
        let path = self.blob_path(&sha1);
        if path.exists() {
            return Ok(sha1);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        std::fs::write(&path, bytes).map_err(Error::Io)?;
        Ok(sha1)
    }

    /// Read back a captured body.
    pub fn get(&self, sha1: &str) -> Result<Option<Vec<u8>>> {
        let path = self.blob_path(sha1);
        if !path.exists() {
            return Ok(None);
        }
        std::fs::read(&path).map(Some).map_err(Error::Io)
    }

    pub fn contains(&self, sha1: &str) -> bool {
        self.blob_path(sha1).exists()
    }
}

/// Capture one file, honouring `max_file_bytes`.
fn capture_file(store: &CaptureStore, abs: &Path, max_file_bytes: u64) -> Fingerprint {
    let Ok(meta) = std::fs::metadata(abs) else {
        return Fingerprint::Unreadable { bytes: 0 };
    };
    if meta.len() > max_file_bytes {
        return Fingerprint::Oversized { bytes: meta.len() };
    }
    match std::fs::read(abs) {
        Ok(bytes) => {
            // Content addressing is what detects a change; storing the body
            // is what lets it be *rendered*. If the store refuses the write,
            // keep the hash anyway (the change is still detected and the
            // render degrades explicitly via a missing blob) instead of
            // mislabelling the file as unreadable.
            let sha1 = store.put(&bytes).unwrap_or_else(|_| sha1_hex(&bytes));
            Fingerprint::Content {
                sha1,
                bytes: bytes.len() as u64,
            }
        }
        Err(_) => Fingerprint::Unreadable { bytes: meta.len() },
    }
}

/// Bounded recursive walk of `workspace` into a [`Snapshot`].
///
/// Used when no shadow index is available. Symlinks are never followed
/// (they could leave the workspace) and [`WALK_DENY_DIRS`] are skipped
/// entirely. The walk stops once `max_files` entries are captured and sets
/// [`Snapshot::truncated`] — a partial ledger is reported as partial, never
/// silently presented as complete.
pub fn walk_snapshot(
    store: &CaptureStore,
    workspace: &Path,
    max_files: usize,
    max_file_bytes: u64,
) -> Snapshot {
    let mut snapshot = Snapshot::default();
    let mut dirs = vec![workspace.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                // An unreadable directory is missing files, not an empty one.
                snapshot.truncated = true;
                continue;
            }
        };
        let mut children: Vec<PathBuf> = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => children.push(entry.path()),
                Err(_) => snapshot.truncated = true,
            }
        }
        children.sort();
        for child in children {
            if snapshot.files.len() >= max_files {
                snapshot.truncated = true;
                return snapshot;
            }
            let name = child
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let Ok(meta) = std::fs::symlink_metadata(&child) else {
                snapshot.truncated = true;
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if WALK_DENY_DIRS.contains(&name.as_str()) {
                    continue;
                }
                dirs.push(child);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let rel = match child.strip_prefix(workspace) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                Err(_) => {
                    // Not reachable for descendants of `workspace`, but a
                    // path we cannot name is a file we cannot report.
                    snapshot.truncated = true;
                    continue;
                }
            };
            let fingerprint = capture_file(store, &child, max_file_bytes);
            snapshot.files.insert(rel, fingerprint);
        }
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sha1_matches_published_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
        // Exactly one padding block: 55 bytes of 'a'.
        assert_eq!(
            sha1_hex(&b"a".repeat(55)),
            "c1c8bbdc22796e28c0e15163d20899b65621d65a"
        );
    }

    #[test]
    fn put_is_write_once_and_content_addressed() {
        let tmp = TempDir::new().unwrap();
        let store = CaptureStore::open(tmp.path()).unwrap();
        let a = store.put(b"hello").unwrap();
        let b = store.put(b"hello").unwrap();
        assert_eq!(a, b, "identical bytes must resolve to one address");
        assert_ne!(a, store.put(b"world").unwrap());
        assert_eq!(store.get(&a).unwrap().as_deref(), Some(&b"hello"[..]));
        assert!(store.contains(&a));
        assert_eq!(store.get("deadbeef").unwrap(), None);
    }

    #[test]
    fn walk_captures_content_and_ignores_deny_dirs() {
        let root = TempDir::new().unwrap();
        let tmp = root.path().join("ws");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.txt"), "one").unwrap();
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("sub/b.txt"), "one").unwrap();
        std::fs::create_dir_all(tmp.join("target")).unwrap();
        std::fs::write(tmp.join("target/junk"), "junk").unwrap();
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        std::fs::write(tmp.join(".git/config"), "x").unwrap();

        let store = CaptureStore::open(root.path().join("store")).unwrap();
        let snap = walk_snapshot(&store, &tmp, 500, 1024);
        let paths: Vec<&String> = snap.files.keys().collect();
        assert_eq!(paths, vec!["a.txt", "sub/b.txt"]);
        assert!(!snap.truncated);
        // Identical bodies across paths share one blob.
        let first = snap.files.get("a.txt").unwrap();
        let second = snap.files.get("sub/b.txt").unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_dir(store.root().join("blobs/"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn walk_marks_truncation_instead_of_silently_dropping_files() {
        let root = TempDir::new().unwrap();
        let tmp = root.path().join("ws");
        std::fs::create_dir_all(&tmp).unwrap();
        for i in 0..5 {
            std::fs::write(tmp.join(format!("f{i}.txt")), "x").unwrap();
        }
        let store = CaptureStore::open(root.path().join("store")).unwrap();
        let snap = walk_snapshot(&store, &tmp, 3, 1024);
        assert_eq!(snap.files.len(), 3);
        assert!(snap.truncated, "over-budget walks must be flagged");
    }

    #[test]
    fn oversized_files_are_fingerprinted_without_being_read() {
        let root = TempDir::new().unwrap();
        let tmp = root.path().join("ws");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("big.bin"), vec![b'x'; 512]).unwrap();
        let store = CaptureStore::open(root.path().join("store")).unwrap();
        let snap = walk_snapshot(&store, &tmp, 10, 64);
        assert_eq!(
            snap.files.get("big.bin"),
            Some(&Fingerprint::Oversized { bytes: 512 })
        );
        assert_eq!(
            std::fs::read_dir(store.root().join("blobs/"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn symlinks_are_not_followed() {
        let root = TempDir::new().unwrap();
        let tmp = root.path().join("ws");
        std::fs::create_dir_all(&tmp).unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), tmp.join("link")).unwrap();
        let store = CaptureStore::open(root.path().join("store")).unwrap();
        let snap = walk_snapshot(&store, &tmp, 10, 1024);
        assert!(snap.files.is_empty(), "{:?}", snap.files);
    }

    #[test]
    fn oversized_and_content_fingerprints_never_compare_equal() {
        let content = Fingerprint::Content {
            sha1: "a".into(),
            bytes: 8,
        };
        assert!(content.same_content(&content.clone()));
        assert!(!content.same_content(&Fingerprint::Oversized { bytes: 8 }));
        assert!(!content.same_content(&Fingerprint::Unreadable { bytes: 8 }));
        assert!(Fingerprint::Unreadable { bytes: 9 }
            .same_content(&Fingerprint::Unreadable { bytes: 9 }));
        assert!(!Fingerprint::Unreadable { bytes: 9 }
            .same_content(&Fingerprint::Unreadable { bytes: 8 }));
    }
}
