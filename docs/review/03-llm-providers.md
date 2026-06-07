# Review: LLM 提供者层 & 多 Agent 系统

**Date**: 2026-06-06
**Reviewer**: Architecture Critic (AI)
**Scope**: src/llm/, src/providers.rs, src/multi.rs, src/compact.rs, src/cost.rs

---

## Executive Summary

整体质量较高。trait 设计清晰，retry 策略统一，错误信息包含模型名和 HTTP 状态码，测试覆盖率充分（每个公开接口都有集成测试）。主要问题集中在以下四个方向：(1) 遗留文件未清理；(2) SSE 流式解析存在可观察但可复现的 UTF-8 边界 bug；(3) 搜索评分有一处逻辑错误导致某些 term 永远触发 fallback；(4) 多 Agent 层没有并发执行和取消机制，与其文档描述的用途不符。

---

## 严重问题 (Critical)

### C1 — `openai.rs.bak` 是版本控制中的活体地雷

**位置**: `/Users/kongjie/projects/Recursive/src/llm/openai.rs.bak`

**问题**: 该文件是旧版 `openai.rs` 的完整拷贝（约 1040 行），包含：
- `OpenAiProvider::new()` 签名返回裸 `Self`（无 `Result`），而当前版本返回 `Result<Self>`
- 内部 `parse_sse_stream` 丢弃全部 tool_calls（`let tool_calls: Vec<ToolCall> = Vec::new();` 第 340 行，之后从不填充）
- `serialize_message` 对所有角色都发送 `reasoning_content`（第 480 行），未限制在 `Role::Assistant`

这三个 bug 在当前 `.rs` 文件中都已修复，但 `.bak` 文件仍在 repo 里。它不参与编译，但：
- 读代码的人（包括未来的自我改进 loop）会混淆哪个是真实实现
- 如果 `.bak` 被意外 include 或自动化脚本扫描，会引入回归

**建议**: 立即 `git rm src/llm/openai.rs.bak`。如果需要版本回溯，git 历史已经有这个版本。

---

## 中等问题 (Major)

### M1 — SSE 流式解析用 `from_utf8_lossy` 跨 chunk 边界会截断多字节字符

**位置**: `src/llm/openai.rs` 第 609–628 行，`parse_sse_stream` 方法

```rust
let text = String::from_utf8_lossy(&bytes);
for ch in text.chars() {
    if ch == '\n' { ... }
    else if ch != '\r' { line_buf.push(ch); }
}
```

**问题**: `bytes_stream` 的每个 chunk 是任意字节边界切割的，UTF-8 多字节序列（日文、中文等）完全可能被分割在两个 chunk 之间。`from_utf8_lossy` 会把残缺的尾字节替换为 U+FFFD，导致该字符永久丢失，且无任何错误提示。结果是模型输出在生产日志和最终 `Completion.content` 中静默损坏。

**注意**: `anthropic.rs` 的 `parse_sse_stream`（第 664 行）使用 `resp.text().await?` 整体读取，不存在此问题。只有 OpenAI SSE 路径受影响，但该路径覆盖了 DeepSeek、MiniMax、Moonshot、Ollama 等大量提供商。

**建议**: 用 `bytes::BytesMut` 或 `std::io::BufReader` 维护跨 chunk 的字节缓冲区，仅在完整 UTF-8 序列齐全时才 decode，或使用 `encoding_rs` 的流式解码器。最简单的修法：改为和 anthropic 一样先 `resp.bytes().await` 整体收集再处理，牺牲首字节延迟换正确性（对工具调用不在乎流式延迟）。

---

### M2 — search.rs 评分 fallback 逻辑有 off-by-bug：多 term 查询时 fallback 永远不触发

**位置**: `src/llm/search.rs` 第 182–184 行

```rust
if score == 0 && parsed.full.contains(term.as_str()) {
    score += weights::NAME_FALLBACK;
}
```

