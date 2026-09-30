# Manual change: 2026-09-30 — issue #64 Skill content-backed body/refs

- **Goal**: #64 (C-end blocker B3) — allow `Skill` to be constructed from
  pure content (remote/DB/tenant delivery) instead of requiring a local
  SKILL.md on disk. Supersedes the rolled-back pipeline attempt
  (run pipeline-64-0930203307, which died on the fmt gate and auto-rolled
  back — only its journal survived; this commit re-lands the same design
  with fmt actually applied).
- **Files touched**:
  - `src/skills.rs`: `Skill.body: Option<String>` (raw full text incl.
    frontmatter), `SkillRef.content: Option<String>`; new
    `skill_from_content()` (pure-content constructor, no disk);
    `read_skill_content()` / `read_ref_content()` content-first readers;
    `skills_for_injection` (Always mode) routed through the reader; 7 tests.
  - `src/tools/load_skill.rs`: the issue's three disk reads (`:96` deps,
    `:204` refs, `:268` main body) now go through the content-first readers;
    `substitute_skill_dir` returns `Result` and **errors clearly** when a
    content-backed skill (no local dir) references an unescaped
    `${SKILL_DIR}` — previously the placeholder was silently passed through,
    inviting the model to run a broken command; the old
    `load_skill_no_skill_dir_when_path_has_no_parent` test asserted the
    pass-through and was updated to assert the error; 5 new body-only tests.
  - `src/skills_injector.rs` (Globs injection) and `src/compact/reinject.rs`
    (post-compaction skill restore): also routed through
    `read_skill_content` — these two are outside the issue's list of three
    but would have kept a body-only Always/Globs skill tied to disk.
  - `src/lib.rs`: re-export `skill_from_content` / `read_skill_content` /
    `read_ref_content`.
  - Mechanical `body: None` / `content: None` at all existing struct
    literals (system_prompt, runtime/tests, http/handlers, tests/integration,
    skills.rs test literals).
- **Tests added**: 12 (constructor frontmatter/sections + fallback name,
  content-first readers ×2, index rendering, Always-mode inline injection,
  body-only main/section/ref/deps load via the `Skill` tool,
  `${SKILL_DIR}` clear error, updated no-parent degenerate test).
- **Impact analysis**: GitNexus MCP unavailable in this session; done by
  grep — every `fs::read_to_string(&skill.path)` / `SkillRef.path` call
  site in the workspace enumerated (6 production reads) and all routed or
  verified. `discover_skills` untouched: dir-discovered skills keep
  `body: None` / refs `content: None`, so existing behavior is
  byte-identical (regression covered by the pre-existing discover/index
  suites plus the new disk-fallback test).
- **Deliberately out of scope** (flagged in the issue reply): a
  *product-level* injection surface (HTTP body / config / env carrying
  inline skill content). The library API now exists; without a
  serve-level surface an external no-fs harness still cannot hand skills
  to the process — that is the `SkillSource`-trait / remote-delivery
  follow-up, tracked as the issue's "recommended" option.
- **Gates**: `cargo test --workspace` all green (57 binaries, 0 failures;
  lib re-run after fmt reflow: 2396+821 pass), `cargo clippy --all-targets
  --all-features -- -D warnings` clean, `cargo fmt --all` applied and
  `--check` clean.
