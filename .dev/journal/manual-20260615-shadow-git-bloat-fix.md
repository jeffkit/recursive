# Manual edit: shadow-git 膨胀修复

**Date**: 2026-06-15
**Goal**: 修复 shadow-git 膨胀问题（历史实测 ~20GB），防止新快照继续膨胀，并提供运维清理工具。
**Files touched**:
- `src/checkpoint.rs` — 修复 pathspec 缺陷 + 实现 ShadowRepo::gc()
- `src/cli/session.rs` — apply_rewind 后调用 gc + 新增两个 CLI 命令
- `src/main.rs` — 新增 SessionCmd::GcCheckpoints / CleanCheckpoints 子命令

**Root causes fixed**:
1. `write_workspace_tree()` (用于 checkpoint_diff) 只排除 `.recursive/`，不排除 `target/` / `node_modules/` 等大目录，每次 diff 都向 objects store 注水。→ 补全与 `snapshot_for_session` 相同的排除 pathspec。
2. `snapshot_for_session` 的 node_modules 排除只用 `:!node_modules/**`，无法排除 `website/node_modules/` 等嵌套路径。→ 改为 `:!**/node_modules/**`。
3. `ShadowRepo::gc()` 从未实现，orphan objects 永久堆积。→ 实现 gc()：git reflog expire --all --expire=now + git gc --prune=now。
4. `apply_rewind` 后不清理 orphan objects。→ 在 cmd_session_rewind 调用 gc（best-effort，失败仅 warning）。

**New CLI commands**:
- `recursive sessions gc-checkpoints` — 对当前 workspace 的 shadow-git 运行 git gc
- `recursive sessions clean-checkpoints [--force]` — 完全删除 shadow-git（带确认提示）

**Disk cleanup done**:
- 删除了所有孤儿 workspace（对应路径已不存在的 worktree shadow-git）
- 残留活跃：主仓 5.1GB（含历史脏快照，session refs 仍引用）/ count-lines-parity 3GB / /tmp 800MB
- 对主仓运行了 git gc --prune=now（压缩 pack，不能删除 session-refs 指向的对象）
- 如需彻底回收主仓历史脏快照空间，可运行：`recursive sessions clean-checkpoints` 后重启 session

**Tests added**: none (gc 是 best-effort 路径，现有测试覆盖 snapshot/restore 路径)
**Tests passing**: cargo test --lib checkpoint (22/22) + cargo test --test checkpoint_e2e (2/2)

**Notes**:
- 真正回收主仓历史脏快照（target/ blobs）需要删除 session refs 再 gc，或直接 `clean-checkpoints`。gc 只能 repack + prune unreachable objects，session refs 指向的历史 commit tree 仍会保留所有 blob。
- 下次 `self-improve` loop 在新 session 创建快照时，pathspec 修复已生效，不会再捕获 target/ 或嵌套 node_modules/。