**问题**: `score` 是一个累计变量，在 `for term in &scoring_terms` 的外层初始化为 `0`，然后在循环内逐 term 累加。当查询包含多个 term 时，第二个 term 开始，`score` 已经 > 0（由第一个 term 贡献），即使第二个 term 既没有 name part 命中也没有 hint/description 命中，`score == 0` 条件仍为 false，fallback 不触发。

结果：对于多 term 查询，"name 子串兜底"（`NAME_FALLBACK = 3`）实际上只对第一个 term 生效。单 term 查询则正常。这个权重注释里写"only when nothing else has scored yet for this term"，但代码实现的是"only when nothing has scored for all previous terms"。

**建议**: 将 fallback 逻辑改为每个 term 独立的局部 score 计数：

```rust
for term in &scoring_terms {
    let mut term_score: i32 = 0;
    if parsed.parts.iter().any(|p| p == term) { term_score += ...; }
    else if parsed.parts.iter().any(|p| p.contains(term.as_str())) { term_score += ...; }
    if term_score == 0 && parsed.full.contains(term.as_str()) { term_score += NAME_FALLBACK; }
    if !hint_lower.is_empty() && word_boundary_match(term, &hint_lower) { term_score += SEARCH_HINT; }
    if word_boundary_match(term, &desc_lower) { term_score += DESCRIPTION; }
    score += term_score;
}
```

---

### M3 — `providers.rs` 中的 `expect()` 在 TOML 损坏时会 panic 生产进程

**位置**: `src/providers.rs` 第 54 行

```rust
.expect("providers.toml is bundled at compile time and must be valid")
```

**问题**: 这是在 `OnceLock::get_or_init` 闭包内的 `expect`，在非 test 生产代码路径中执行。虽然 `providers.toml` 是编译时嵌入的静态字符串，TOML 格式错误会在 `cargo build` 阶段被 `toml::from_str` 解析——但只有在第一次调用 `all_presets()` 时才会实际解析，不是编译时。如果有人修改了 `providers.toml` 格式，二进制仍然能构建，但 runtime 首次调用时进程会 panic。

更严重的是：`all_presets()` 被 `context_window_tokens_for_model` 和 `pricing_for` 调用，这两个函数在 agent 运行时的 hot path 上。

**建议**: 改为在 binary 启动时做一次显式校验（main 或 Config::from_env），并返回有意义的错误；或在 build.rs 中加一个 `cargo:rerun-if-changed=providers.toml` + 编译时解析检查。

---

### M4 — multi.rs 多 Agent 调度是串行的，`TeamOrchestrator` 名字语义误导

**位置**: `src/multi.rs` 第 451–519 行（`TeamOrchestrator::run`），第 363–387 行（`Pipeline::execute`）

**问题**: 文档注释（第 579–588 行，coordinator_system_prompt）明确描述了 `spawn_workers_parallel` 可以并发运行 worker。但实际 `TeamOrchestrator::run` 的 delegation 执行是顺序的 `for (role, task) in &delegations { ... pool.run_with_role(...).await? ... }`。`Pipeline::execute` 同理，每个 stage 都是 `.await` 顺序阻塞。

如果 LLM 调用 P95 延迟是 5s，一个有 3 个并行 delegation 的任务实际等待 15s 而不是 5s。这对于号称"多 Agent 并发"的系统是根本性的功能缺失。

同时，没有任何取消机制（无 `CancellationToken`，无 `tokio::select!`）。如果一个子 Agent 挂死，整个 orchestration 永久阻塞。子 Agent 的 panic 会被 `?` 展开为 `Err`，不会传播为 panic，这点是正确的。

**建议**: 对独立的 delegations 改用 `tokio::try_join!` 或 `futures::join_all`；为 `run_with_role` 添加 `tokio_util::CancellationToken` 参数，在超时时可主动取消。

---

### M5 — `compact.rs` 压缩时的 `_older_chars` 死变量：信息记录不完整

**位置**: `src/compact.rs` 第 286 行

```rust
let _older_chars: usize = older.iter().map(|m| m.content.len()).sum();
```

