# manual-20261012 issue #185 — recursive-cli 单测共享进程 env、锁不跨模块

- issue:        jeffkit/recursive#185（P2，属 #194 工程门禁簇）
- worktree:     `/Users/jeff.jie/projects/infra4agent.worktrees/recursive-185`
- branch:       `fix/185-test-env-lock`（基于 `origin/main` @ `6c724056`）
- 方案:         规格里的 **A（共享 guard）**；B（显式 env 视图）留给后续单独评审
- 结论:         completed（本地提交，未 push、未发评论）

## 根因

`cargo test -p recursive-cli --bin recursive` 的**全部单测跑在同一个进程**里，
而进程 env 是全局状态。修前：

- `crates/recursive-cli/src/cli/builder.rs` 有一份**模块私有**的
  `static ENV_LOCK` + `EnvGuard`，只序列化 builder 自己的 env 写入；
- `main.rs` 测试模块里有 **39** 处 `std::env::set_var`/`remove_var`
  （`RECURSIVE_PROVIDER_TYPE` / `RECURSIVE_SANDBOX` /
  `RECURSIVE_PLAN_APPROVAL_TIMEOUT_SECS` / `RECURSIVE_HTTP_AUTH_*` /
  `RECURSIVE_API_KEY` / `RECURSIVE_TRACE_SPANS` / `RECURSIVE_LOG_FORMAT` /
  `ENV_S3_BUCKET` / `RECURSIVE_HOME`），
  `cli/resume.rs` 测试模块另有 **6** 处（`RECURSIVE_HOME`），都不取那把锁；
- 更关键的是 **读者**：`main.rs::tests::build_runtime_construction_smoke` 与
  `build_runtime_threads_per_turn_slot_into_agent_tool` 通过 `build_runtime`
  解析 agent preset（进程 env `RECURSIVE_AGENT_PRESET`），却完全不持锁。

于是 builder 的 `unknown_preset_env_fails_the_cli_build` 在持锁的情况下把
`RECURSIVE_AGENT_PRESET=no-such-preset` 写进进程 env 的那一瞬间，会被上面两个
reader 读到 → `unknown agent preset "no-such-preset" (known: standard, minimal)`。
锁只保护写者、不保护读者，所以是**隔离缺陷**而不是逻辑缺陷。

## 改动

新增 `crates/recursive-cli/src/test_env.rs`（`#[cfg(test)] mod test_env`，
crate 级唯一）：

- `static ENV_LOCK: Mutex<()>` + `drop` 还原前值的 `EnvGuard`；
- API：`EnvGuard::lock()`（只持锁、不改 env，给 reader 用）、
  `EnvGuard::set(&[(&str, Option<&str>)])`（builder 原有签名，14 处调用点零改动）、
  `set_var(..)` / `remove_var(..)`（同一把锁内做多次变更，首次变更快照原值）；
- `Drop` 里统一做 `unsafe { set_var/remove_var }` 还原；`ENV_LOCK` 中毒容忍；
- 自带 2 条自测：`env_guard_holds_the_crate_wide_lock`（活着时
  `ENV_LOCK.try_lock()` 必须失败）、`env_guard_restores_the_previous_value_on_drop`。

迁移：

| 文件 | 改动 |
|---|---|
| `cli/builder.rs` | 删除私有 `ENV_LOCK`/`EnvGuard`（4 处 raw env 调用随之搬进 `test_env.rs`），改 `use crate::test_env::EnvGuard;`；**14** 处既有 `EnvGuard::set(..)` 调用点文本不变，自动改用共享锁 |
| `main.rs` | 测试模块 **39** 处 raw `set_var`/`remove_var` 全部改走 guard；两个 `build_runtime` reader 测试新增 `EnvGuard::lock()` 持锁（这是真正止血的一步）；`main()` 里 `--sandbox` 的产品代码 `set_var`（原 `:588`，现 `:593`）**保持不动** |
| `cli/resume.rs` | **6** 处 raw env 调用 + 私有 `HOME_LOCK`/`pin_recursive_home`/`restore_recursive_home`（各 3 个调用点）删除，统一用共享 guard 的 `lock()`+`set_var` |

**raw `set_var`/`remove_var` 迁移总数 = 49**（builder 4 + main 39 + resume 6），
外加 14 处 builder `EnvGuard::set` 调用点重新指向 crate 级 guard、3 处 resume
`HOME_LOCK` 调用点、2 处 main reader 新增持锁。测试数 169 → 171（+2 自测）。
env 变量的语义、`unsafe { … }` 语义均未变——`unsafe` 块只是从各调用点收敛进
`test_env.rs` 的 guard 实现里（edition 2021 下两者行为完全一致）。

## 验证

### 1. 修前复现（红）

未改源码，`cargo test -p recursive-cli --bin recursive` 默认并行连跑 15 次：
**15/15 全红**，两个用例交替（实际每次都同时）报错：

```
--- RUN 1: FAILED ---
test tests::build_runtime_construction_smoke ... FAILED
test tests::build_runtime_threads_per_turn_slot_into_agent_tool ... FAILED
thread 'tests::build_runtime_construction_smoke' (2525519) panicked at crates/recursive-cli/src/main.rs:3990:9:
thread 'tests::build_runtime_threads_per_turn_slot_into_agent_tool' (2525520) panicked at crates/recursive-cli/src/main.rs:4080:10:
build_runtime with a per-turn slot: unknown agent preset "no-such-preset" (known: standard, minimal)
test result: FAILED. 167 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 11.51s
...
=== total failed runs: 15 / 15 ===
```

