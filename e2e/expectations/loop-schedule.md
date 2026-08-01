# Expectation: loop-schedule (tests/17-loop-mode.yaml)

> 套件的「预期行为」契约。accept-fixture.sh 录制完真模型后，spawn 一个 agent-judge
> 读 transcript 对照本文件判定 PASS/FAIL。没配 expectation 的套件退回判完整性。

## suite-id
`loop-schedule`（对应文件 `tests/17-loop-mode.yaml`）

## 场景（loop 模式的地道用法：轮询等待外部条件）

agent 被要求 watch 一个 `ready.flag` 文件，该文件由 setup 的后台定时器（`sleep 8 &&
touch`）延迟创建。这是 loop-supervise 的核心模式——等待外部事件。

## goal（agent 收到的任务）
```
Watch for the file ready.flag in the workspace. On each turn, check if it exists.
If it does NOT exist yet, call schedule_wakeup (delay_secs around 5) to check again
shortly, then end the turn. When ready.flag exists, write multi-loop.txt with the
exact content wakeup-done and do not schedule any further wakeup.
```

## 预期行为（agent-judge 据此判 PASS）

- **早期 turn（flag 不存在时）**：agent 检查 `ready.flag`（用 list_dir / read_file /
  search_files / Glob 任一），发现不存在 → **调用 `schedule_wakeup`**（delay_secs 在
  1-10s 区间，带 reason 和 prompt）安排下一次检查，然后结束该 turn。这是 loop 保活
  的关键——不 schedule 则 loop 退出，任务失败。
- **flag 出现后的 turn**：agent 再次检查，发现 `ready.flag` 存在 → 调用 `write_file`
  写 `multi-loop.txt`，内容为 `wakeup-done`；**不再调 `schedule_wakeup`**，使 loop
  干净退出。
- **session**：最终状态为 `completed`；所有 turn 落在**同一个** session 的 transcript
  里（multi-turn-same-session，Goal 327）。
- transcript 里至少出现一次 `schedule_wakeup`（等待期）和一次 `write_file`（完成期）。

## 合理变体（不算 FAIL）

- agent 可能轮询多次（flag 8s 后才出现，若 agent 选了短 delay 如 2s，可能在 flag
  出现前检查 2-3 次，每次都 schedule_wakeup）。turn 数不固定，只要最终写了文件且
  停止 schedule 即可。
- agent 检查文件用的工具不固定（list_dir / read_file / search_files / Glob 均可）。
- schedule_wakeup 的 delay_secs 只要落在 1-3600 合理区间即可。

## anti-patterns（命中任一即判 FAIL）

- turn 1 看到 flag 不存在，**不调 schedule_wakeup 就结束** → loop 立即退出，
  `multi-loop.txt` 永远不会被写（任务失败）。
- flag 已存在、文件已写完，**仍在 schedule_wakeup** → loop 不会停，无法干净退出。
- `schedule_wakeup` 用了旧参数名（`seconds` / `message` 而非 `delay_secs` / `reason`
  / `prompt`）→ 静默默认 delay_secs=60，loop 睡 60s 撞 setup 超时。
- 写了 `multi-loop.txt` 但内容不是 `wakeup-done`。
- 两 turn 落在不同 session（未满足 same-session 性质）。

## 验收阈值
`completed == true && score >= 4`（accept-fixture.sh 默认门槛）。