**问题**: 前导下划线表示"已知未使用"，但这个值（压缩前的字符数）正是 on-call 工程师在诊断"上下文为何压缩""压缩了多少"时最需要的。header（第 290–293 行）只记录了消息数量和压缩后 summary 大小，没有记录"从多少字符压到多少字符"。

同时 `estimate_chars` 的实现（第 55–67 行）包含 tool_calls 和 reasoning_content，但 `_older_chars` 的计算仅用了 `m.content.len()`，两者不一致。

**建议**: 将 `_older_chars` 改为使用 `Compactor::estimate_chars(older)` 并写入 header，方便调试。

---

## 轻微问题 (Minor)

### N1 — cost.rs `update_meta_with_cost` 对 `cost_usd = None` 时写入 0.0，语义错误

**位置**: `src/cost.rs` 第 167–171 行

```rust
serde_json::Number::from_f64(self.cost_usd().unwrap_or(0.0))
    .unwrap_or(serde_json::Number::from_f64(0.0).unwrap()),
```

**问题**: 当 `cost_usd()` 返回 `None`（未知模型），写入 meta.json 的是 `0.0`，这会让外部工具误认为本次运行免费。`cost.json` 中 `cost_usd` 字段正确地保留了 `null`（`Option<f64>` 序列化），但 `.meta.json` 中被覆盖为 `0`。

**建议**: 只在 `cost_usd().is_some()` 时才插入该字段，否则跳过，保持 meta 中没有该 key。

---

### N2 — `LlmProvider` trait 缺少 capability negotiation，`complete_structured` 默认返回 Error 不符合 Rust trait 惯例

**位置**: `src/llm/mod.rs` 第 256–260 行

```rust
async fn complete_structured(&self, _req: StructuredRequest) -> Result<Value> {
    Err(Error::Config { message: "provider does not support structured output".into() })
}
```

**问题**: 这是一个合理的设计，但调用方必须通过 try-and-catch-error 来发现 provider 是否支持 structured output，而不是通过 capability flag。这违反了"让非法状态不可表示"的原则。当新增 `vision`、`thinking`、`audio` 能力时，每次都需要在 trait 中新增一个默认返回错误的方法，长期看会让 trait 膨胀。

**建议**: 考虑添加一个 `fn capabilities(&self) -> ProviderCapabilities` 方法返回 bitflag 或 capability 结构体，让调用方在调用前 probe，而不是通过错误发现。这不是紧急问题，但现在 trait 有 6 个方法，是做这个决策的合适时机。

---

### N3 — retry 策略默认 `max_retries = 2` 对 rate limit 太保守，`Retry-After` header 被忽略

**位置**: `src/llm/mod.rs` 第 49–55 行（RetryPolicy default），`src/llm/anthropic.rs` 第 289–303 行，`src/llm/openai.rs` 第 174–188 行

**问题**: HTTP 429 按固定指数退避（最大 8s）重试，没有读取 `Retry-After` header。Anthropic 在 429 响应里明确给出要等多少秒，忽略它会导致在 server 推荐的等待时间前就重试（触发再次 429），或者等得太久（server 已经 ready 但我们还在睡眠）。

**建议**: 在 429 响应时解析 `Retry-After` header，用其值替换计算出的 backoff。

---

### N4 — `mock.rs` 中对 Mutex 的 `unwrap()` 在 poison 时会二次 panic

**位置**: `src/llm/mock.rs` 第 72、79、91–92、114 行

```rust
self.calls.lock().unwrap().push(messages.to_vec());
```

**问题**: MockProvider 在 `#[cfg(feature = "test-utils")]` 下对外暴露，如果某个测试 panic 了导致 Mutex 中毒，后续调用 `lock().unwrap()` 会再次 panic，掩盖原始失败信息。这在 test 代码里是可以接受的，但如果 `test-utils` feature 在某些 integration 路径中启用，会有问题。

