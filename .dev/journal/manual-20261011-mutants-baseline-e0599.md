# manual-20261011 — 变异门「幸存者」实为 main 编译失败（tests/issue89_parent_environ.rs E0599）

- **Date**: 2026-10-11 (Asia/Shanghai)
- **Goal**: issue #195（自动建档：push→main 落地门检出变异测试幸存）。复跑发现
  该单的 14:42 评论对应的 run **没有任何幸存 mutant**——门在「未变异基线」阶段
  就挂了；根因是 main 在 Linux 上编译不过。

## 根因

eef5f42b 从 salvage 分支带进 `tests/issue89_parent_environ.rs`（#149 单的增量），
该文件调用 `RunShell::new(..).execute(..)`（`execute` 是 `Tool` trait 的方法），
却只 `use recursive::tools::shell::RunShell;`，漏了 `use recursive::Tool;`。

- 该文件 `#![cfg(target_os = "linux")]`，所以 **Linux 全站红、Windows 不受影响**：
  - `cargo test` / `cargo clippy --workspace --all-targets` 编译该 test target 时
    E0599（CI run 38101765375 的 `test (ubuntu-latest)` 即栽在 clippy 步）。
  - mutants 落地门同理：`cargo-mutants` 的 `cargo test --no-run` 基线失败 →
    `ERROR cargo build failed in an unmutated tree, so no mutants were tested`
    （run 38148115356，agent crate）；而 mutants.yml 把任何非零退出都记成
    `mutants survivors in:` 并开「变异测试幸存」单 → 假信号。

## Files touched

- `tests/issue89_parent_environ.rs`（+1 行：`use recursive::Tool;`）

## 验证

- `cargo test --no-run -p recursive-agent --features test-utils,anthropic,http,mcp,web_fetch,web_search,skill-hub,acp,coordinator-mode,otel`
  —— 修复前 EXIT=101（复现 E0599），修复后 EXIT=0。
- 该 featureset 是 `agent-mutants.sh` 的 `FEATURES`，即 cargo-mutants 基线的原样复刻。
- `cargo test -p recursive-agent --features <同上> --test issue89_parent_environ`
  → `1 passed`（不只是能编译，用例本身也过）。
- `cargo fmt --all` 干净。

## Notes

- eef5f42b run（38101765315）的 **cli** crate 基线正常，真出过幸存者：
  `192 mutants tested in 28m: 18 missed, 170 caught, 4 unviable`，全部在
  `crates/recursive-cli/src/main.rs`。`crates/recursive-cli/` 在
  eef5f42b..e7ff337a 之间零改动，故该清单仍有效，已随 issue #195 评论逐条列出。
- 其中 3 条疑似**等价 mutant**（改测试杀不掉，应走 `.cargo/mutants.toml`
  `exclude_re` + 注明理由，参照既有 `render_empty_state` 先例）：
  `replace < with <= in tail_entries`(2406)、`... in tail_messages`(2417)、
  `replace match guard !raw.trim().is_empty() with true in plan_approval_timeout_secs`(3340)。
  等价性需以实跑 `cli-mutants.sh` 确认后再 exclude。
- 本次未改 mutants.yml / *-mutants.sh 的退出码分流——「基线失败 ≠ 幸存者」的
  分诊建议已写入 issue #195 评论，留给后续 goal（本单验收范围是 agent 基线恢复）。
