# Manual edit: fix-skip-commit-recovery

**Date**: 2026-06-15
**Goal**: 彻底解决 skip-commit 反复需要人工干预的问题
**Files touched**:
- `.dev/flows/self-improve.flow.js`
- `.dev/scripts/launch-flow.sh`（上一个 commit 已包含）

**Problem**:
每次 reviewer provider 因网络/quota 不可用时，flow 返回 `skip-commit`，
质量门全绿的代码改动被搁置，需要人工检查并手动 commit。
这占据了本 session 大量的人工介入时间。

**Root cause**:
flow 对 `reviewer UNAVAILABLE` 采取保守策略：不提交、发通知等待人工。
但质量门（cargo test/clippy/fmt）已经验证了代码可靠性，reviewer 只是可选的二次确认层。

**Changes**:

1. **UNAVAILABLE → auto-commit（主要修复）**
   当 reviewer 多次报错无法响应时，直接 fall-through 到 commit 步骤。
   通知内容从 "待人工复核" 改为 "自动提交"。
   理由：所有质量门通过即说明改动可靠，reviewer 不可用不应导致成果丢失。

2. **`--commit-pending` 快速补提交模式（针对已有 skip-commit 的历史 run）**
   对已发生 skip-commit 的旧 run，可通过以下命令补提交：
   ```bash
   node .dev/flows/self-improve.flow.js \
     --run-id <old-run-id> \
     --goal-file .dev/goals/xxx.md \
     --commit-pending
   ```
   流程：补跑 cargo test + clippy + fmt → 全绿则提交 → 发企微通知。
   不重跑 agent，不等 reviewer，最快几分钟搞定。

3. **`--reviewer-agent claude` 支持（上一个 commit）**
   当 claude CLI 可用时，使用本地 claude 做 review，完全脱离外部 provider 依赖。
   `launch-flow.sh` 自动检测并附加此参数。

**Tests added**: none（flow 逻辑，通过手工验证）
**Notes**:
- `agy` 配置已在 `agents.json` 中，但 `agy -p` 非交互场景挂死（需 IDE daemon），暂无法使用。
- claude CLI 已验证可用：`claude -p "say: ok"` 正常返回。
