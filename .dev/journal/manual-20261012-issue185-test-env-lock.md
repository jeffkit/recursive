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
  `EnvGuard::set(&[(&str, Option<&str>)])`（builder 原有签名，**13** 处调用点零改动；
  `grep -c "EnvGuard::set"` 得到 14，多出来的 1 处是 `builder.rs:894` 的 doc 注释）、
  `set_var(..)` / `remove_var(..)`（同一把锁内做多次变更，首次变更快照原值）；
- `Drop` 里统一做 `unsafe { set_var/remove_var }` 还原；`ENV_LOCK` 中毒容忍；
- 自带 2 条自测：`env_guard_holds_the_crate_wide_lock`（活着时
  `ENV_LOCK.try_lock()` 必须失败）、`env_guard_restores_the_previous_value_on_drop`。

迁移：

| 文件 | 改动 |
|---|---|
| `cli/builder.rs` | 删除私有 `ENV_LOCK`/`EnvGuard`（4 处 raw env 调用随之搬进 `test_env.rs`），改 `use crate::test_env::EnvGuard;`；**13** 处既有 `EnvGuard::set(..)` 调用点文本不变，自动改用共享锁（grep 到的第 14 处是 doc 注释） |
| `main.rs` | 测试模块 **39** 处 raw `set_var`/`remove_var` 全部改走 guard；两个 `build_runtime` reader 测试新增 `EnvGuard::lock()` 持锁（这是真正止血的一步）；`main()` 里 `--sandbox` 的产品代码 `set_var`（原 `:588`，现 `:593`）**保持不动** |
| `cli/resume.rs` | **6** 处 raw env 调用 + 私有 `HOME_LOCK`/`pin_recursive_home`/`restore_recursive_home`（各 3 个调用点）删除，统一用共享 guard 的 `lock()`+`set_var` |

**raw `set_var`/`remove_var` 迁移总数 = 49**（builder 4 + main 39 + resume 6），
外加 13 处 builder `EnvGuard::set` 调用点重新指向 crate 级 guard、3 处 resume
`HOME_LOCK` 调用点；reader 侧补锁见下方"返工"一节。测试数 169 → 171（+2 自测）。
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
   *读者*（`build_runtime` 用例）没持锁。第一轮只补了 `main.rs` 的 2 个 reader，
   漏了 `builder.rs` 的 2 个，见"返工"一节。
7. **静态 grep 不变量**：第一轮以 guard 自测（`env_guard_holds_the_crate_wide_lock`）
   代替，复查指出这只能证明"活着的 guard 持有锁"，挡不住**将来新增**一处绕过
   guard 的 raw `set_var`。第二轮已按 spec 补
   `tests/invariants/test_env_isolation.rs`（豁免产品代码 `main.rs` 的 `--sandbox`）。
8. **resume.rs 原本用 tokio Mutex 的 `HOME_LOCK`**（注释称"guard 要跨 await"）
   已并入共享 guard 的 `std::sync::Mutex`。这与 `builder.rs` 既有做法一致：
   持 `EnvGuard` 跨 `.await` 是刻意的——它只和其它测试争抢，不会拖死各自的
   runtime。`clippy::await_holding_lock` 穿不过 `EnvGuard` 这层 wrapper，所以
   调用点无需 `#[allow]`：**本补丁未新增任何 `#[allow]`**；crate 内仍有 3 处
   **先前既有**的 `#[allow(clippy::await_holding_lock)]`
   （`builder.rs:1606/1700/1735`，均在本补丁之前就在），clippy 依旧全绿。
9. 未新增依赖；未动 `src/`（lib）、`crates/recursive-tui` 或其它 crate。
10. 未 push、未建 PR、未发 gist、未在 issue 下评论；只有本地提交。

## 返工（independent review 后，commit 2）

### reviewer 的发现（已在其树上复现，我也在自己的树上复现）

第一轮 `2c73f040` 把 `main.rs` 的 2 个 preset reader 锁上了，但 `builder.rs` 还有
2 个 `build_runtime` reader 完全没锁：

- `builder.rs:1559` `build_runtime_seeds_the_transcript_from_seed_messages`
- `builder.rs:1777` `build_runtime_uses_the_anthropic_endpoint_for_an_anthropic_config`

（`build_runtime` 在 `builder.rs:732` 经 `PresetEnv::from_process()` 读进程 env。）

定向三元组（默认并行）：

```bash
cargo test -p recursive-cli --bin recursive -- \
  build_runtime_seeds build_runtime_uses_the_anthropic unknown_preset_env_fails
```

第一轮代码在我的树上 **2/6 红**（reviewer 独立跑是 10/12 红）：

```
--- TRIPLE RUN 1: FAILED ---
test cli::builder::tests::build_runtime_uses_the_anthropic_endpoint_for_an_anthropic_config ... FAILED
panicked at crates/recursive-cli/src/cli/builder.rs:1818:10:
build_runtime must build an anthropic provider: unknown agent preset "no-such-preset" (known: standard, minimal)
--- TRIPLE RUN 3: FAILED ---
test cli::builder::tests::build_runtime_seeds_the_transcript_from_seed_messages ... FAILED
panicked at crates/recursive-cli/src/cli/builder.rs:1568:10:
build_runtime must succeed with a valid config: unknown agent preset "no-such-preset" (known: standard, minimal)
```

