# Manual edit: e2e-new-suites

**Date**: 2026-06-21
**Goal**: 新增并验证 12 个 E2E 测试套件，补全 ArgusAI 覆盖率，覆盖此前完全没有测试的 P0/P1/P2 功能区域
**Files touched**:
- `e2e/e2e.yaml` — 注册 12 个新 suite；移除了冲突的 9097:9097 端口映射
- `e2e/tests/24-bash-tool.yaml` — Bash 工具（run_shell）
- `e2e/tests/25-glob-tool.yaml` — Glob 工具
- `e2e/tests/26-facts-memory.yaml` — 语义 Facts 内存（remember/recall）
- `e2e/tests/27-todo-tool.yaml` — TodoWrite 工具
- `e2e/tests/28-episodic-recall.yaml` — 跨 session 转录搜索
- `e2e/tests/29-background-tasks.yaml` — run_background/check_background（重写为 agent-loop 方式，因 job manager 是内存态）
- `e2e/tests/31-checkpoint-tool.yaml` — checkpoint_list（deferred）；MCP deferred 工具不在 serve 列表的断言
- `e2e/tests/33-utility-tools.yaml` — estimate_tokens/count_lines（count_lines 是 agent-loop only）
- `e2e/tests/34-sandbox-security.yaml` — 沙箱路径逃逸拒绝（Invariant #3）
- `e2e/tests/35-permissions.yaml` — 权限系统 deny/allow
- `e2e/tests/36-session-rewind.yaml` — sessions rewind CLI（容器无 git，仅测 no-checkpoints 路径 + sessions list）
- `e2e/tests/39-http-auth.yaml` — HTTP API 认证（X-API-Key，容器内 curl 方式，回退到与 08-http-api 相同模式）
- `e2e/fixtures/28-episodic-recall.json` — 修正 fixture match key 为 `episodic-search-task`，避免与 01-basic-tools 冲突
- `e2e/fixtures/29-background-tasks.json` — 新建 agent-loop fixture（利用 bg-1 固定 job ID 断言）

**Tests added**: 12 个新 ArgusAI suite，合计新增 ~55 个测试 case；12 个全部通过

**Key decisions & lessons**:
1. `run_background`/`check_background` job manager 是内存态，需要在同一 agent loop session 里完成两步；不能用两次 MCP serve 调用。
2. `checkpoint_list`, `checkpoint_diff` 是 deferred 工具，只在 agent loop 暴露，MCP serve 不列出。改为测"correctly-deferred"不变量。
3. `count_lines` 只注册在 `cli/builder.rs`，不在 `tools/registry.rs`，因此 MCP serve 不可见。改为测"agent-loop only"不变量。
4. `sessions rewind` 要求 `checkpoints.jsonl`，而容器内没有 git，所以无法进行完整 rewind。改为测 no-checkpoints 错误路径 + `sessions list`。
5. ArgusAI `request:` 是从宿主机发 HTTP，无法访问容器内临时 HTTP server。HTTP server 测试（auth、api）均使用 `exec: curl`（容器内），与 08-http-api 保持一致。
6. fixture 的 `userMessage` 匹配词需要在所有 fixture 中唯一，否则会被其他 fixture 抢先匹配（episodic-recall 教训：任务里含 "hello.txt" 被 01-basic-tools 抢匹配）。
7. `facts.jsonl` 存在 `<workspace>/.recursive/memory/` 里，不在 RECURSIVE_HOME 里。

**Notes**:
- 全量跑 37 套件：92 passed，14 failed（均为 pre-existing 旧 suite 故障，与本次无关）
- 旧 suite 失败原因：session path 查找使用了旧的 `.recursive/sessions` 路径（未用 RECURSIVE_HOME 隔离），以及工具名称变更（`read_file` → `Read`）等；这些是独立问题，不在本次修改范围内
