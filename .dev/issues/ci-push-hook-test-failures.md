# CI Issue — Remote push hook rejects main due to test failures

**状态**: 待解决  
**发现时间**: 2026-06-01  
**影响**: `git push origin main` 被远程 pre-receive hook 拒绝

---

## 问题描述

向 `origin/main` 推送时，远程服务器的 pre-receive hook 运行
`cargo test --workspace --all-targets`，其中 21 个测试失败，导致推送被拒绝。

**本地运行完全相同命令：596 个测试全部通过（0 个失败）。**

## 失败的测试

```
test checkpoint::tests::read_file_at_returns_none_for_missing ... FAILED
test checkpoint::tests::shadow_repo_init_creates_dir ... FAILED
test checkpoint::tests::changed_paths_lists_files_between_checkpoints ... FAILED
test checkpoint::tests::snapshot_dedups_objects ... FAILED
test checkpoint::tests::list_for_session_returns_empty_before_any_snapshot ... FAILED
test checkpoint::tests::snapshot_per_session_independent ... FAILED
test checkpoint::tests::restore_paths_only_touches_specified_files ... FAILED
test checkpoint::tests::concurrent_snapshots_use_distinct_temp_indexes ... FAILED
test checkpoint::tests::restore_paths_handles_deletion ... FAILED
test checkpoint::tests::worktree_workspace_supported ... FAILED
test rewind::tests::apply_rewind_blocks_on_conflict_without_force ... FAILED
test rewind::tests::apply_rewind_restores_and_truncates_log ... FAILED
test rewind::tests::detect_conflicts_flags_externally_modified_file ... FAILED
test rewind::tests::apply_rewind_force_overrides_conflict ... FAILED
test rewind::tests::rewind_does_not_touch_sibling_session_files ... FAILED
test runtime::tests::runtime_snapshots_at_turn_boundaries ... FAILED
test runtime::tests::runtime_records_touched_files_for_write_file ... FAILED
test runtime::tests::runtime_falls_back_to_diff_for_run_shell ... FAILED
test tools::checkpoint::tests::diff_tool_returns_empty_for_no_change ... FAILED
test tools::checkpoint::tests::list_tool_only_shows_own_session ... FAILED
tools::checkpoint::tests::list_tool_shows_session_checkpoints ... FAILED
```

结果摘要：`575 passed; 21 failed; 0 ignored; 0 measured; 0 filtered out; finished in ~3s`

## 失败测试的共同特征

**所有 21 个失败测试都依赖 git 操作（shadow git repository）。**

涉及的源文件（均未被最近的 g152 提交修改）：
- `src/checkpoint.rs` — 使用 `ShadowRepo::open_at()`，内部初始化 bare git repo
- `src/rewind.rs` — 使用 checkpoint 系统执行 git 操作
- `src/runtime.rs` 中的 3 个测试 — 使用 `has_git()` 检查 + `ShadowRepo`

`has_git()` 函数（`src/runtime.rs:988`）：
```rust
fn has_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}
```

如果 `has_git()` 返回 `false`，这些测试会提前返回（跳过）。在 CI 服务器上，
`has_git()` 显然返回 `true`（否则测试应被跳过而非失败），但随后的 git
操作失败了。

## 可能的根因

以下任一情况都可能导致 git 操作在 CI 服务器上失败：

1. **git 用户名/邮箱未配置**  
   `ShadowRepo` 内部执行 `git commit`，如果 CI 环境没有 `user.name` /
   `user.email` 全局配置，git 会拒绝提交并输出错误。  
   检查：在 CI 环境运行 `git config --global user.name`

2. **`tempfile::tempdir()` 创建的目录不可执行**  
   部分容器/沙箱环境的 `/tmp` 挂载为 `noexec`，导致 git 内部操作失败。  
   检查：在 CI 环境运行 `mount | grep tmp`

3. **`git` 版本过低或行为差异**  
   某些旧版 git（< 2.28）不支持 `git init --initial-branch`，
   而 `ShadowRepo` 可能依赖该参数。  
   检查：在 CI 环境运行 `git --version`

4. **`RECURSIVE_HOME` 或 `HOME` 环境变量问题**  
   `env_lock()` 是进程内锁，无法在并发测试二进制之间共享。
   当多个测试 binary 并行运行时，`RECURSIVE_HOME` 可能被一个 binary
   修改后泄漏到另一个 binary（因为环境变量是进程级别的）。  
   注意：`cargo test --all-targets` 会编译多个测试 binary 并**并行执行**。

## 排查步骤（供解决此问题的 agent 参考）

1. **登录 CI 服务器或在同等环境运行**：
   ```bash
   git --version
   git config --global user.name
   git config --global user.email
   mount | grep tmp
   echo $HOME
   echo $RECURSIVE_HOME
   ```

2. **单独运行失败测试**，观察实际错误信息：
   ```bash
   cargo test --lib checkpoint::tests 2>&1 | head -50
   ```
   （正常推送时 hook 输出只显示 FAILED，不显示 panic message）

3. **如果是 git 配置问题**，在 CI 环境设置：
   ```bash
   git config --global user.name "CI Bot"
   git config --global user.email "ci@recursive.local"
   ```
   或在 hook 脚本中添加这两行。

4. **如果是并发 HOME 污染问题**，在 hook 中改用串行运行：
   ```bash
   cargo test --workspace --all-targets -- --test-threads=1
   ```
   （会更慢，但能排除并发环境变量问题）

5. **如果是 `noexec /tmp` 问题**，在 CI 环境设置 `TMPDIR` 到可执行目录：
   ```bash
   export TMPDIR=/var/tmp
   cargo test --workspace --all-targets
   ```

## 受影响的推送

当前本地 `main` 分支（`46cdd0a`，包含 g152 incremental writes）等待推送，
已成功合并但无法推送到 `origin/main`。

## 验证方法

修复后，在 CI 环境运行：
```bash
cargo test --workspace --all-targets 2>&1 | grep "test result:"
```

预期输出应全部为：`test result: ok. N passed; 0 failed`
