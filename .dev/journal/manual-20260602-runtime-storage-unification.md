# Manual edit: runtime-storage-unification

**Date**: 2026-06-02
**Goal**: 实现 B / D / C / E 四项路线图任务，worktree feat/runtime-storage-unification
**Files touched**:
- `src/runtime.rs` (Goal 191 + 193)
- `src/http.rs` (Goal 193)
- `docker-compose.yml`, `scripts/localstack-init.sh`, `.env.example`, `README.md` (Goal 192)
- `src/memory/mod.rs`, `src/memory/noop.rs`, `src/memory/openai_embedding.rs`, `src/memory/sqlite_vec.rs` (Goal 194)
- `src/tools/memory.rs` (Goal 194)
- `src/lib.rs` (Goal 194)
- `Cargo.toml` (Goal 194)
- `.dev/goals/191-*.md`, `192-*.md`, `193-*.md`, `194-*.md`

**Tests added**:
- `runtime::tests::storage_backend_saves_transcript_after_run`
- `runtime::tests::restore_from_storage_loads_transcript`
- `runtime::tests::restore_from_storage_returns_false_for_new_session`
- `memory::noop::tests::*` (4 tests)
- `memory::sqlite_vec::tests::*` (5 tests, only compiled with vector-memory feature)

**Notes**:
- Goal 191: `AgentRuntime::run()` 每轮后 save_transcript；新增 set_session_id / restore_from_storage
- Goal 192: docker-compose 三服务（recursive + redis + localstack S3）；README 云端部署章节补全
- Goal 193: tracing debug/info 日志带 session_id；HTTP session 创建注入 session_id
- Goal 194: sqlite-vec 0.1.10-alpha.4 在 macOS arm64 有 C 构建缺陷，改用纯 rusqlite + Rust 余弦相似度，行为等效
- `AgentRuntimeBuilder` 新增 with_storage / with_session_store / with_tool_set_provider 代理方法
