# #242 — 85bac8fb builder.rs scope 幸存者 2 条收敛（skill_source_sha256_pins）

> B2 席 2026-10-25 随修复提交进仓（协议 §3）。证据原址：本文 + commit message；
> 负控日志为本地临时文件，关键行已转录于下，不改写数字。

## 基线与源 run

- **基线 sha**：aa8a5cb7（origin/main，本席 wt-b2 `reset --hard` 起点）。
- **源 CI run**：[40144987139](https://github.com/jeffkit/recursive/actions/runs/40144987139)
  @ 85bac8fb（#231 落地）——mutants.yml push→main 落地门，`ubuntu-latest`，
  **files scope**：85bac8fb 触及 `crates/recursive-cli/src/cli/builder.rs`
  （`file_config_or_report`），整文件进变异范围。
- **结果**：2 MISSED，均落在 **邻函数** `skill_source_sha256_pins`
  （builder.rs:682-691，#74 拆单 3/3 预存代码）；`file_config_or_report`
  本体零 MISSED（#231 的 3 条定向测试全杀）。

## 幸存者清单与逐条处置

| # | mutant（run 日志原文） | 处置 |
|---|---|---|
| 1 | `builder.rs:683:5: replace skill_source_sha256_pins -> Vec<String> with vec![String::new()]` | **钉死**-`skill_source_sha256_pins_unset_or_separators_only_is_empty` / `skill_source_sha256_pins_parses_positionally_trims_and_drops_empties` / `skills_from_http_sources_unpinned_multi_url_clears_pin_count_gate`（三钉各自独立杀） |
| 2 | `builder.rs:683:5: replace skill_source_sha256_pins -> Vec<String> with vec!["xyzzy".into()]` | **钉死**-同上三钉各自独立杀 |

零 `exclude_re`、零等价论证：两条都是真可杀——函数对 unset / 空白 / 多 pin
三类输入的外延输出与 mutant 不同，只是从未有测试直接观测它。

### 幸存根因（为什么既有 6 条 `skills_from_http_sources_*` 测试没杀）

`skill_source_sha256_pins` 是私有 fn，唯一调用方 `skills_from_http_sources`
（:604）；mutant 返回 **长度 1** 的 Vec。既有测试逐条对表：

- `unset_or_blank_is_empty`：URL 未设 → :596 早返回，pins 根本不求值；
- `plain_http_url_errors` / `allowlist_blocks_unlisted_host` /
  `allowlist_admits_listed_host` / `propagates_request_failure`：**恰 1 URL**
  + SHA256 未设 → 真码 pins 空跳过计数门；mutant 1 幻影 pin == 1 URL，
  `pins.len() != urls.len()` 同为 false，计数门同样静默，随后错误（https
  策略 / allowlist / ECONNREFUSED）与 pin 无关；
- `pin_count_must_match_url_count`：2 URL + 1 真 pin → Err；mutant 也是 1 pin
  → 同一 Err 同一句。

即：**没有任何测试区分「pins 为空」与「1 个幻影 pin」**，也没有测试断言
pin 内容 / trim / 去空 / 顺序。

### 三钉设计（builder.rs `mod tests`，+71 行，产品逻辑零改动）

1. `skills_from_http_sources_unpinned_multi_url_clears_pin_count_gate`（:1096）
   ——**调用方层**：2 个 plain-http URL + SHA256/HOSTS 均 None。真码 pins 空
   → 跳过计数门 → 第一 URL 撞 https-only 策略（`err.contains("https://")`）；
   mutant 1 幻影 pin ≠ 2 URL → 计数门 Err「has 1 pin(s) for 2 URL(s)」
   （`!err.contains("pin")` 兜底）。这一钉同时说明 mutant 的现实爆炸半径：
   **幻影 pin 会让所有未 pin 的 ≥2 URL 部署在启动即被拒**。无网络、无盘、
   `#[test]`（https 门在运行时前触发，先例 `plain_http_url_errors`）。
2. `skill_source_sha256_pins_unset_or_separators_only_is_empty`（:1121）
   ——None / `"   "` / `" , ,"` 三臂各自 scoped（EnvGuard 非可重入锁先例）
   → 均 `is_empty()`。
3. `skill_source_sha256_pins_parses_positionally_trims_and_drops_empties`
   （:1149）——`" aaaa , ,bbbb,"` → `["aaaa","bbbb"]`（trim / 去空 / 保序 =
   `pins[i]` ↔ `urls[i]` 的位置契约）。

## 负控实证记录

命令（定向，分钟级；§2b 禁本地全量 mutants）：
`cargo test -p recursive-cli --bin recursive -- skill_source_sha256_pins skills_from_http_sources_unpinned_multi_url`

- **正控**（真码）：`3 passed; 0 failed`。
- **mutant 1**（手工把函数体整替为 `vec![String::new()] /* ~ changed by cargo-mutants ~ */`）：
  EXIT=101，`0 passed; 3 failed`——
  - `..._parses_positionally_...`：`assertion left == right failed: pins must be trimmed, de-blanked, and kept in URL order`
  - `..._unset_or_separators_only_...`：`unset env must disable pinning`（第一臂即红）
  - `skills_from_http_sources_unpinned_multi_url_...`：`unpinned URLs must reach the https-only policy, not the pin gate: RECURSIVE_SKILL_SOURCE_SHA256 has 1 pin(s) for 2 URL(s); provide one comma-separated pin per URL (in URL order)`
- **mutant 2**（`vec!["xyzzy".into()] /* ~ changed by cargo-mutants ~ */`）：
  EXIT=101，`0 passed; 3 failed`，三条失败点与诊断句逐字同 mutant 1。
- **还原**：两次均 `cmp` 对负控前快照 **逐字节一致**（快照取于 fmt 之前；
  其后 `cargo fmt --all` 只改本席测试里一处 `EnvGuard::set` 折行，产品函数
  :682-691 自基线起未动——`git diff` 确认 71 行全在 `mod tests`）。

6/6 红格（2 mutant × 3 钉）：非「忘了杀」，亦非等价类。

## 三门（无管道、各自落盘、`;` 隔开；席位记忆 zsh 退出码口径）

- `cargo fmt --all -- --check`：**EXIT=0**（fmt 应用后复检）。
- `cargo test --workspace`：**EXIT=0**（74 个测试二进制，5293 passed / 0 failed /
  12 ignored——ignored 全为 #224 预存 windows 项）。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：**EXIT=0**
  （`--workspace` 超集，蕴含 AGENTS.md 字面根包命令通过）。

## ubuntu-only 自查

三钉只依赖进程 env（`crate::test_env::EnvGuard`，crate 级单锁串行）与
`HttpSkillSource::new` 的纯校验路径：无网络、无文件系统、无 `cfg(unix)` /
平台分支、无端口绑定。与 CI `ubuntu-latest` runner 无平台差异面。

## 终判 run

**待填**：本 commit 落地 main 后 mutants.yml push→main 的 Mutation run
（协议 §3「独立进程 recheck = CI runner」）。预期 builder.rs scope 零 MISSED。

## 复核口径

- §3：CI Mutation run 终裁；本地不做全量复验（§2b）。
- 本文「负控实证」= 手工单点变异 + 定向断言，分钟级，与 #223 批 3 / #228 同口径。
- GitNexus MCP 本会话不可用（工具表无 `gitnexus_*`）；本单零产品符号改动，
  blast radius 手工核：`skill_source_sha256_pins` 唯一调用方
  `skills_from_http_sources`（:604），后者本单未动。