对照（同一次脚本最后一段）：

```
=== single-thread control ===
test result: ok. 169 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 37.12s
```

串行全绿 + 并行必红 ⇒ 是隔离问题。

### 2. 负向对照（证明新安排是承重的）

把 `test_env.rs` 的 `EnvGuard::lock()` 临时改成持一把**每次新建、互不相干**的
私锁（等于把锁拿掉；备份 `/tmp/test_env.rs.orig`，sha256
`7be9cc1e…77d`）。默认并行连跑 5 次：**5/5 全红**，且不只是原来的两个用例——

```
test cli::builder::tests::unknown_preset_env_fails_the_cli_build ... FAILED
test test_env::env_guard_holds_the_crate_wide_lock ... FAILED
test tests::build_runtime_threads_per_turn_slot_into_agent_tool ... FAILED
...
test result: FAILED. 166 passed; 5 failed; ... 
```

其中 `env_guard_holds_the_crate_wide_lock` 每次都红，正是"锁被绕过"的直接证据；
`cli::builder` 的 preset / skills 用例也开始互相踩。随后按备份还原，
`shasum -a 256` 与改动前完全一致（`7be9cc1ec0d19d0e3d3aea787644ab1a4f3a89b99cd4bfe1a67b8e7e535e577d`），
还原后重跑默认并行全绿。

### 3. 验收

`for i in $(seq 1 20); do cargo test -p recursive-cli --bin recursive || break; done`
—— **跑满 20 次，20/20 全绿**（每次都是 `ok. 171 passed; 0 failed`，19~22s/次；
本机当时被同仓其它 worktree 的 cargo 测试占满 CPU，故单次偏慢）：

```
RUN 1: test result: ok. 171 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 19.80s
...
RUN 20: test result: ok. 171 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 19.39s
=== parallel acceptance: 20 green / 0 red (of 20) ===
```

串行对照（同一次脚本末尾）：

```
=== single-thread control ===
test result: ok. 171 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 37.68s
```

### 4. 门禁

| 命令 | 结果 |
|---|---|
| `cargo test -p recursive-cli --bin recursive` | ok. 171 passed; 0 failed |
| `cargo test -p recursive-cli`（含 6 个 integration target） | 全绿（见 `/tmp/gates185.log`） |
| `cargo test --test invariants` | 全绿 |
| `cargo clippy -p recursive-cli --all-targets -- -D warnings` | 无 warning |
| `cargo fmt --all -- --check` | clean |

## Notes

### 规格与现实的漂移（逐条）

1. **基线 SHA**：issue 写 `基线 HEAD = 14bac15a`；本 worktree 由父 agent 指定为
   `origin/main` @ `6c724056`（更新的 base），已按后者实施。
2. **行号**：issue 的 `builder.rs:1155-1177`、`:1710`、`main.rs:583/3992/4193-4229/…`
   都对得上但整体偏移（builder guard 实为 `:1155-1188`，preset 测试 `:1706`）；
   `main.rs` 的 raw 站点实测为 `:4009`、`:4210`、`:4224`、`:4238`、`:4246`、
   `:4389-4436`、`:4645-4702`、`:4710-4719`、`:4736-4742`、`:4752`、`:4780-4809`。
3. **`main.rs:583/588` 不是测试代码**：它在 `async fn main()` 里（`--sandbox`
   覆盖 `RECURSIVE_SANDBOX` 的产品路径），issue 把它列进"测试直接 set_var"
   是笔误。按"不改产品语义"约束**保持原样**，全 crate 现在只剩这一处产品
   `set_var`。
4. **用例数**：issue 说串行 168 项；本 base 实测 169 项，修后 171 项（+2 自测）。
5. **"src/ 侧已有 env_lock 先例可复用"不适用于本 crate**：`recursive::test_util::env_lock`
   在 lib 侧且要 `test-utils` feature，bin 的 `#[cfg(test)]` 拿不到，
   所以新 guard 落在 `recursive-cli` 自己的 `test_env.rs`（invariant #4：测试随代码）。
6. **规格里"所有 `set_var` 都改走 guard"单独并不足以止血**：真正红的路径是
   *读者*（`build_runtime` 两个用例）没持锁。因此额外给这 2 个 reader 测试加了
   `EnvGuard::lock()`。规格的选项 A 描述里没点明这点。
7. **可选静态检查未做**：spec 的"测试代码里 `set_var` 只能出现在 `test_env.rs`"
   grep 断言标了"可选"；且 `main.rs:593` 是产品代码，naive grep 会误报。
   改以 guard 自测（`env_guard_holds_the_crate_wide_lock`）承载"锁不可被绕过"，
   它正是负向对照里失败的那条。
8. **resume.rs 原本用 tokio Mutex 的 `HOME_LOCK`**（注释称"guard 要跨 await"）
   已并入共享 guard 的 `std::sync::Mutex`。这与 `builder.rs` 既有做法一致：
   持 `EnvGuard` 跨 `.await` 是刻意的——它只和其它测试争抢，不会拖死各自的
   runtime；`cargo clippy -D warnings` 全绿（`clippy::await_holding_lock`
   看不到这层 wrapper，所以调用点无需 `#[allow]`，实测加了反而多余，已去掉）。
9. 未新增依赖；未动 `src/`（lib）、`crates/recursive-tui` 或其它 crate。
10. 未 push、未建 PR、未发 gist、未在 issue 下评论；只有本地提交。