**为什么第一轮全量 20× 仍然全绿**：libtest 默认按字母序调度，
`build_runtime_*`（'b'）排在 `unknown_preset_env_fails_the_cli_build`（'u'）之前，
reader 在 writer 开始前就跑完了。**这个绿是顺序侥幸**，改名 / 新增用例 / 不同调度
即翻红。

### 改动

1. 上述 2 个 reader 首行加 `let _env = EnvGuard::lock();`
   （anthropic 那条放在 `use tokio::io::AsyncReadExt;` 之后，避免 items-after-statements）。
2. 用同一把锁把**所有读 env 派生状态的测试**补齐（reviewer 的"建议"逐条处理）：

| 位置 | 测试（行号为返工前） | 读什么 | 处置 |
|---|---|---|---|
| main.rs | `hook_timing_flag_accepted` (:4211) | `Cli::parse_from` → RECURSIVE_SANDBOX / LOG_FORMAT | 加锁 |
| main.rs | `hook_timing_flag_defaults_to_false` (:4286) | 同上 | 加锁 |
| main.rs | `sessions_and_agents_accept_the_json_and_show_flags` (:4574) | 同上 | 加锁 |
| builder.rs | `build_tools_returns_shared_read_state_when_none` (:1130) | `build_tools` → RECURSIVE_SANDBOX / HOME / skills env | 加锁 |
| builder.rs | `build_tools_reuses_supplied_read_state_when_some` (:1144) | 同上 | 加锁 |
| builder.rs | `finish_tool_surface_registers_mcp_then_applies_allow_list` (:1354) | 同上 | 加锁 |
| builder.rs | `finish_tool_surface_keeps_full_registry_without_filters` (:1388) | 同上 | 加锁 |
| resume.rs | `cmd_resume_propagates_target_resolution_error` (:1298) | `RECURSIVE_HOME` → user dirs（checkpoint wiring） | 加锁 |
| resume.rs | `cmd_resume_refuses_tool_registry_hash_mismatch` (:1325) | 同上 | 加锁 |
| resume.rs | `cmd_resume_allow_tool_drift_degrades_mismatch_to_warning` (:1361) | 同上 | 加锁 |
| resume.rs | `cmd_resume_accepts_matching_tool_registry_hash` (:1396) | 同上 | 加锁 |
| resume.rs | `run_resumed_propagates_runtime_failure` (:1431) | 同上 | 加锁 |
| resume.rs | `prepare_replay_seed_skip|redo|abort|leaves_a_paired` (:1787/:1802/:1821/:1833) | `tools_for` → `build_tools` → sandbox/HOME/skills env | 各加锁 |
| main.rs | `doctor_probe_disk_writes_and_cleans_up` (:4783) | 只读 `config.workspace`；`probe_disk` 函数体内无任何 env 读取 | **不加锁**（已核实） |

枚举方法：对每个 `mod tests` 内的函数做静态扫描——命中
`build_runtime(` / `build_tools(` / `Cli::parse_from(` / `cmd_resume(` /
`run_resumed(` / `prepare_replay_seed(` / `probe_storage(` 等入口且函数体内没有
`EnvGuard::` 的，逐个判读 env 依赖并补锁。两轮共新增 **18** 处 reader 持锁
（commit 1 的 2 处 + 本轮 16 处）。

### 静态 grep 不变量（spec 的可选项，本轮补上）

新增 `tests/invariants/test_env_isolation.rs`（照 `dep_justification.rs` 风格），
并在 `tests/invariants.rs` 注册。断言：

- 递归扫 `crates/recursive-cli/src/**/*.rs`；`std::env::set_var(` /
  `std::env::remove_var(` 的**调用行**（整行注释忽略）只允许出现在 `test_env.rs`；
- 豁免 `main.rs` 中 `mod tests {` **之前**的产品代码——并断言该豁免**恰好 1 处**、
  且内容含 `RECURSIVE_SANDBOX`（即 `--sandbox` 分支），防止将来有人悄悄扩大豁免面。

`cargo test --test invariants`：**56 passed**（原 55 + 新 1）。

### 负向对照（本轮，两类，均已实跑）

**NC-1：静态不变量确实能抓住"新增一处绕过 guard 的 raw `set_var`"**

在 `cli/resume.rs::tests::cmd_resume_propagates_target_resolution_error` 体内临时插入
一行 `std::env::set_var("RECURSIVE_NC_PROBE", "1");`（不取 guard），跑
`cargo test --test invariants`：

```
test test_env_isolation::cli_process_env_is_mutated_only_by_the_shared_guard ... FAILED
thread 'test_env_isolation::cli_process_env_is_mutated_only_by_the_shared_guard' (5771721) panicked at tests/invariants/test_env_isolation.rs:91:5:
test code mutated the process env without crate::test_env::EnvGuard (issue #185: one shared lock, or the --bin recursive suite races itself). Route these through EnvGuard:
cli/resume.rs:1299: std::env::set_var("RECURSIVE_NC_PROBE", "1"); // NEGATIVE CONTROL: raw set_var bypassing the guard
test result: FAILED. 55 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.01s
```

