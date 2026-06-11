# Goal: 让 self-review 产出可靠的结构化 verdict

给 `.dev/flows/self-improve.flow.js` 的跨 provider self-review 步骤加上「限步数 +
强制结构化 verdict 提取」，解决真实 parity 跑出来的问题：agentic reviewer 给了工具
就会跑偏去探索仓库 / 跑测试，结束时（finishReason=no_more_tool_calls）经常**不输出**
`VERDICT:PASS` / `VERDICT:NEEDS_FIX` 行，导致质量门全绿的好改动被保守判否、误回滚。

这正是 recursive 自己用 `review-changes.sh` 输出 JSON verdict 的原因——review 必须
被强约束成「只给裁决」，而不是放任成一个自由 agent。

## 背景与现状

- 当前 `selfReview()` 把 diff 内联进 prompt，要求最后一行输出 VERDICT，但 reviewer 是
  agentic 循环、可自由调工具，常常不按要求收尾。
- `reviewWithRetry()` 已有三态（PASS / NEEDS_FIX / UNAVAILABLE）：
  - reviewer 调用出错（网络/退出码）→ 重试，多次失败 → UNAVAILABLE（不丢弃成果）。
  - reviewer 正常返回但无 verdict token → 当前**保守判 NEEDS_FIX**（即误回滚来源）。

## Requirements

1. **限步数**：给 review 的 recursive 调用传一个低 `--max-steps`（默认 8，可配），
   避免 reviewer 跑偏做大量探索 / 跑测试。
2. **强制结构化 verdict 提取**：当 reviewer 正常结束但输出里没有
   `VERDICT:PASS` / `VERDICT:NEEDS_FIX` 时，**不要**直接判 NEEDS_FIX；改为在同一
   transcript 上 `replay --resume-from N` 追问一次：
   「Output ONLY one line, exactly `VERDICT:PASS` or `VERDICT:NEEDS_FIX`. No other text.」
   解析这次追问的输出。
3. **判级**：
   - 追问后拿到明确 verdict → 按其裁决。
   - 追问后仍无 verdict（或调用出错）→ 归为 `UNAVAILABLE`（保留成果 + HITL 升级），
     不得静默回滚一个质量门全绿的改动。
4. **可配置**：review 的 max-steps 和是否启用强制提取，走 flow 的 CLI flag
   或 opts，默认开启。

## 非目标

- 不引入完整的 JSON-schema reviewer（review-changes.sh 那套静态不变量检查留后续）。
- 不改 recursive 的 Rust kernel。

## Definition of done

- `.dev/flows/self-improve.flow.js` 的 review 路径实现上述「限步数 + 强制 verdict 提取」。
- 新增/更新单测：模拟 reviewer「无 verdict 输出」时，flow 会走强制提取而非直接判否；
  模拟提取后仍无 verdict 时归为 UNAVAILABLE（保留成果）。
- `npm test` 全绿。
- 在 recursive 仓用一个 easy goal 跑通一次 `review on` 的真实运行，verdict 为 committed
  （而非因 reviewer 不吐 verdict 被误回滚）。

最后写一段 summary，列出改动文件与测试结果。