**建议**: 用 `.lock().unwrap_or_else(|e| e.into_inner())` 或在 test helper 中 document "poison 不是预期行为"。

---

### N5 — `bak` 文件的 `serialize_message` 对所有 role 发送 `reasoning_content`，已修复但未 remove

**位置**: `src/llm/openai.rs.bak` 第 479–481 行（与 C1 合并，可一并删除）

这与 C1 重复，仅作记录确认 `.bak` 中存在的具体 bug。

---

### N6 — `MessageBus` 历史无界增长，没有 TTL 或容量上限

**位置**: `src/multi.rs` 第 141–208 行

**问题**: `MessageBus.messages` 是一个无界 `Vec`，永不清理（除非手动 `clear()`）。在长期运行的 agent pool 中，消息历史会无限增长，最终成为内存泄漏。对于自我改进循环这类会持续运行数十轮的场景，这会积累大量历史。

**建议**: 加一个可配置的 `max_history: Option<usize>`，或在 `send` 时按 TTL 清理。

---

## 正面评价

1. **RetryPolicy 统一且共享**。`mod.rs` 中定义一次，两个 provider 都复用，行为一致，`backoff_for` 是纯函数，易于单元测试——且确实有测试（`policy_retries_5xx_with_exponential_backoff`、`policy_does_not_retry_4xx` 等）。

2. **错误信息包含模型名**。`make_err` 在两个 provider 中都把 `self.model` 注入到 `Error::Llm.provider`，生产日志里能直接定位是哪个模型出了问题，符合 on-call 工程师的需求。

3. **SSE 解析测试覆盖扎实**。`test_e_stream_text_deltas_accumulate`、`test_f_stream_tool_use_assembles_tool_calls`、`test_g_stream_tx_receives_text_chunks` 等都是真实 TCP server 测试，不是 mock，能覆盖真实协议行为。

4. **`compact.rs` 的 fallback 链设计得当**。structured output 失败 → free-text → 报错，每个降级都有 tracing log，且有三个独立测试验证每条路径。

5. **`providers.toml` 集中维护**，`pricing_for` 和 `context_window_tokens_for_model` 都走同一个数据源，没有硬编码散落各处。

6. **多 Agent 消息总线设计干净**。`SharedMemory` + `MessageBus` 职责分离，pub/sub 和 history inbox 都测试过。

7. **`filter_leading_assistant` 防御了 Anthropic API 的"首条消息必须是 user"约束**，且只过滤无 tool_calls 的 assistant 消息，不会把正常的 tool-call 开头截断。

---

## 建议优先级

| 优先级 | 条目 | 原因 |
|--------|------|------|
| P0（立即） | C1 — 删除 `openai.rs.bak` | 活体混淆源 |
| P1（本 sprint） | M1 — SSE UTF-8 边界 | 生产静默数据损坏，覆盖所有 OpenAI-compatible 提供商 |
| P1（本 sprint） | M2 — search.rs scoring fallback bug | 多 term 查询时 fallback 权重从不生效，影响 ToolSearch 准确率 |
| P2（下 sprint） | M3 — providers.rs expect() | 可在 runtime 首次调用时 panic |
| P2（下 sprint） | M4 — 多 Agent 串行执行 | 与 coordinator prompt 的并发承诺不符 |
| P3（backlog） | M5、N1–N6 | 质量改进，不影响正确性 |

---

## 如果只能改一件事

如果只能改一件事，应该是 **M1（openai.rs SSE 流式解析的 UTF-8 跨 chunk 截断问题）**。这是唯一在生产中会静默损坏用户可见数据的 bug，影响范围覆盖所有 OpenAI 协议提供商（DeepSeek、MiniMax、Moonshot、Ollama 等），且损坏不会触发任何错误——流式调用正常返回 `Ok(Completion)`，只是中文/日文字符被替换为 U+FFFD，在模型输出包含多字节字符时必然复现。其他所有问题要么是代码卫生问题（C1），要么影响的是特定查询路径（M2），要么是未实现的功能（M4），不是静默数据损坏。
