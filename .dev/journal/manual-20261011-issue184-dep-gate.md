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

## 后续 2 · keeper 复验后的边界收口（2026-10-12）

keeper 的 v2 复验结论是"通过、建议进 PR"，只留下一个**可选、不阻塞**的边界，并给了最小复现：

```toml
[dependencies]
rand = { version = "0.8",
         features = ["a#b"] }
```

`#` 在**字符串里**，却被当成注释起点 → 该行的 `#b"] }` 一起被剥掉 → 花括号深度永远回不到 0 →
pending 条目不输出 → **这个依赖完全不出现在比对结果里**，门禁 `exit 0` 放行（fail-open）。
单行形态同理：`foo = { git = "https://example.invalid/x#v1" }` 也会整条消失。
该洞在 v1 就存在（非 v2 回归），但"本该失败关闭的门在静默漏判"正是本单要根除的失败模式，故收掉。

**修法**：awk 增加 `strip_comment()`，只在**双引号外**把 `#` 当注释起点（字符串内保留 `\` 转义）；
表头行与正文行都改用它，替换原先的 `sub(/[[:space:]]*#.*/, "")`。

新增回归用例 2 条：

| 用例 | 断言 |
|---|---|
| `dep_gate_flags_hash_inside_inline_table_string` | `features = ["a#b"]` 未说明 → 红，且必须点名 `rand` |
| `dep_gate_flags_single_line_spec_with_hash` | `foo = { git = "…/x#v1" }` 未说明 → 红，且必须点名 `foo` |

另按 keeper 建议，把门禁退出码写进 `.dev/AGENTS.md` 不变量 #6 的说明（`2` = base ref 不可解析 = 失败关闭）。

**验证**：

- 夹具矩阵扩到 15 例：v2 **13/15**（漏的两例报 `OK: No dependency changes detected`）、v3 **15/15**。
- 负向对照：把 2 条新用例对着 **v2 脚本**跑 → **2/2 转红**，报错
  `invariant #6 must fail for … but the gate passed`；脚本还原并 md5 校验一致。
- `cargo test --test invariants` 65 passed（+2）；`cargo clippy --test invariants -- -D warnings`、
  `cargo fmt --all -- --check` 干净；`check-new-deps.sh eef5f42b^` 仍识别 `libc`。

## 后续 3 · 自审出的同类 fail-open：`[dependencies.foo]` 表内部改动被忽略（2026-10-12）

keeper 未提这一条，是我复核自己 README 里"覆盖 `[dependencies.foo]` 表形态"这句时发现它是 **overclaim**：

表形态走的是 `printf "%s\t<table>"` —— 只要表名还在，spec 恒为 `<table>`，于是

```toml
[dependencies.serde]
version = "1"                →   version = "2"
                                 features = ["derive"]
```

两侧条目完全相同 → 门禁 `exit 0`（实测 v3 报 `No dependency changes detected`）。同属"本该失败关闭却静默漏判"。

**修法**：表形态改为把**整个表体**当作 spec —— 进入表形态时 `pending = <表名>`、`spec = "<table>"`，
其后每行（去注释后）追加进 spec，到下一个表头时由 `emit()` 统一输出。
这引入一个新的 EOF 风险：**文件最后一个表若没有后续表头就永不输出** → 增加 `END { emit() }` 兜底。

**验证**：

- 夹具矩阵扩到 19 例：v3 **18/19**（漏的正是"表体改动"）、v4 **19/19**。
- 负向对照 A（表体改动）：2 条新用例对着 **v3 脚本**跑 →
  `dep_gate_flags_edits_inside_a_table_form_dependency` **转红**
  （`invariant #6 must fail for an edit inside a [dependencies.foo] table, but the gate passed`）；
  如实记录：`dep_gate_reports_table_form_dependency_at_eof` 在 v3 上是**绿**的（v3 立即输出、不会丢末尾表），
  它守的是 v4 的新缓冲设计，不是 v3 的 bug。
- 负向对照 B（END flush 的必要性）：保留 v4 但删掉 `END { emit() }` → `..._at_eof` **转红**；
  shell 层同理（去掉该行后夹具 18/19）。
- `cargo test --test invariants` 67 passed（+2）；clippy `-D warnings`、`fmt --all -- --check` 干净；
  `check-new-deps.sh eef5f42b^` 仍识别 `libc`；本仓 `HEAD~1` 报 `Cargo.toml unchanged`（exit 0，符合预期）。

## 后续 4 · 系统性形态扫描：非裸标识符的键被整条跳过（2026-10-12）

keeper 没提这一条，是我把"依赖表比对"的**键形态**做了一遍系统扫描（8 种键/值形态 × 新增依赖），
发现 3 种会让依赖**整条从比对里消失**（同为 fail-open）：

| 形态 | v4 行为 |
|---|---|
| `"quoted-crate" = "1"` | 漏判（键正则只认裸标识符） |
| `dotted.crate = "1"` | 漏判 |
| `"quoted-inline" = { version = "1" }` | 漏判 |
| `[dependencies."quoted-table"]` | 能发现，但条目名带引号（journal 匹配会错） |

**修法**：

- 键正则改为「裸键 | 双引号键 | 单引号键」并允许点分组合；正则在 `BEGIN` 里拼成 `KEYPAT`。
- 新增 `strip_quotes()` 去掉键上的 TOML 引号（引号是语法、不是 crate 名），让 journal 匹配用真实名字；
  单引号字符统一写 `\047`。

**踩坑记录（值得写下来）**：`bash -n` **抓不到**这类内嵌引号错误。我先后在 awk 程序里写了 `["']`、
在注释里写了 `shell's`、在正则里写了 `'[^']*'` —— 三次 `bash -n` 都通过，但**实际执行**报
`syntax error near unexpected token`。结论：**这类脚本必须实跑（至少跑一遍夹具），不能只做语法检查。**

**验证**：

- 夹具矩阵扩到 23 例：v4 **21/23**、v5 **23/23**。
- 负向对照：3 条新用例对着 **v4 脚本**跑 → `quoted` 与 `dotted` 两条**转红**（`… but the gate passed`）；
  如实说明第三条（`quoted table-form`）在 v4 上是**绿**的 —— v4 会立即输出表头名（带引号），
  它守的是 v5 的引号归一化与整体键模式，不是 v4 的漏判。
- `cargo test --test invariants` **70 passed**（+3）；`clippy -D warnings`、`fmt --all -- --check` 干净；
  `check-new-deps.sh eef5f42b^` 仍识别 `libc`。

## 后续 5 · keeper 复验 v3 后的收官（2026-10-12）

keeper 对 **v3（`357dfe93`）** 的复验通过：补丁完整性（干净基线 apply + 增量链逐字节一致）、15 例矩阵 7/12/13/15 逐个复现、
Rust 侧 65 passed、负向对照 2/2 转红、真仓冒烟与 CI 回路均一致；结论是"建议收官落地，不再加码"。

它留了一条**只记不改**的残余边界：`strip_comment()` 只跟踪双引号，单引号字面量（`'1#x'`）与多行字面量（`'''`/`"""`）
里的 `#` 仍会被当注释起点。**按实测更正它的说法**（它测的是 v3 之前的行为）：

- 被截断的那条依赖**仍会被报出来** —— v4 加的 `END { emit() }` 会在 EOF 把未闭合条目 flush 出去；
- **真正漏的是同表里它后面的条目**：闭合括号被吃掉 → 深度回不到 0 → 后续同级条目被并进上一条的 spec、永不参与比对。

实测（本仓 worktree，HEAD~1 为基线）：

```
[dependencies]
keep = "1"
bad  = { version = '1#x' }      # 截断成 `bad = { version = '1`
newdep = "2"                    # 新依赖 → 未被报出（并入 bad 的 spec）
```
→ `Changed dependencies: bad`（只报 bad）。即：**漏的是 `newdep`**，而 keeper 描述的"整条被丢掉"在 v4 之后已不成立。

按 keeper 建议**不改代码**，改为在 `scripts/check-new-deps.sh` 头部注释里写清**实测后果**与"若真出现该怎么补"
（加 `'` 状态位、覆盖 `'''`，并**针对"后续同级条目"这一真正泄漏点**加夹具用例）。本题材不会由 cargo 产出
（`[dependencies]` 恒为双引号、`cargo add`/Cargo.lock 往返也不会生成），继续处理等于手写 TOML 词法器。

**验证**：注释为纯文档改动，仍实跑 `check-new-deps.sh HEAD~1`（exit 0）与 23 例夹具（23/23）确认未破坏任何行为。
