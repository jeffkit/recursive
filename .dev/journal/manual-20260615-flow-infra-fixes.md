# Manual edit: flow-infra-fixes

**Date**: 2026-06-15
**Goal**: 修復 self-improve loop 中反复出现的基础设施坑，杜绝 supervisor 层面的静默失败
**Files touched**:
- `.dev/flows/self-improve.flow.js` — 加入 provider-ping 预检 + 殭尸进程清理
- `.dev/scripts/launch-flow.sh` — 新建健壮启动脚本
- `.gitignore` — 排除 `.flowcast/logs/`
- `~/.flowcast/providers.json` — 更新 GLM 模型 5.1→5.2，新增 glm-5.2 别名

**Tests added**: none（纯工具链改进，无 Rust 代码变更）

---

## 本次 session 踩到的坑与修复

### Pit 1: `&` 背景启动 → SIGHUP 杀死 flow

**现象**：`node self-improve.flow.js ... &` 后 shell 退出，flow 进程收到 SIGHUP，
再把 SIGTERM 传给 recursive 子进程，导致 agent 以 `reason: cancelled` 结束，
transcript 为空，run.log.jsonl 中 `run.recursive done` 永远不写入。

**根因**：非交互式 shell `&` 后台进程不自动 disown，父 shell 退出时子进程组收 SIGHUP。

**修复**：
- `launch-flow.sh` 用 `setsid -f`（macOS fallback `nohup`）启动 Node，
  新进程与原终端彻底断开，免疫 SIGHUP。
- 启动后 2s 存活检查：如果进程立即退出就打印尾日志并以 exit 1 报警。

---

### Pit 2: Provider API 挂死无响应，agent 白等 5min+

**现象**：DeepSeek v4-pro API 当天不可达（可能限流/维护），recursive 在第一次
LLM 调用时阻塞，flow 进程也跟着挂死，没有任何日志，无法判断是 API 问题还是代码问题。

**根因**：flow 没有 provider 健康探测步骤，直接把 goal 塞给 agent，API 超时要等
reqwest 的重试退避（默认可能 >60s）。

**修复**：在 `main()` 里新增 `preflight.provider-ping` 步骤：
- `GET <apiBase>/models` with 12s AbortSignal timeout
- 任何 HTTP 响应（包括 401/404）均视为可达
- 超时或连接拒绝则抛错，flow 立即失败（不浪费后续步骤）

**附：当天 provider 状态**
- DeepSeek v4-pro：API 无响应（挂死）
- GLM-5.2：HTTP 429 余额不足
- MiniMax (MiniMax-M3)：✅ 正常，3084ms 首 token

---

### Pit 3: 殭尸 recursive 进程积累

**现象**：发现 3 个 recursive 进程（PID 34473/40344/40352）分别运行了
25 分钟、4 天以上，CPU 0%，均为旧 worktree run 遗留。其中一个仍占用
release 二进制（影响新 build 判断）。

**根因**：flow 中断/崩溃后 recursive 子进程变成孤儿，父进程死了子进程继续
睡眠等待网络 I/O（LLM API 调用），永远不退出。

**修复**：`main()` 开头调用 `killStaleRecursiveProcs(repo, runId)`，
通过 `pgrep -af recursive` 找到命令行包含仓库路径的 recursive 进程并 SIGKILL，
跳过当前 run 关联的进程和 flow 进程本身。

---

### Pit 4: GLM provider 模型版本过时

**现象**：`~/.flowcast/providers.json` 中 `glm` 指向 `glm-5.1`，
用户已升级到 5.2 并在 `.zshrc` 配置了新 key，但 providers 文件未同步。

**修复**：更新 `glm.model` 为 `glm-5.2`，新增 `glm-5.2` 别名 profile。

---

## 遗留问题

- DeepSeek API 当前不可用原因未知，等待恢复后可恢复为主 provider。
- GLM 余额不足，需要充值后才能作为备用。
- `killStaleRecursiveProcs` 用 `pgrep -af` 匹配 `/recursive ` 路径，
  在部分 macOS 版本 pgrep 输出格式可能不一致，如遇问题可退化为 `ps aux | grep`。
</contents>
</invoke>