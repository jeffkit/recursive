# manual 20261003 — issue #108 state.json 原子写

## Date
2026-10-03

## Goal
fix(flows): self_improve_bridge_v2.py 的 state.json 观测契约文件读改写为裸
`write_text`——崩溃瞬间留截断 JSON 打炸 supervisor/keeper 的 `json.loads`；
且 `_merge` `except Exception: pass` 静默吞写失败，状态停旧值误导值守。
与同文件 checkpoint.json 的 tmp+rename 原子写（:386-393）形成对照。

## Files touched
- `.dev/flows/self_improve_bridge_v2.py`
  - 新增 `_atomic_write_json(path, obj)`：tmp 掺 pid+uuid + `tmp.replace(path)`
    原子替换（checkpoint `_ckpt_save` 同款，D6；额外掺 uuid，见 Notes 评审轮）。
  - `StepTracker._merge`：改走 `_atomic_write_json`；读侧对
    `ValueError`（含子类 UnicodeDecodeError，半截残留）丢弃重建、本次写自愈；
    `except Exception: pass` → stderr 留一行
    `[state-tracker] state.json merge failed: ...`（flush，nohup 日志可见）。
  - `main()` 初写（status=running）与终态写（verdict/finished_at）两处
    裸 `state_path.write_text` → `_atomic_write_json`。
- `.dev/flows/test/flow_v2_paths.py`
  - 新增 `s28_state_json原子写_永不截断`：原子写基本契约（落盘即合法 JSON、
    无 tmp 残留）/ 读改写保字段 / 截断残留自愈 / 写失败必 log（redirect_stderr
    捕获）/ 同 pid 4 线程并发 merge 无异常无 tmp 互吞（评审反馈 #2 回归锚）/
    源码扫描禁令（state_path 行禁 `write_text(` + 契约函数被引用）。

## Tests added
- `flow_v2_paths.py` s28（场景清单 + SCENARIOS 注册）。
- s28 单场景独立验证通过（含并发段 800 次 merge 零失败零残留）。
- 全 harness 复跑：s8/s18/s21/s24/s28 PASS；其余场景均为 preflight 磁盘守卫
  `retry-later`（本宿主盘余 15.5GiB < min，环境性，与本改动无关——评审人
  独立复跑同样记录 23 例同因失败）。带 git fixture 的 s7 首步同因（守卫先于
  继承提交断言）。
- 附加 kill 注入实证（不入库）：子进程 4s 内高频 `_merge` 200KB state.json，
  主进程边写边 `json.loads` 轮询 ~14k 次：
  - 修复前形态（旧代码）：torn_reads 数百/轮，kill 后目录留 0 字节截断文件；
  - 修复后：torn_reads=0，kill 后 state.json 仍全文可解析（rename 原子）。

## Notes
- 评审轮修订（2026-10-03，NEEDS_FIX 处理）：
  1. **run/ 残留（blocker）**：kill 注入实证用相对路径落在了 worktree 根，
     留 200KB `run/state.json` 未跟踪文件；GIT_PUBLISH 的 `git add -A` 会把它
     带进发布提交。已删 `run/` 并复核 `git status --porcelain` 只含两个目标
     文件 + 本 journal。实证脚本今后一律 tmpfile 绝对路径。
  2. **tmp 掺 uuid（nit）**：pid 独串版同 pid 多线程写同一文件会互吞对方 tmp
     （一方 `replace()` 消费走另一方 tmp → FileNotFoundError，merge 丢失）。
     `_atomic_write_json` tmp 名改掺 `uuid.uuid4().hex[:8]`，s28 增 4 线程
     800-merge 并发段锚定。checkpoint `_ckpt_save`（:405）保持 pid 独串原样
     ——单线程宿主内串行使用，不在本单范围扩散。
  3. **except 元组（nit）**：`UnicodeDecodeError` 是 `ValueError` 子类，
     收敛为单 `except ValueError` 并注释子类覆盖。
- 初版 `_atomic_write_json` 用 `with_suffix`（剥掉 `.json` 再拼 `.tmp-<pid>`），
  写失败路径的报错文件名成了 `state.tmp-<pid>`；行为无碍（同目录同 inode 语义）
  但名字有误导，保留 checkpoint 同款写法，未过度设计。
- 崩溃自愈语义：若新进程首见半截 state.json（旧版崩溃遗留），丢弃重建从
  `{"status":"running"}` 起步——currentStep/node_timings 丢一轮属可接受
  （观测文件，非恢复数据；恢复数据在 per-issue checkpoint.json，其原子性
  早有保证）。
- 同目录其他裸 write_text（nodes-dump / engine-error.log / reply.md /
  worktree-preserved.log / reply-error.log / recovery-error.log）均为
  run_dir 内诊断/旁路工件，非值守轮询契约，不在本单范围。