还原后 `cli/resume.rs` 的 sha256 `5da27d059e28c18a64108edcfe234a391ddd54be47c25229526e8a7363191cf7`
与插桩前一致、探针串 0 处，`cargo test --test invariants` 回到 `ok. 56 passed`。

**豁免方式（精确排除，不是宽泛白名单）**：扫描器只豁免「`main.rs` 中
`mod tests {` 这一行**之前**、且整行是 `std::env::set_var(` / `remove_var(` 调用」的行，
即产品代码段；并**额外断言这样的豁免恰好 1 处、且该行内容必须含
`RECURSIVE_SANDBOX`**（`--sandbox` 分支）。因此：
(a) 测试模块里新增任何 raw 调用 → 红（NC-1 已证）；
(b) 产品段再插第二处 raw 调用 → 数量断言红；
(c) 有人把测试模块挪到产品段之前 → 豁免数变 0 → 红。

**NC-2：两处新 reader 锁是承重的**

从 `builder.rs` 去掉**恰好**那两处新加的 `let _env = EnvGuard::lock();`
（`build_runtime_seeds_the_transcript_from_seed_messages` 与
`build_runtime_uses_the_anthropic_endpoint_for_an_anthropic_config`；
grep 计数 6 → 4，其余 4 处是 `build_tools` / `finish_tool_surface` 测试的锁），
定向三元组连跑 10 次：

```
--- TRIPLE RUN 1: FAILED ---
test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 168 filtered out; finished in 0.43s
  build_runtime must build an anthropic provider: unknown agent preset "no-such-preset" (known: standard, minimal)
--- TRIPLE RUN 4 / 5 / 6 / 9: FAILED ---
panicked at crates/recursive-cli/src/cli/builder.rs:1590:10:
build_runtime must succeed with a valid config: unknown agent preset "no-such-preset" (known: standard, minimal)
=== triple: 5 red / 10 runs ===
```

按 `/tmp/builder.rs.bak` 还原，sha256
`a14096be5fe502baa52504a7174401191271f80aebad48f5ab6578da0c7c6887`
与去掉锁之前一致，三元组立即回到 `ok. 3 passed`。

### 验收数字（本轮，返工后）

- 定向三元组 ×20：**0 red / 20**，每次 `ok. 3 passed; 0 failed; 168 filtered out`（7~8s/次）
- 全量 `--bin recursive` ×20：**0 red / 20**，每次 `ok. 171 passed; 0 failed`（35~44s/次）
- `--test-threads=1` 对照：`ok. 171 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out … 36.89s`

日志 `/tmp/accept185b.log`。对比第一轮：全量 20× 也是 20/20 绿，但那是
**顺序侥幸 + 漏了 2 个 reader**（见上），所以本轮必须同时给定向三元组。

### 门禁（本轮，全部通过）

| 命令 | 结果 |
|---|---|
| `cargo test -p recursive-cli --bin recursive` | `ok. 171 passed; 0 failed`（35.26s） |
| `cargo test -p recursive-cli`（7 个 target） | exit 0；171+28+3+12+17+2+13 全绿 |
| `cargo test --test invariants` | `ok. 56 passed; 0 failed`（含新增静态检查） |
| `cargo clippy -p recursive-cli --all-targets -- -D warnings` | clean |
| `cargo fmt --all -- --check` | clean（首跑红：新 invariants 文件未过 rustfmt，`cargo fmt --all` 后复跑 clean） |
| 额外：`cargo clippy --test invariants -- -D warnings` | clean |

日志 `/tmp/gates185b.log`、`/tmp/gate2b.log`。

### 已知死锁陷阱（可复现的脚枪，务必遵守）

`ENV_LOCK` 是**不可重入**的 `std::sync::Mutex`：

1. **同一测试内持有两个 guard → 挂起。** 一个测试里 `let a = EnvGuard::lock();`
   之后再 `EnvGuard::lock()` / `EnvGuard::set(..)` 会自我死锁
   （默认并行下该测试线程永久等待，`--test-threads=1` 下整个测试二进制挂死）。
   指引：**每个测试最多持有一个 guard**；同一测试内要多次变更 env，就用同一个
   guard 的 `set_var` / `remove_var`（它们不再取锁）。
2. **持锁期间 spawn 出去的任务再去取同一把锁 → 挂起。** `EnvGuard` 只在 `Drop`
   释放锁；持锁时 `tokio::spawn` 的 future 里若调用 `EnvGuard::lock()` / `set(..)`，
   在持有者释放前拿不到锁，而持有者又在 await 该任务 → 死锁。
   指引：**不要在持锁期间让其它任务取锁**；需要并发时让子任务只读已经快照好的值。

当前代码没有触发这两条（本轮新增的持锁测试都不嵌套、也不 spawn 会取锁的任务），
但这是可复现的脚枪，后续任何 reader 加锁都必须避开。
