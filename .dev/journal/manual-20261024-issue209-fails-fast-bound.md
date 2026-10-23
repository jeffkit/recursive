# 2026-10-24 — #209 修复:test(mcp) http_initialize_fails_fast 负载敏感假红(时长上界 2s→3s + 负控)

**[07:49] 交付行(wt-b1 席)**:单 commit 落 recursive-b1,于基线 10fa0d2e
(= origin/main)之上,未 push 未动 main,等主控验收(sha 见台账)。

## Date

2026-10-24(wt-b1 席)

## Goal

#209(P2):`mcp::tests::http_initialize_fails_fast_and_ignores_the_tool_budget`
全量 suite 并行负载下间歇假红("failed after 2.745s"),单跑稳定 0.31s。
主控拍板:放宽时长预算到 3s、不改断言语义、负控证明预算非摆设。

**基线更正(验收须知)**:主控口径「900ms → 3s」中的 900ms 在基线 10fa0d2e
上并非断言值——7267249d(goal 415-4 偏航产物)已把该测试改成 mock 延迟 3s /
tool_timeout 10s / **上界 2s**;900ms 是旧注释里「300ms 定时器负载下抖动观测值
~900ms」。实测假红点 2.745s 撞的是 2s 上界。按拍板值落 **3s**,同样在 2.745s
之上留余量,方向与幅度均符合裁决意图。

## 根因

全量 suite 并行跑时,300ms 读超时的 tokio 定时器 waker 被调度延迟,错误最迟
2.745s 才返回,超过上界断言 `elapsed < 2s` → 假红。判别逻辑本身仍然成立
(mock 3s 才应答 Ok,任何「等响应」型回归都会变 Ok 撞首断言),红的只是
抖动吸收器设得太紧。

## Files touched

- `src/mcp.rs`(仅 `mod tests`,+53/-11,产品逻辑零改动):
  1. 新增共享常量 `INITIALIZE_FAILS_FAST_BOUND = 3s`,doc comment 写明语义
     不变量(「initialize 失败远快于 tool budget 且不消费预算」)、上界只是
     抖动吸收器非契约、放宽理由(满载下 300ms 定时器 2.745s 才触发,2s 假红,
     引 #209)。
  2. `http_initialize_fails_fast_and_ignores_the_tool_budget`:上界
     `Duration::from_secs(2)` → `INITIALIZE_FAILS_FAST_BOUND`;**首断言
     「ignores the tool budget」(含消息)字节不动**——语义钉;mock 延迟
     3s 保持不变,与上界重合的设计写进 doc comment(上界内的错误必然落在
     mock 唯一应答之前,只能来自 read-budget tier)。
  3. 新增负控 `http_initialize_slower_than_the_fails_fast_bound_is_flagged`:
     mock 注入 8s 慢初始化(>共享上界)+ 读预算故意放宽到 4s(模拟不再
     fails-fast 的客户端);断言 `is_err()` 且 `elapsed >= 上界`。timeout
     不可能提前触发,故错误必然落在 ≥3s(实测 4.009s)——确定性判别,证明
     放宽后的上界仍能在新预算上抓真慢(预算不是摆设);若该测试 3s 内完成,
     说明 mock 没延迟或读预算没绑定,上界即失效。

## Tests added

- `http_initialize_slower_than_the_fails_fast_bound_is_flagged`(负控,
  全量三遍均绿)。
- 时序证据:单跑主测试 **307.5ms**(与 issue 单跑基线 0.31s 一致,零回退);
  负控 **4.009s** 被拒。

## 三门与稳定性

- `cargo test --workspace` **×3 全 EXIT=0**,各 5250 passed / 0 failed
  (基线 5249 + 新增 1 条;原红点三连绿即证)。
- `cargo clippy --all-targets --all-features -- -D warnings` EXIT=0。
- `cargo fmt --all -- --check` EXIT=0。
- 本地 cargo-mutants 未跑(§2b)。

## Notes

- **env 审计(#222/#226 口径,主控第 4 点)**:该测试与 HTTP 三兄弟
  (`server_with_url` + `spawn_with_timeout` 路径)经 `default_tool_timeout()`
  裸读 `RECURSIVE_MCP_TOOL_TIMEOUT_SECS`;唯一写者是
  `default_tool_timeout_env_override_and_fallback`(无锁 set/remove)。
  结论:**无需 EnvGuard**——①initialize 走 read_timeout 不走 tool_timeout,
  这正是被测不变量,env 值无论 42s/7s/默认 300s 都不改变本测试结果;
  ②三个 HTTP 时序测试在 Ok 路径都显式 `with_tool_timeout` 覆盖,env 派生值
  被丢弃;③断言 tool_timeout 字段的测试全部经 `idle_streamable_client`
  直构,不经 env。即:无「结果依赖 env 值」的读侧,写侧自包含,无跨测试
  竞态可翻红。全局 state 方面,mock 每次绑 127.0.0.1:0 临时端口、线程
  detached,与兄弟测试无冲突。
- GitNexus 本会话不可用,impact 审查以 `git diff` 全量人工替代:改动全部
  落在 `src/mcp.rs` 的 tests 模块内,不触产品逻辑。
- 残余风险(留观察项):上界 3s 与观测最坏 2.745s 余量 ~9%。若未来满载
  下再红,下一档杠杆是把 mock 延迟与上界一起上移(保持「上界 ≤ mock 延迟」
  的判别关系),而不是回到收紧断言。
