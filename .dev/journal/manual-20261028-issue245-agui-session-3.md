# manual 2026-10-28 — #245 agui_session 3 条钉死（wt-b2 席，小联合批 ②）

- **Date:** 2026-10-28（主控轮值注入标 2026-10-29 22:0x；基线 origin/main = 07162139 + #247 对账 docs commit 6c1d274a）
- **Goal:** #245——full 首审 run 40598348438（b2a38189 上 `mutants --all (recursive-agent)`，76 MISSED 其中 2 条为
  「treating as FAILURE」汇总行，实体 74）的 `src/agui_session.rs` 段 3 条；#152 复核后的残余，主控点名小联合批 ②
- **Files touched:** `src/agui_session.rs`（+162，全在 `#[cfg(test)] mod tests`，生产半区零触）；本 journal
- **Tests added:** 3 fn（模块 14 passed / 0 failed）
- **Notes:** 产品逻辑零改动；无 exclude_re/无 mutants.toml 改动（3 条全钉死、零豁免，故无 --list 自检面）；#222/#224
  未触；本地 mutants 未跑（§2b，击杀力以手工变异负控实证）；GitNexus MCP 本会话不可用，纯测试半区无产品符号

## 逐条：MISSED 根因 → 钉 → 负控（全部一手重取）

### 1. `:69:5` replace `legacy_thread_dir_name -> String` with `String::new()`

- **MISSED 根因（自洽盲区）**：既有 `legacy_flat_thread_is_migrated_and_becomes_visible` 用**同一被变异函数**做 setup
  （种子 legacy 目录名）与验证（生产查找经 `legacy_session_dir`），变异后两侧同变、自洽通过。
- **钉**：`legacy_migration_finds_the_flat_dir_by_its_documented_name`——种子侧独立拼写
  （`format!("agui-{}", legacy_sanitize_thread_id(thread))`，不经 :69 面）+ 直接断言 helper 拼写 == 文档名 + 行为断言
  （`resolve_session_dir` 按文档名找到种子目录并完成迁移）。
- **NC1**：手工变异后 `cargo test --lib agui_session::tests::legacy` → **EXIT=101**：新钉 FAILED，而
  `legacy_flat_thread_is_migrated_and_becomes_visible` 等 3 个既有 legacy 测试**仍绿**（= MISSED 成因一手复现）；
  还原 + touch 后模块复绿。

### 2. `:264:51` replace match guard `entry.content != *payload` with `true`（`apply_resume_tool_results`）

- **MISSED 根因（无写面盲区）**：guard 只在「payload 已与在盘内容相等」时改变行为——变异把 `changed` 恒置 true →
  多一次 `atomic_write`（temp+rename），**内容逐字节不变**；既有替换测试的 content 必不等，guard 不参与判定。
- **钉**：`resume_replacement_with_identical_content_does_not_rewrite`——`File::set_times`（std 1.75，跨平台）把
  transcript mtime 钉到旧纪元（1e9 s），identical payload 后断言 mtime 不变（无写即可观测面）+ 内容保持；in-test 自检
  （真实不同 payload 后 mtime 必变）防空洞绿。
- **NC2**：手工变异后同测试 → **EXIT=101**，红于 `an already-applied replacement must not rewrite`（mtime
  1793164560 ≠ 钉住的 1000000000）；还原后绿。

### 3. `:342:27` replace `&&` with `||`（`migrate_legacy_dir` 拷贝守卫）

- **MISSED 根因（差异面不可达）**：`from.is_file() && !to.exists()` 的 `||` 变体只在「`to` 已存在且 `from` 是文件」时
  多一次**覆写拷贝**；既有测试首轮迁移时新目录不存在（`to.exists()` 恒 false），幂等重跑段走 `resolve_session_dir` 的
  transcript 存在早退，两处都踩不到差异面。
- **钉**：`migration_never_clobbers_native_companion_files`——native 目录预置自己的 `.interrupts.json` 且无 transcript
  （崩溃于首条 append 前，迁移必被触发），legacy 侧同名文件内容不同；断言 transcript 迁到 + native
  `.interrupts.json` 原样（no-clobber 契约）。
- **NC3**：手工变异后 → **EXIT=101**，红于 `a legacy companion file must not overwrite the native one`
  （left=`[{"legacy":true}]` right=`[{"native":true}]`，覆写实锤）；还原后绿。

## ubuntu-only 自查

3 个新测试全用 std 跨平台面：`FileTimes`/`set_times`（Rust 1.75 起，三 OS 语义一致）、`Path`/`is_file`/
`read_to_string`/`OpenOptions`、`IsolatedWorkspace` tempdir；无 unix-only API、无外部进程（node/git）、无时钟精度依赖
（mtime 只做「等/不等」断言，不涉测量精度）、无 env 触碰。windows/macos/ubuntu CI 均可跑，非 ubuntu-only。

## full 首审余量重分段建议（run 40598348438 口径，本批后）

- `crates/recursive-tui/src/**` 54（modal 19 / input 9 / command_menu 7 / ollama_probe 5 / commands 4 / cost 3 /
  chat 2 / render 2 / runtime_builder 1 / input_state 1 / state 1）——挂账未动
- `src/bin/recursive-eval.rs` 16
- `crates/recursive-cli/src/main.rs` 1（CLI 段批 1-12 报过全清，此为 full `--all` 新浮出残留，建议主控复核口径）
- `src/agui_session.rs` 0（本批清偿）

## 三门（无管道，exit 独立落盘 /tmp/b2-*.log）

- fmt --all → EXIT=0；fmt --all -- --check → EXIT=0
- clippy --workspace --all-targets --all-features -- -D warnings → EXIT=0
- test --workspace → EXIT=0，81 suites，**5475 passed / 0 failed / 12 ignored**（批次 11 基线 5472 恰 +3 = 本批 3 新钉）

## [13:29] 交付行

**结果一句话**：agui_session 3 条全钉死零豁免——3 新测试各对一条变异，负控 3/3 手工变异判死（EXIT=101 且各红在预期
断言；NC1 同时实证既有 legacy 测试在变异下仍绿 = MISSED 成因），还原后 diff 纯增 +162 全在 mod tests；三门无管道绿
（5475/0/12 + clippy --workspace --all-features + fmt）；产品逻辑零改动；无 mutants.toml 改动；非 ubuntu-only。

- **验证**：模块 14/14 绿（11 既有 + 3 新）；NC 日志 `/tmp/b2-nc{1,2,3}.log`，门禁日志 `/tmp/b2-{fmt,fmtcheck,clippy,test-ws}.log`。
- **红线自证**：`git diff -U0 | grep -cE '^-[^-]'` = 0（零删行）；改动文件唯一 `src/agui_session.rs`，首个改动行在
  `#[cfg(test)] mod tests` 内；#222 合规（无新 env 面）；#224 无涉（无 windows 语义触碰）；本地 mutants 未跑（§2b）。
- **未竟/移交**：full 余量 tui 54 / eval bin 16 / cli main.rs 1（见上节重分段建议）；本批终判待 CI push→main 的
  Mutation files-scope run（agui_session.rs）复扫 0 missed。commit 留 wt-b2 待主控验收，未 push。
