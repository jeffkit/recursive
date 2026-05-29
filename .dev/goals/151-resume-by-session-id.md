# Goal 151 — Resume by session ID, on the JSONL session format

> **Roadmap**: Phase 18 — Long-running Goals (preliminary, blocks 18.5)
> **Design principle check**:
> - Pure CLI/UX change + a thin reader helper. No agent loop changes,
>   no kernel changes, no new persistence layer. The JSONL session
>   format already exists (g141 + 142) and is already written on
>   every run; this goal just makes it the **resumable** thing.
> - Orthogonal: `Cmd::Resume` becomes a search + load + invoke; the
>   runtime path it feeds (`run_resumed`) is unchanged.
> - Replaces the legacy `SessionFile` (.json) resume path. Backwards
>   compatibility for *reading* old `.json` files via `sessions show`
>   is kept; *resuming* them is dropped (one-shot migration script
>   provided).

## Why

Today there are two session formats living in the same codebase:

1. **JSONL session directory** (g141 + 142, current default):
   - `~/.recursive/workspaces/<hash>/sessions/<slug>/<id>/`
   - `transcript.jsonl` + `.meta.json` + `checkpoints.jsonl`
   - Written automatically by every `recursive run` / `recursive loop`
   - Discoverable by ID substring via `resolve_session_path`
     (`src/main.rs:794`) and used by `sessions show / delete /
     export / rewind`

2. **`SessionFile` single .json** (legacy):
   - One blob with `goal`, `model`, `tool_registry_hash`,
     `transcript: Vec<Message>`, `compaction_history`
   - Only written when the user passes `--session-out path.json`
   - **The only format `recursive resume` accepts**

The mismatch creates two real problems:

1. **No "just resume the last session"**. Today users must remember
   to pass `--session-out` ahead of time, then know the file path,
   then call `recursive resume <path>`. If they didn't, the JSONL
   record is there in `~/.recursive/...` but `resume` can't load it.
   For 18.5 (long-running goals, crash recovery) this is the wrong
   default — the transcript that was written *automatically* should
   be the thing you resume from, not a parallel format you had to
   opt into.
2. **Two write paths, one read path**. The agent writes JSONL
   automatically and `.json` only on opt-in; `resume` reads only the
   opt-in one. This is the inverse of what you want.

## Scope

### Surface change

```
# Before
recursive resume <path-to-session.json>

# After
recursive resume <session-id-or-substring>
recursive resume                          # resumes most-recent session in workspace
```

ID matching reuses `resolve_session_path` (already battle-tested by
`sessions show` and friends). Substring match against the
`<timestamp>-<workspace-slug>` name. Ambiguous match → list candidates
and abort. No match → suggest `recursive sessions list`.

### What gets read

`resolve_session_path` returns a directory; the new helper
`SessionReader::load_messages` returns the LLM-shaped seed:

```rust
impl SessionReader {
    /// Load the transcript and convert each `TranscriptEntry` to a
    /// runtime `Message` (drops persistence-only fields: id, parent_id,
    /// timestamp, and — once g153 lands — audit). The result is what
    /// `run_resumed` expects as `seed`.
    pub fn load_messages(session_dir: &Path) -> std::io::Result<Vec<Message>>;
}
```

Goal/model/provider come from `.meta.json` via the existing
`load_meta`. No new on-disk schema.

### Tool registry validation

The legacy `SessionFile` carries `tool_registry_hash` and refuses
to resume if the current registry's hash differs. Port this to the
JSONL path:

- Add `tool_registry_hash: Option<String>` to `SessionMeta` (None for
  pre-151 sessions; written from g151 onwards).
- Hash is recorded **at session creation time**, not lazily, so the
  rest of the run can rely on it. Concretely:
  - `SessionWriter::create` gains a new param `tool_specs: &[ToolSpec]`.
    The runtime layer in `main.rs` already builds the tool registry
    before `SessionWriter::create` is called (`build_tools` happens
    earlier in `run` / `run_resumed`), so this is a one-line
    plumbing change at each call site (~3 sites).
  - For the "resume into an existing session" case, we do **not**
    rewrite the hash — it's a creation-only field. Hash mismatch
    on resume → abort. (This matches the legacy `SessionFile`
    behaviour, which also stamps the hash at write time.)
