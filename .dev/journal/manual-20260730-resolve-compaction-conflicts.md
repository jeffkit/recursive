# Resolve compaction conflicts

## 2026-07-30

### fix — 恢复 compaction 冲突并修复 emergency hook 配对

- 解决 `src/run_core.rs`、`src/runtime.rs` 与已删除 `src/compact/runner.rs` 的未合并索引状态。
- 补充 emergency compaction no-op 回归测试，确保不可压缩时不触发 hook 或 provider。
- 验证：fmt、全量 cargo test（2082 passed，2 ignored）、全 features clippy 与 build 均通过。
- 关联：`docs/exec-plans/active/resolve-compaction-conflicts/`
