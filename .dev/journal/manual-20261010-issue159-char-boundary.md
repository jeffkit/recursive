# Manual change — issue #159: compact re-injection sliced on byte indices → UTF-8 panic

## Date
2026-10-10

## Goal
`src/compact/reinject.rs` truncated re-injected file/skill content with
`&content[..max_chars]`. `max_chars` was really a **byte** budget
(`per_file_budget * 4` = 20_000), so any >20 KB content containing non-ASCII
bytes hit `byte index 20000 is not a char boundary` → panic inside
`AgentRuntime::run`, after the turn was already persisted. Under the default
`standard` preset (reinject_recent_files on), a single Read of a >20 KB CJK
document followed by a cross-turn compaction killed the task.

## Files touched
- `src/compact/reinject.rs`
  - new `pub(crate) fn truncate_on_char_boundary(&str, usize) -> &str`
    (hand-rolled — `str::floor_char_boundary` is still nightly; MSRV 1.75).
  - `FileReinjector::reinject`: `max_chars` → `max_bytes`, slice via the helper.
  - `SkillReinjector::reinject`: same fix for `&body[..max_chars]`.
  - Truncation markers unchanged; budget semantics (`len()/4 + 1`) unchanged.

## Tests added
- `truncate_stops_at_char_boundary` — 10×"中" cut at 5 bytes → `"中"` (3 bytes).
- `truncate_returns_full_string_when_short` — `max >= len` returns input verbatim.
- `file_reinjector_survives_cjk_over_budget` — 7000 CJK chars (21000 B) through
  `FileReinjector::reinject`; no panic, marker present, 6666 chars survive.
- `skill_reinjector_survives_cjk_over_budget` — same shape via `SkillReinjector`.

## Verification
- `cargo test -p recursive-agent --lib compact::reinject` — 35 passed, 0 failed.
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all` — applied.

## Notes
Out of scope (per the issue): summary/prefix injection problems, `per_file_budget`
/ `token_budget` defaults, injected layout.
