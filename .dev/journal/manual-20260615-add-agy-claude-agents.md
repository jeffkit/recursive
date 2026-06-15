# Manual edit: add-agy-claude-agents

**Date**: 2026-06-15
**Goal**: 将 agy CLI 和 claude CLI 作为 agent 加入 flowcast agent 池
**Files touched**:
- `~/.flowcast/agents.json`（机器级，新增 `agy`、`claude`）
- `.flowcast/agents.json`（项目级，新建；含 `agy`、`claude`、`claude-minimax`、`claude-deepseek`、`recursive-deepseek`、`recursive-minimax`）

**Tests added**: none（配置文件变更，通过 `loadAgents` + `resolveAgent` dry-run 验证）

**Notes**:
- `agy` 执行器已内置于 flowcast（executor.js），自带鉴权，不接受外部 provider；extraArgs 加 `--dangerously-skip-permissions` 用于批量非交互场景。
- `claude` 执行器支持 `applyProvider`（ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN），可绑定 anthropic-minimax / anthropic-deepseek 网关；不绑定时使用 claude 自身环境配置。
- 项目级 agents.json 覆盖机器级同名 key；机器级保留 `cursor-default` 作为全局通用入口。
- 配置已通过 `node --input-type=module` 脚本验证：7 个 agent 全部正确解析。
