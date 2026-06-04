//! Per-session log of turn-level checkpoint records.
//!
//! Each session writes a `checkpoints.jsonl` file alongside its
//! transcript. One line = one turn, capturing the pre/post checkpoint
//! ids and the set of files this turn touched (via structured tool
//! calls and/or fallback shell-diff).
//!
//! This metadata is what `recursive sessions rewind` reads to:
//!  1. find the right `pre` checkpoint to restore to,
//!  2. compute the union of files touched in the rewound-away turns,
//!  3. detect conflicts where the workspace's current state diverges
//!     from this session's last known post-snapshot.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::checkpoint::CheckpointId;
use crate::error::{Error, Result};

/// One record per turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRecord {
    /// 0-indexed turn within the session.
    pub turn: usize,
    /// Checkpoint id captured at the start of the turn (before LLM
    /// + tools). May be absent for the very first turn of legacy
    ///   sessions.
    pub pre: Option<CheckpointId>,
    /// Checkpoint id captured at the end of the turn.
    pub post: CheckpointId,
    /// Workspace-relative paths the agent touched this turn.
    pub touched_files: Vec<String>,
    /// "structured" if all touched files came from typed tool args;
    /// "shell-diff" if at least one file was attributed via fallback
    /// pre/post tree diff after a `run_shell` call.
    pub touched_via: TouchedVia,
    /// Unix seconds.
    pub started_at: i64,
    /// Unix seconds.
    pub finished_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TouchedVia {
    Structured,
    ShellDiff,
}

/// Append-only writer.
#[derive(Debug, Clone)]
pub struct CheckpointLogWriter {
    path: PathBuf,
}

impl CheckpointLogWriter {
    /// Open or create the log at `path`. Parent directory must exist.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        // Touch the file if missing.
        let _ = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(Error::Io)?;
        Ok(Self { path })
    }

    /// Append a record. Each call performs an O_APPEND fsync-less write;
    /// records are durable on flush.
    pub fn append(&self, rec: &CheckpointRecord) -> Result<()> {
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
            .map_err(Error::Io)?;
        let mut w = BufWriter::new(&mut f);
        serde_json::to_writer(&mut w, rec).map_err(Error::Json)?;
        w.write_all(b"\n").map_err(Error::Io)?;
        w.flush().map_err(Error::Io)?;
        Ok(())
    }
}

/// Read all records from `path`. Empty / missing file → empty vec.
pub fn read_log(path: &Path) -> Result<Vec<CheckpointRecord>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let f = File::open(path).map_err(Error::Io)?;
    let r = BufReader::new(f);
    let mut out = Vec::new();
    for (i, line) in r.lines().enumerate() {
        let line = line.map_err(Error::Io)?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: CheckpointRecord = serde_json::from_str(&line).map_err(|e| Error::Tool {
            name: "checkpoint-log".into(),
            message: format!("malformed log line {}: {e}", i + 1),
        })?;
        out.push(rec);
    }
    Ok(out)
}

/// Truncate the log to records with `turn < cutoff`. Atomic via temp
/// file + rename.
pub fn truncate_to_turn(path: &Path, cutoff: usize) -> Result<()> {
    let recs = read_log(path)?;
    let kept: Vec<&CheckpointRecord> = recs.iter().filter(|r| r.turn < cutoff).collect();

    let tmp = path.with_extension("jsonl.tmp");
    {
        let f = File::create(&tmp).map_err(Error::Io)?;
        let mut w = BufWriter::new(f);
        for r in &kept {
            serde_json::to_writer(&mut w, r).map_err(Error::Json)?;
            w.write_all(b"\n").map_err(Error::Io)?;
        }
        w.flush().map_err(Error::Io)?;
    }
    std::fs::rename(&tmp, path).map_err(Error::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(turn: usize, post: &str) -> CheckpointRecord {
        CheckpointRecord {
            turn,
            pre: None,
            post: CheckpointId(post.to_string()),
            touched_files: vec!["a.txt".into()],
            touched_via: TouchedVia::Structured,
            started_at: 0,
            finished_at: 0,
        }
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoints.jsonl");
        let w = CheckpointLogWriter::open(&path).unwrap();
        w.append(&rec(0, "aaa")).unwrap();
        w.append(&rec(1, "bbb")).unwrap();
        let read = read_log(&path).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].turn, 0);
        assert_eq!(read[1].post.0, "bbb");
    }

    #[test]
    fn read_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        assert!(read_log(&path).unwrap().is_empty());
    }

    #[test]
    fn truncate_drops_later_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.jsonl");
        let w = CheckpointLogWriter::open(&path).unwrap();
        for t in 0..5 {
            w.append(&rec(t, &format!("p{t}"))).unwrap();
        }
        truncate_to_turn(&path, 3).unwrap();
        let kept = read_log(&path).unwrap();
        assert_eq!(kept.len(), 3);
        assert!(kept.iter().all(|r| r.turn < 3));
    }

    #[test]
    fn truncate_to_zero_empties_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.jsonl");
        let w = CheckpointLogWriter::open(&path).unwrap();
        w.append(&rec(0, "x")).unwrap();
        truncate_to_turn(&path, 0).unwrap();
        assert!(read_log(&path).unwrap().is_empty());
    }
}
