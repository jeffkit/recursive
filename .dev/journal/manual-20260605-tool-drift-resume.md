# Journal — manual-20260605-tool-drift-resume

Date: 2026-06-05
Goal: #109 fix(cli): 跨版本 resume 被 tool_registry_hash 一票否决且无 override

## What changed

`recursive resume` 的 tool-registry hash 不匹配从「无逃生口的一票否决」改为
「默认仍硬拒，`--allow-tool-drift` 显式放行后降级为警告 + 漂移报告」。

## Files touched

- `crates/recursive-cli/src/main.rs` — `Cmd::Resume` 新增 `--allow-tool-drift`
  flag（默认 false），穿过 `-r` / `-c` 隐式 resume 路径（false）与
  `cmd_resume` 调用点。
- `crates/recursive-cli/src/cli/resume.rs` — hash mismatch 分支：
  - 无 flag：错误文本追加 `Re-run with --allow-tool-drift to resume anyway.`
    （原来是无出口的 bail）。
  - 有 flag：`registry_drifted = true`，stderr 打 warning（含新旧 hash），
    并用新增的 `SessionReader::load_referenced_tool_names` 报告 transcript
    引用但当前 registry 已不存在的工具列表。
  - orphan Redo 分支新增守卫：`registry_drifted` 且 orphan 的工具在当前
    registry 中缺失 → 硬拒（`UnknownTool` 死循环防呆），提示改用
    `--orphans=skip|ask`。skip / ask 路径不受影响——旧调用在 seed 里只是
    历史文本，重放不需要工具仍存在。
- `src/session/reader.rs` — 新增
  `SessionReader::load_referenced_tool_names(dir)`：收集 post-compaction
  tail 内所有 assistant `tool_calls` 引用的工具名（BTreeSet 去重排序）；
  `scan_orphan_tool_calls` 文档更新（registry 缺失的工具回退 External，
  与既有行为一致，只是写明契约）。附 2 个单元测试。
- `src/session/orphan.rs` — `OrphanToolCall::side_effect_at_call` 文档更新
  （去掉「resume 一定先验证过 hash」的过时前提）。
- `crates/recursive-cli/tests/cli_resume_surfaces.rs` — Rig 新增
  `session_dir_with_hash`；4 个真二进制集成测试：
  1. 无 flag 漂移 → 拒绝 + 提示 flag；
  2. 有 flag 漂移 → 成功 + warning；
  3. 有 flag + vanished tool 引用 → 成功 + 漂移报告点名；
  4. 有 flag + `--orphans redo` + vanished tool → 仍拒绝。

## Tests added

- `src/session/reader.rs`:
  - `load_referenced_tool_names_collects_every_assistant_call`（去重排序）
  - `load_referenced_tool_names_empty_without_tool_calls`
- `crates/recursive-cli/src/cli/resume.rs`:
  - `cmd_resume_allow_tool_drift_degrades_mismatch_to_warning`
  - 既有 `cmd_resume_refuses_tool_registry_hash_mismatch` 扩展断言
    错误文本命名 `--allow-tool-drift`。
- `crates/recursive-cli/tests/cli_resume_surfaces.rs`: 上述 4 个集成测试。

## Gates

- `cargo test --workspace` — 58/58 suites ok, 0 failed
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all` — applied

## Notes

- 刻意不改 `SessionFile::validate_tool_registry`（legacy `.json` 路径）与
  `src/session/writer.rs` 的 hash 写入逻辑——hash 本身仍是有效的漂移信号，
  只是把 CLI 消费方式从硬闸改为可放行警告。
- `-r` / `-c` 隐式 resume 不传 drift flag：无人值守调用方保持升级安全；
  需要时显式敲 `recursive resume --allow-tool-drift`。
- schema_version 单向闸（拒新拒旧策略不变）未动。
