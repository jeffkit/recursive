# Manual edit: e2e-coverage-gaps

**Date**: 2026-06-21
**Goal**: 补充 E2E 测试用例，覆盖之前审查报告识别的 Critical/High/Medium 缺口
**Files touched**:
- `e2e/e2e.yaml` — 注册 13 个新 suite
- `e2e/tests/24-bash-tool.yaml` — Bash tool 基本执行、cwd、env、sandbox cwd 拒绝
- `e2e/fixtures/24-bash-tool.json` — Bash tool agent loop fixture
- `e2e/fixtures/24-bash-cwd.json` — Bash cwd 参数 fixture
- `e2e/tests/34-sandbox-security.yaml` — Invariant #3 路径逃逸拒绝（read_file / write_file / Glob / Bash cwd）
- `e2e/tests/35-permissions.yaml` — 权限 deny 配置（via RECURSIVE_HOME config.toml）
- `e2e/tests/25-glob-tool.yaml` — Glob 文件模式匹配
- `e2e/fixtures/25-glob-tool.json` — Glob fixture
- `e2e/tests/26-facts-memory.yaml` — remember / recall 语义记忆
- `e2e/fixtures/26-facts-memory.json` — facts fixture
- `e2e/tests/27-todo-tool.yaml` — TodoWrite 代理任务列表
- `e2e/fixtures/27-todo-tool.json` — todo fixture
- `e2e/tests/28-episodic-recall.yaml` — episodic_recall 跨 session 搜索
- `e2e/fixtures/28-episodic-recall.json` — episodic fixture
- `e2e/tests/29-background-tasks.yaml` — run_background / check_background 异步任务
- `e2e/tests/31-checkpoint-tool.yaml` — checkpoint_list / checkpoint_diff
- `e2e/fixtures/31-checkpoint-tool.json` — checkpoint fixture
- `e2e/tests/33-utility-tools.yaml` — estimate_tokens / count_lines
- `e2e/fixtures/33-utility-tools.json` — utility fixture
- `e2e/tests/36-session-rewind.yaml` — `sessions rewind` CLI
- `e2e/tests/39-http-auth.yaml` — RECURSIVE_HTTP_AUTH_KEYS 401 拒绝
- `.dev/flows/test/e2e.test.js` — 新增 clippy 门和 fmt 门红灯 → rolled-back 两个测试用例

**Tests added**:
- 13 argusAI yaml suites (11 个新文件 + 若干 fixture json)
- 2 个 flow E2E 测试（clippy/fmt gate failure）

**Notes**:
- Sandbox 和 Permissions 测试均使用 MCP stdio 直接调用，无需 mock LLM
- Background tasks 测试用 shell 脚本串接 job_id，避免动态 ID 的 fixture 匹配问题
- HTTP auth 测试启动一个独立 HTTP 服务（port 9097）并 teardown 时关闭
- Session rewind 测试在 dry-run 情况下跳过 checkpoint 不存在的情况（ignoreError）