- On resume: if the field is present and disagrees with the current
  registry, abort with the same error message the legacy path uses.
  If absent (older session), log a warning and continue — we don't
  want to brick anyone's existing JSONL records.

This is a one-line schema bump on `SessionMeta`; the field is
`#[serde(default, skip_serializing_if = "Option::is_none")]` so old
meta files still parse.

### Locking (prevent two processes resuming the same session)

A second `recursive resume <id>` while another is still running on
the same session would interleave writes to `transcript.jsonl` and
corrupt it (especially after g152, where writes are incremental).
Cheap fix:

- On open for resume **and** on `SessionWriter::create`, write a
  sentinel file `<session_dir>/.lock` containing
  `{pid}\n{hostname}\n{started_at_unix}\n` (plain text, no escaping
  needed — pid is `u32`, hostname goes through `.replace('\n', "_")`
  defensively).
- On open, if the file exists, read it and check whether the
  recorded pid is still alive. Strategy:
  - Unix: invoke the platform `kill(2)` with signal 0 to probe.
    Implementation options, in order of preference:
    1. **stdlib-only**: spawn `/bin/kill -0 <pid>` via
       `std::process::Command`. ~1ms cost, runs only at acquire
       time (not in any hot path), no new dep. Recommended.
    2. **`libc` direct call** (`libc::kill(pid, 0)`): faster but
       requires adding `libc` to `[dependencies]` (it's already
       transitively in the dep tree via tokio, but Cargo's
       namespacing means we'd still have to declare it). Use only
       if option 1 turns out to be measurably bad.
  - Windows: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, ...)`
    via `windows-sys` (already transitively in tree via tokio's
    Windows backend; same Cargo-namespacing caveat — declare it if
    needed, behind `#[cfg(windows)]`).
  - Acquire-time outcomes:
    - Alive → abort with: `session <id> is being written by another
      process (pid {pid}, host {hostname}). If you believe this is
      stale, remove <session_dir>/.lock and retry.`
    - Dead or unreadable → overwrite the sentinel with our own info
      and proceed. Log a warning that we recovered a stale lock.
