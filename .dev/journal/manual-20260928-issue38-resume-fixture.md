# Issue #38 — e2e(resume) 续跑轮 aimock 404 no_fixture_match

- Date: 2026-09-28
- Goal: 修 `sh .dev/scripts/e2e-run.sh resume` 恒红（`Session status "crashed" not in [completed, success]`），验收 passed=2 / failed=0。
- Files touched:
  - e2e/fixtures/11-session-resume.json — turn-1 匹配由 `hasToolResult: true` 改为 `false`（并补注释）。
  - e2e/fixtures/17-loop-mode.json — 同根因另一处：turn-2 wakeup 首调（最新 user 是 wakeup
    prompt）同样应为 `hasToolResult: false`；不改则 entry 1（turnIndex 0）被放松匹配命中，
    wakeup 轮不写文件、loop 无限重排（loop-schedule 红：0/1/2 → 改后 3/0/0 全绿）。
  - e2e/fixtures/README.md — 修正 `hasToolResult` 语义（当前轮，非全历史）。
  - e2e/RECORD_REPLAY.md — 同步修正已知陷阱 #2。
- Tests added: 无新增单测（纯 fixture + 文档，产品代码未改）；验收走 `e2e-run.sh resume`
  （并顺带跑 `loop-schedule`）。
- Notes:
  - 根因：aimock `hasToolResult` 只看「最后一条 user 之后的当前轮」是否含 tool 结果
    （<https://aimock.copilotkit.dev/multi-turn>）。resume 会在 seeded tool result 之后
    追加 `Continue from where you left off.`（user），于是该请求 `hasToolResult=false`。
    请求实测 shape = `turnIndex=1, hasToolResult=false, lastUser="Continue..."`，
    与 fixture 的 `hasToolResult: true` 差一个布尔 → 404。commit dfcd709 只改了
    userMessage 没改这个布尔（当时 container rebuild deferred，未回放验证）。
  - 复现/证据：容器内 strace + 本地 logging proxy 抓取 resume 请求体；probe-aimock
    （自建 fixture）验证 `hasToolResult` = 当前轮是否含 tool。另发现本机 aimock 默认
    **relaxed turnIndex**（`AIMOCK_STRICT_TURN_INDEX=1` 才严格相等，日志有
    `turnIndex relaxed` 提示）：无严格命中时会放宽到最近的脚本 turnIndex，
    这正是 loop-schedule 里 entry 1 被误命中的放大器。
  - 附带核实：`/tmp/rh-resume/workspaces/` 第二个 hash 目录（→ `/workspace`）来自
    `Config::from_env()` 阶段 `episodic_recall_summary(&cwd)`（src/config.rs:597 →
    src/tools/episodic_recall.rs:240 → SessionReader::list_sessions），发生在 CLI
    `--workspace` 覆盖 `config.workspace`（main.rs:549）之前；该目录无 session，
    与 resume 落盘无关。属独立的「--workspace 覆盖前读 cwd」小瑕疵，本 issue 未改。
  - 附带核实：`/tmp/rh-resume/workspaces/` 第二个 hash 目录（→ `/workspace`）来自
    `Config::from_env()` 阶段 `episodic_recall_summary(&cwd)`（src/config.rs:597 →
    src/tools/episodic_recall.rs:240 → SessionReader::list_sessions），发生在 CLI
    `--workspace` 覆盖 `config.workspace`（main.rs:549）之前；该目录无 session，
    与 resume 落盘无关。属独立的「--workspace 覆盖前读 cwd」小瑕疵，本 issue 未改。
