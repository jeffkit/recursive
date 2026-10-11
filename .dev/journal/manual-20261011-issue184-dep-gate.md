# manual-20261011 — 不变量 #6 依赖门空转（issue #184）

- **Date**: 2026-10-11 (Asia/Shanghai)
- **Issue**: https://github.com/jeffkit/recursive/issues/184（P2，属 #194 工程门禁簇）
- **Base**: `origin/main` = 6c724056（worktree `fix/184-dep-gate`）

## 根因（两处，缺一不可）

1. `scripts/check-new-deps.sh:38` 的过滤器 `grep -v '^[+-]\+\|^[+-]version\|…'`：
   `\+` 在 BRE 里是「一个或多个」，`^[+-]\+` 于是匹配**所有** `+`/`-` 开头
   的行 → `DEP_CHANGES` 恒空 → 打印 `OK: No dependency changes detected` 并
   `exit 0`。门禁的判定分支从未被执行过。
2. CI 浅克隆：`actions/checkout@v4` 默认 `fetch-depth: 1`，`HEAD~1` 不存在；
   `:31` 用 `2>/dev/null` 把 git 的报错吞掉，于是走「Cargo.toml unchanged」
   分支静默通过。本地/CI 都没有第二道防线：`tests/invariants/dep_justification.rs`
   只断言「脚本存在且退出 0」，脚本永远 0 → 测试永远绿。

## 改动

- `scripts/check-new-deps.sh`
  - **失败关闭**：base ref 无法解析时 `exit 2` 并打印原因（CI 需 fetch-depth ≥ 2），
    不再假报「unchanged」。
  - **按依赖表比对，而非按 diff 文本正则**：新增 `list_dependency_entries()`
    （awk），从 base/HEAD 两侧的 Cargo.toml 各自抽出 `<name>\t<spec>`，
    用 `comm -13` 求「只在 HEAD 侧出现」的条目 —— 新增依赖与 spec 变更都算，
    删依赖不算，`[package] version = "0.8.3"` 这类元数据改动天然不可见。
    同时覆盖 `[target.'cfg(..)'.dependencies]`、`[workspace.dependencies]`、
    `[dependencies.foo]` 表形态。
  - 删掉 `grep -oP`（GNU 扩展，macOS/BSD grep 不支持）；所有 git 调用改为
    `git -C "$WORKSPACE"`，判定不再依赖调用方 cwd；变更文件列表只取一次
    （原先每个 journal 文件都跑一次 `git diff`，697 个 journal → 697 次子进程）。
- `.github/workflows/ci.yml`：test job 的 checkout 改 `fetch-depth: 2`（`HEAD~1` 才存在）。
- `tests/invariants/dep_justification.rs`：新增 5 个**门禁自证**用例（抛盘仓库
  夹具 + 真实脚本）：未说明的新依赖必红（含指名道姓）、同 run journal 说明后
  必绿、`[package]` 版本号变更必绿、base ref 缺失必 `exit 2`、换 cwd 判定不变。

## 验证

夹具矩阵（同一套 9 例，脚本换成 `origin/main` 版本 vs 修复版本）：

| 用例 | 旧脚本 | 新脚本 |
|---|---|---|
| 新增依赖、无 journal | **0（漏）** | 1 |
| 已有依赖 spec 变更 | **0（漏）** | 1 |
| base ref 不存在 | **0（漏，假报 unchanged）** | 2 |
| 从无关 cwd 调用 | **0（漏）** | 1 |
| package version 变更 | 0 | 0 |
| `[package]` 元数据变更 | 0 | 0 |
| 新增依赖 + 本 run journal 说明 | 0 | 0 |
| 删除依赖 | 0 | 0 |
| Cargo.toml 未改动 | 0 | 0 |

- 旧脚本 5/9，新脚本 9/9。
- 真仓历史冒烟：`bash scripts/check-new-deps.sh eef5f42b^` → 现在能识别出 #150
  引入的 `libc`（`target.'cfg(target_os = "linux")'.dependencies`），并命中
  `manual-20261010-issue150-parent-environ.md` 的说明 → exit 0；旧脚本对该 commit
  同样报「No dependency changes detected」。
- `cargo test --test invariants dep_`：8/8 通过（3 个原有 + 5 个新增）。
- `cargo fmt --all -- --check`、`cargo clippy --test invariants -- -D warnings` 干净。

## Notes（未做）

- journal 侧的**兜底分支**（任一历史 journal 提到该依赖即算已说明）本次保留：
  改成「只认本 run 的 journal」是策略收紧，会让「重新引入一个历史提过的依赖」
  之类合法改动变红，需单独评审；#184 正文的 5 条设计项里也不含它。
- `crates/*/Cargo.toml` 与 `Cargo.lock` 仍未纳入不变量 #6 的检查面（本单不改面）。

## 后续 · review 反馈收口（2026-10-11 晚）

issue-keeper 的独立复验（#184 评论）通过全部验收项，只指出一处**非阻塞误报**：跨行内联表依赖。

```toml
[dependencies]
rand = { version = "0.8", default-features = false,
         features = ["small_rng"] }
```

续行 `features = [...]` 命中键正则 → 被当成名为 `features` 的新依赖 → 即使 journal 已正确说明
`rand` 也照样 exit 1（门禁失败关闭 ⇒ 一次误报挡一次合法提交）。

**修法**：`list_dependency_entries()` 内维护**花括号深度**，只有 depth == 0 的行才算新依赖；
并把整段（可能跨行的）spec 缓冲到表闭合再输出 —— 顺带修掉「行内表内部的编辑不算 spec 变更」
这个漏判。

新增 3 条回归用例（`tests/invariants/dep_justification.rs`）：

| 用例 | 断言 |
|---|---|
| `dep_gate_ignores_inline_table_continuation_keys` | 未说明 → 红，且**只点名 `rand`**，输出中不得出现 `features` |
| `dep_gate_passes_when_inline_table_dependency_is_justified` | journal 说明 `rand` → 绿（v1 上会假红） |
| `dep_gate_flags_edits_inside_an_inline_table` | 行内表内部加 feature（首行未变）→ 红 |

**验证**：

- 夹具矩阵扩到 12 例：`origin/main` 6/12、v1（本单首版补丁 `f35ecf02`）10/12、本次 12/12。
- 负向对照：把新增的 3 个用例对着 v1 脚本跑 → **3/3 转红**，报错正是
  `✗ features: NO JOURNAL JUSTIFICATION FOUND`（证明新断言不是同义反复）；脚本随后还原，
  md5 校验一致。
- `cargo test --test invariants` 63 passed（+3）；`cargo clippy --test invariants -- -D warnings`、
  `cargo fmt --all -- --check` 干净；`check-new-deps.sh eef5f42b^` 仍识别 `libc`。