- The lock is removed by `SessionWriter::finish` (and on Drop, with
  best-effort cleanup so a panic doesn't strand it).
- Cross-host locks: this design treats different hosts as
  independent — if the recorded `hostname` differs from ours, we
  do **not** trust the pid check and abort conservatively. Power
  users on shared filesystems can still remove `.lock` manually.

We deliberately do **not** use `flock(2)` for this:

- It would need a new dependency (`fs2` or `nix::fcntl`).
- Its semantics over NFS / network filesystems are notoriously
  flaky, and Recursive's session dir is exactly the kind of thing
  users put in Dropbox / iCloud / `~/sync/`.
- The PID-sentinel approach gives a better error message ("pid
  12345 on host X is still running") and is easy to override
  manually when something does go wrong.

`fs2` / `flock` may be revisited if we ever add a daemon-style
session manager that needs OS-level mutual exclusion; it's not
needed for the current single-process-per-resume model.

### Legacy `.json` files

The `SessionFile` *type* and its readers stay (used by
`sessions show` for backwards-compat viewing). But:

- `Cmd::Resume.session: PathBuf` becomes `Option<String>` (see
  "CLI surface" below). The handler dispatches:
  1. **No arg** → most-recent shortcut.
  2. **Arg ending in `.json`, OR arg that resolves as an existing
     file path** (not a directory) → legacy path detected. Print
     the migrate-legacy hint and exit non-zero. Do **not** attempt
     to load it; the legacy resume code path is removed.
  3. **Otherwise** → treat as session ID / substring, hand to
     `resolve_session_path`. If `resolve_session_path` returns a
     file (rare: a stray `.json` in `~/.recursive/sessions/<slug>/`),
     fall back to case 2's error.
- Helpful error text:
  ```
  legacy .json sessions are no longer resumable directly.
  Run 'recursive sessions migrate-legacy <path>' to convert it
  to the JSONL format, then 'recursive resume <id>'.
  ```
- For users with scripts that pipe a path: keep an **escape
  hatch** flag `--from-file <path>` that still accepts a JSONL
  session directory (not legacy `.json`) for one release. This
  preserves "I have the path, don't make me look up the ID"
  workflows without re-introducing a parallel resume model.
- New flag `--session-out` is **deprecated** with a runtime warning;
  removed in a follow-up cleanup goal (not this one).
- New subcommand `recursive sessions migrate-legacy <path>` reads
  one old .json and writes a new session directory under the user
  data dir. One-shot tool, ~50 lines.

### `--session-out` semantics during resume

`run_resumed` currently accepts a `cli.session_out: Option<PathBuf>`
and writes a legacy `SessionFile` if the run finishes non-success
(`src/main.rs:1690-1700`, `1986-1996`). Under g151 this is
**unchanged but redundant** — the JSONL session directory is
*always* written now. The deprecation warning we emit when
`--session-out` is present should explicitly call this out:

```
warning: --session-out writes the legacy .json format, which is
no longer used for resume. Your session is automatically being
written to <session_dir>; use 'recursive resume <id>' to resume.
The flag will be removed in a future release.
```

Importantly, `run_resumed` does **not** open a fresh session
directory when called via `Cmd::Resume`. It must continue writing
to the **existing** session dir (the one we just resolved). This
requires a small new constructor:

```rust
impl SessionWriter {
    /// Re-open an existing session directory for appending.
    /// Reads the existing `.meta.json` to recover `message_count`,
    /// `created_at`, `goal`, `model`, `provider`. Hash is **not**
    /// re-validated here; the caller (resume handler) must have
    /// done that already.
    pub fn open_existing(session_dir: &Path) -> std::io::Result<Self>;
}
```

This is what continues the `msg_NNN` ID sequence past the resume
boundary instead of restarting from `msg_001`.

### CLI surface change

```rust
// Before
Cmd::Resume { session: PathBuf }

// After
Cmd::Resume {
    /// Session ID or substring. If omitted, resumes the most-recent
    /// active or interrupted session in the current workspace.
    session: Option<String>,
    /// Escape hatch: resume from an explicit JSONL session directory
    /// path (not a legacy .json file). Mutually exclusive with
    /// positional `session`.
    #[arg(long, conflicts_with = "session")]
    from_file: Option<PathBuf>,
}
```

## Architecture

### Files touched

- `src/session.rs`
  - Add `SessionReader::load_messages(dir) -> Vec<Message>`.
  - Add `tool_registry_hash: Option<String>` to `SessionMeta`
    (with `#[serde(default, skip_serializing_if = "Option::is_none")]`).
  - Change `SessionWriter::create` signature to accept
    `tool_specs: &[ToolSpec]`; compute and stamp the hash into
    `.meta.json`.
  - Add `SessionWriter::open_existing(session_dir) -> Self` that
    re-loads meta, restores `message_count`, and continues appending
    (shared helper between `create` and `open_existing` for the
    locking + meta-write logic).
  - Add `SessionLock` (RAII guard) with `acquire(session_dir)` /
    `Drop` cleanup. Owns the `.lock` sentinel file. PID liveness
    check via spawned `/bin/kill -0 <pid>` on Unix (no new deps);
    `OpenProcess` on Windows behind `#[cfg(windows)]`.
  - Bump `SessionWriter` to take ownership of a `SessionLock` so
    the lock is held for as long as we're writing.
  - On `updated_at`: bump `SessionMeta.updated_at` on **every**
    `append()`, not just on `finish()`. Cheap (one `serde_json` + one
    `fs::write` per turn — already happens for the transcript.jsonl
    line, so no new write per *message*; we batch the meta write to
    once per `append` of an `assistant` or `user` role to avoid
    hot-pathing on tool-call-heavy turns). The "most-recent" shortcut
    relies on this being live.
- `src/main.rs`
  - `Cmd::Resume.session: PathBuf` → `Option<String>`; add
    `--from-file <path>` (mutually exclusive).
  - Resume handler dispatch: detect legacy `.json` → migrate-legacy
    error. Otherwise `resolve_session_path` → `acquire SessionLock`
    → `load_meta` → `validate_tool_registry_hash` → `load_messages`
    → `run_resumed`.
  - `run_resumed`: take an existing `SessionWriter` (from
    `open_existing`) instead of creating a new one when invoked
    via the resume path. Threads through `cli.session_out`'s
    deprecation warning.
  - `--session-out` deprecation warning printed once at startup
    when the flag is set (regardless of subcommand).
  - New `Cmd::Sessions::MigrateLegacy { path: PathBuf }`.

### Resume control flow (new)

```
recursive resume [<id> | --from-file <path>]
  │
  ├─ if no arg:
  │     SessionReader::list_sessions(workspace)
  │       sorted by .meta.json updated_at desc
  │       pick first whose status ∈ {active, interrupted}
  │
  ├─ if arg looks like a file path or ends with `.json`:
  │     print migrate-legacy hint → exit 2
  │
  ├─ resolve_session_path(workspace, id) → session_dir
  │
  ├─ SessionLock::acquire(session_dir)
  │     ├─ stale (pid dead / cross-host) → recover, warn
  │     └─ live → abort with pid + hostname hint
  │
  ├─ SessionReader::load_meta(session_dir)
  │
  ├─ build_tools(&config) → ToolRegistry
  │     └─ if meta.tool_registry_hash mismatch → abort
  │
  ├─ SessionReader::load_messages(session_dir)
  │     └─ Vec<Message>: see entry_to_message
  │
  ├─ SessionWriter::open_existing(session_dir)
  │     └─ continues `msg_NNN` numbering; reuses the held lock
  │
  └─ run_resumed(config, writer, seed=messages, goal, ...)
```

### `entry_to_message`

```rust
fn entry_to_message(entry: TranscriptEntry) -> Message {
    Message {
        role: match entry.role.as_str() {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => Role::User,  // defensive; should never hit
        },
        content: entry.content,
        tool_calls: entry.tool_calls,
        tool_call_id: entry.tool_call_id,
        reasoning_content: entry.reasoning_content,
    }
}
```

The conversion is the **isolation point** between persisted shape
(`TranscriptEntry`: id, parent_id, timestamp, eventual `audit`) and
LLM wire shape (`Message`: only what providers consume).
Persistence-only fields are dropped here.

### Most-recent shortcut

`recursive resume` (no arg) → `SessionReader::list_sessions(workspace)`
sorted by `.meta.json` `updated_at` desc, take the first one whose
status is `active` or `interrupted`. If none → tell the user to
specify an ID.

The "bump `updated_at` on every append" change above is what makes
this correct in the presence of crashed sessions:

- Old behaviour: `updated_at == created_at` for any session that
  never reached `finish()`. A workspace with three sessions A
  (clean), B (crashed mid-run), C (clean) might pick A or C as
  "most recent" depending on raw timestamps, missing B entirely.
- New behaviour: `updated_at` advances with every appended message,
  so B (the crashed one — the one the user almost certainly wants
  to resume) sorts to the top.

If two sessions tie on `updated_at` (sub-second creation in tests),
break ties by `transcript.jsonl` mtime as a fallback, then by
`session_id` lexicographically. Documented in `list_sessions`.

## Tests

Unit (`src/session.rs`):

- `load_messages_drops_persistence_fields` — write a session with
  TranscriptEntries that have id/timestamp/parent_id; assert the
  reloaded `Vec<Message>` has none of those fields visible.
- `meta_round_trip_with_tool_registry_hash` — write meta with hash,
  reload, hash matches. Field is `None` for an old `.meta.json`
  parsed without it.
- `meta_round_trip_old_format_no_hash` — fixture from before this
  goal (no `tool_registry_hash` field) parses; resulting meta has
  `tool_registry_hash: None`.
- `append_bumps_updated_at` — create writer, capture `updated_at`,
  append a user message, reload meta, assert `updated_at` changed.
- `open_existing_continues_msg_numbering` — create session, append
  3 messages, drop writer (without `finish`), `open_existing`,
  append one more, reload transcript, assert message ids are
  `msg_001..msg_004`.
- `lock_alive_pid_blocks_acquire` — write a `.lock` file with the
  current process's pid, second `acquire` returns busy error
  mentioning the pid.
- `lock_dead_pid_recovered` — write a `.lock` file with a
  guaranteed-dead pid (e.g. `u32::MAX`), `acquire` succeeds and
  overwrites the sentinel; warning is logged (captured via
  `tracing-subscriber` test layer or stderr capture).
- `lock_cross_host_aborts` — write `.lock` with a synthetic
  hostname; `acquire` from current host aborts even if the recorded
  pid is dead, since cross-host pid checks are unsafe.
- `lock_released_on_drop` — `acquire` then drop the guard, sentinel
  file is removed; second `acquire` succeeds.

Integration (`tests/resume_by_id.rs`, new):

- `resume_by_full_id` — create session, run one turn, exit, resume
  by exact ID, assert seed length matches.
- `resume_by_id_substring` — same, resume by trailing chars only.
- `resume_ambiguous_id_fails` — two sessions matching same prefix
  → error lists both.
- `resume_no_arg_picks_most_recent` — three sessions, the middle
  one has the latest `updated_at`; resume w/o arg picks it.
- `resume_no_arg_prefers_interrupted_over_clean` — two sessions
  with identical `updated_at`, one `status: active`, one
  `status: success`; the active one wins.
- `resume_rejects_legacy_json` — pass a `.json` path → exit 2,
  stderr mentions `migrate-legacy`. Cover both:
    - `recursive resume ./old.json` (file argument)
    - `recursive resume <id>` where `resolve_session_path` returns
      a `.json` file (legacy in-tree session).
- `resume_from_file_accepts_jsonl_dir` — `--from-file <dir>` with
  a JSONL session directory works equivalently to `<id>`.
- `resume_from_file_rejects_legacy_json` — `--from-file ./old.json`
  → same migrate-legacy error as the positional case.
- `resume_aborts_on_tool_registry_mismatch` — write session with one
  tool set, try to resume with a different set → abort.
- `resume_warns_on_old_session_without_hash` — write a session
  whose `.meta.json` has no `tool_registry_hash`; resume succeeds
  but stderr contains a warning about absent hash.
- `resume_continues_msg_numbering` — resume an existing session
  with 5 messages; after a one-turn resumed run, the JSONL has
  `msg_006`+ entries, no duplicates, `updated_at` changed.
- `resume_session_out_warning` — resume with `--session-out
  /tmp/x.json`, stderr mentions deprecation; the file *is* still
  written (back-compat) but the new JSONL session is also updated.
- `resume_lock_blocks_concurrent_resume` — process A holds resume
  lock, process B's `recursive resume <id>` exits non-zero with a
  message containing A's pid. (Use a small bash helper that
  backgrounds A as `sleep`-ing on stdin.)
- `migrate_legacy_creates_jsonl_dir` — legacy .json → directory
  with transcript.jsonl + .meta.json; resume by ID works. Hash
  is carried over from the legacy file.

## Acceptance

- `cargo build` green
- `cargo test` green; ≥ 18 new tests (8 unit + 10 integration)
- `cargo clippy --all-targets -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- No new external crate dependencies. (PID liveness via `libc`
  which is already transitive; no `fs2`, no `nix`, no `chrono`,
  no `uuid`. If any of these is found necessary during
  implementation, bring it back to design review before merging.)
- Manual smoke: `recursive run "echo hi" && recursive resume`
  (no flags, no IDs) reopens the same session and accepts a
  follow-up turn. The resumed turn appears in the same
  `transcript.jsonl` continuing the `msg_NNN` sequence.

## Out of scope (deferred)

- Drop `SessionFile` type and `--session-out` flag entirely
  (separate cleanup goal in a later release; needs a deprecation
  cycle).
- Cross-machine resume (the lock is local-fs; networked agents
  would need a different design).
- Resume-into-a-fork (start a new session that *branches* from an
  existing one's transcript at turn N — already partly supported
  via `--resume-from`, but the UX is separate).
- Session encryption at rest.
- Concurrent multi-writer protocols (the lock is exclusive on
  purpose; if you want shared writes, that's a different feature).

## Why this blocks 18.5

g153 (tool execution journal as audit fields) lives **inside**
`TranscriptEntry`. The natural way to use it on resume — detect
orphan tool_calls, decide skip-or-redo — needs a path where
`recursive resume <id>` actually loads the JSONL transcript and
reasons about it. Today that path doesn't exist; this goal builds it.

Note that g153's orphan detection further requires **incremental
transcript writes** (g152): today `SessionWriter::append` is called
in a batch *after* `runtime.run()` returns, so a process killed
mid-turn leaves no orphan trail to find. g151 + g152 together are
the prerequisites for g153's detection layer to be meaningful.
