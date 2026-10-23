# 2026-10-24 — #226 验收退回修复:hardening 测试 env 竞态(只读测试未持 env_lock)

**[06:20] 交付行(wt-b2 席)**:#226 测试竞态退回单修复完毕——`reexec_skips_when_no_credentials_captured`
等 3 个测试补 `crate::test_util::env_lock()` 全程持锁;负控实证修前 EXIT=101 同主控签名
(`left: AlreadyCleanEnv / right: NotNeeded`)、修后同条件 11/11 绿;三门全绿
(test --workspace 5249 passed / 0 failed;clippy --workspace EXIT=0;fmt EXIT=0);
fixup commit 于 9d2a251b 之上,未 push 未动 main。

## Date

2026-10-24(wt-b2 席,基线 9d2a251b)

## Goal

#226 验收退回:主控复跑三门,`cargo test --workspace` EXIT=101——
`hardening::tests::reexec_skips_when_no_credentials_captured` 并行下
`assertion left: AlreadyCleanEnv / right: NotNeeded`(单跑绿)。定性:兄弟测试
`reexec_skips_when_sentinel_present` 持锁 set 哨兵 `RECURSIVE_REEXEC_DONE`,
但本测试**裸读**同一变量(经 `reexec_clean_env` 的哨兵短路臂),读落在 set→restore
窗口内即翻上 `AlreadyCleanEnv` 分支——#185/#222 口径「凡改**或依赖** env 的测试
必须全程持 crate 级 EnvGuard(只读也要 lock)」只落了写侧、漏了读侧。

## 窗口在哪(哪个测试 set 了什么)

- **setter**:`reexec_skips_when_sentinel_present`——`env_lock()` 持锁全程,
  `set_var("RECURSIVE_REEXEC_DONE","1")` → `CredentialHandoff::capture()`
  (全 environ 扫描,窗口主体)→ `reexec_clean_env` → restore。锁只对**同样
  持锁的兄弟**生效。
- **reader**:`reexec_skips_when_no_credentials_captured`——不持锁,
  `reexec_clean_env(&default())` 第一件事就是 `var_os(RECURSIVE_REEXEC_DONE)`
  (src/hardening.rs:584 哨兵短路臂,先于 `is_empty` 判定)。空载时 reader 微秒级
  完成、通常赶在 set 之前;满载(`--workspace` 3591+ 测试抢线程)reader 被调度
  延迟,读落窗内 → AlreadyCleanEnv。setter 无过错,补的是 reader 的锁。

## Files touched

- `src/hardening.rs`(仅 tests,产品零改动;+13 行)——三个 #226 新增测试补
  `let _lock = crate::test_util::env_lock();`:
  1. `reexec_skips_when_no_credentials_captured`(unix-not-linux)——**本单 bug 主因**,
     裸读哨兵;doc comment 写明「只读也要 lock」及窗口机理。
  2. `reexec_is_covered_by_prctl_on_linux`(linux)——同形态入口;该平台哨兵臂被
     cfg 掉、现无 env 读,按口径补防御锁(未来任一编译臂加 env 读不再重演)。
  3. `reexec_is_noop_without_execve_semantics`(windows)——同上。

## #226 新增测试 env 接触面审计(全 10 个,不只修 panic 这条)

| 测试 | cfg | env 接触 | 修前 | 修后 |
|---|---|---|---|---|
| reexec_skips_when_sentinel_present | unix-nl | set/remove 哨兵 | 持锁 | 不变 |
| reexec_skips_when_no_credentials_captured | unix-nl | **读哨兵** | **裸奔** | **补锁** |
| reexec_is_covered_by_prctl_on_linux | linux | 入口同形态 | 裸(该臂无读) | 补锁 |
| restore_is_absent_on_linux_even_with_a_preset_fd | linux | set/remove fd 变量 | 持锁 | 不变 |
| reexec_is_noop_without_execve_semantics | windows | 入口同形态 | 裸(该臂无读) | 补锁 |
| handoff_capture_takes_predicate_matches_only | unix-nl | set 4 探针+全 environ 扫 | 持锁 | 不变 |
| handoff_blob_round_trips_and_rejects_truncation | unix | 纯字节,无 env | 无需 | 不变 |
| reexec_child_envs_excludes_credentials_and_reserved_names | unix-nl | set 4 变量 | 持锁 | 不变 |
| restore_reexec_env_drains_pipe_and_sets_variables | unix-nl | set/remove CRED+fd(pipe e2e) | 持锁 | 不变 |
| 报告/predicate/log-capture 等 5 个 | all | 无进程 env 依赖 | 无需 | 不变 |

顺带自查(退回单第 3 条):`restore_reexec_env_drains_pipe_and_sets_variables`
确实用 `set_var`,但 9d2a251b 已持锁(首行 `env_lock()`,含失败分支与二次 restore),
**不裸奔**;#150 scrub、#223 log-capture 等既有测试亦全部持锁。

## 负控实证(修前红 → 修后绿,同条件)

天然复现:两测试同跑 `--test-threads 2` 循环 25 轮未中(空载窗口 = capture() 全
environ 扫描的微秒级,与主控满载 `--workspace` 才命中一致)→ 转窗口放大注入
(两处 `NEGCTL-TEMP` sleep,**未进 commit**):

- setter:set 后 sleep 20ms(放大 set→restore 窗口,模拟 capture() 扫描被拉长);
- reader:读前 sleep 2ms(模拟满载下被调度延迟,读确定落窗)。

- **修前**:`cargo test --lib reexec_skips_when -- --test-threads 2` →
  **EXIT=101**,`left: AlreadyCleanEnv / right: NotNeeded`,与主控 CI 日志同签名
  (行号 913 = 主控 911 + 注入 2 行偏移)。
- **修后**(注入原样保留):同条件 **11/11 绿**——reader 阻塞在 env_lock,
  锁释放时哨兵已 restore,读必得 NotNeeded。证明是锁关窗,非时序运气。
- 撤注入后自然条件复跑绿;三门照旧。

## Notes

- 修复取 `crate::test_util::env_lock()`(src/test_util.rs,毒化容忍、全平台),
  与仓内 9 个既有持锁测试同一口径;crates/recursive-cli 侧的 `test_env::EnvGuard`
  是 bin 测试进程自用的(pub(crate)),跨包不适用,root crate 先例如
  src/http/agui.rs `pinned_home`(env_lock + PinnedRecursiveHome)。
- **产品逻辑零改动**(diff 可证:13 行全部在 `#[cfg(test)] mod tests`)。
- 三门:`cargo test --workspace` EXIT=0(5249 passed / 0 failed);clippy 取
  `--workspace --all-targets --all-features -- -D warnings`(memory trap:字面命令
  只 lint 根包,取超集口径)EXIT=0;`cargo fmt --all --check` EXIT=0。
- 未 push,未动 main;commit 追加在 9d2a251b 之上(wt-b2 席内)。
