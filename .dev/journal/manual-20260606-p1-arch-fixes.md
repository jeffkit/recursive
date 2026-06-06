# Manual edit: p1-arch-fixes

**Date**: 2026-06-06
**Goal**: Fix 3 P1-severity architecture issues from the review
**Files touched**:
- `src/tools/mod.rs` — SEC-008: policy sandbox接入 invoke_with_audit
- `src/tools/sub_agent.rs` — M-4: build_sub_registry 用 with_same_transport()
- `src/tools/spawn_worker.rs` — M-4: 同上
- `src/tools/spawn_workers_parallel.rs` — M-4: 同上
- `src/session.rs` — C2-storage: .meta.json 原子写

**Tests added**: none (behavior changes validated by existing 79 HTTP + 15 integration tests)

**Notes**:
- SEC-008: policy check 在 tool.execute() 之前但在权限检查之后，因此 PermissionDenied 优先级高于 policy deny，语义正确。仅检查 "command" / "path" / "file_path" 字段，覆盖 RunShell/Read/Write/Edit/StrReplace。
- M-4: fork() 实现为 clone()，register() 是 BTreeMap::insert() 覆盖——所以 fork() + re-register 不减少工具，只是冗余注册。with_same_transport() 从空 BTreeMap 开始，语义正确。
- C2-storage: atomic_write() 用 PID 区分临时文件名，防止同一目录并发写时临时文件冲突。
