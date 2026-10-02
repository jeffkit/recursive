# DESIGN: v2 本地分布式宿主（v3）——节点级断点续跑，不经 console

Status: DRAFT v2.3（2026-10-02 Track R1 收尾修订：main() 终态回收按 checkpoint
      活性豁免 / 宿主终态补发 on_flow_end / dry-run 开放进 v3 / D5 关闭；
      增量修订处均带「v2.2」「v2.3」标记，其余原文不动）
Date: 2026-10-01（v2.2 修订 2026-10-02；v2.3 修订 2026-10-02）
Author: 值守 agent
评审: 架构×1 / 重试语义×1 / 运维契约×1 / 测试策略×1（全部 APPROVE-WITH-CHANGES，
      本版已吸收全部 P0/P1；意见原文与编号见文末 §11）
关联: issue-keeper `docs/DESIGN-console-execution.md`（L3/console 路线，本文是其本地替代方案）

---

## 0. 一句话

给 v2 换一个**本地宿主循环**：用 plaita 现成的 DISTRIBUTED 策略逐节点驱动，
checkpoint 落 keeper 工件根（跨派发可续）；节点异常自动重试（v2.1 修订：仅
异常类失败，业务红不在此列——§3）；Langfuse 全链观测接上。keeper 派发链
仅增一个 `engine_env` 透传字段（照 agent/reviewer 先例），reaper 语义不动。

## 1. 背景与动机

- v2 现状：bridge 内 `FlowExecution.execute()`（NORMAL 策略）一口气跑完，
  节点间无断点——图重走时 impl 前段已完成的门禁/评审段全部重付。
- L1（wip 分支代码续跑）与 L2（`recursive resume <sid>` 会话续跑）已落地，
  但只覆盖**节点内**损失；**节点间**的重付没有解。
- plaita 的 DISTRIBUTED 策略（`core/strategies.py:208`）是现成的。2026-10-01
  最小实验（`test/dist_demo.py`，plaita 912f7df 顺带修了 codeflow EVENT 的
  eventType 别名 bug）实证：逐节点推进 + 进程崩溃后从落盘 checkpoint 恢复 +
  事件挂起/恢复，全部工作，零 plaita 核心改动。

### 为什么不是 console（L3 路线降级为可选）

v2 的图里**一个 EventNode 都没有**（不需要事件挂起）；console 对 v2 的独有
价值（UI、事件挂起、executions API）用不上，而 v2 需要的 G1/G3 它也没有
（P0 验收实证）。本地宿主绕开 worker 四件套直接拿节点级断点。

## 2. 架构：bridge 进程内的宿主循环

替换 `self_improve_bridge_v2.py` 的 `ex.clean(); ex.execute(fl, params)`。
**v2.1 修订要点**（吸收 D1/T5/O7 的 params 遗漏、D6 的并发防护、R1/D2 的
终态 checkpoint 陷阱、D3/R2/T2 的重试计数、D4 的超时类失败）：

```python
CKPT = ISSUE_ROOT / "checkpoint.json"        # ← keeper 工件根（§4），非 run_dir
LOCK = ISSUE_ROOT / "host.lock"              # v2.2 修订：per-issue（原 run_dir 形同虚设）

def _ckpt_save(ctx: dict, step_id: str):
    tmp = CKPT.with_suffix(f".tmp-{os.getpid()}")     # tmp 掺 pid（D6）
    tmp.write_text(json.dumps({
        "flow_id": fl.flow_id,
        "flow_hash": _flow_hash(fl),                  # 图结构指纹（§4，D8）
        "run_id": run_dir.name,
        "saved_at": time.time(),
        "last_node": step_id,
        "context": ctx,
        # v2.2 新增：记录这份 context 实际所属的 run/worktree（续跑轮
        # $INPUT.run_dir 仍是首派 run_dir）。worktree_path 权威来源
        # $NODE.pre.worktree，缺失按 $INPUT.run_dir/worktree 推导。
        "ckpt_run_dir": ..., "worktree_path": ...,   # 跨派发续跑闸（O2 v2.2）
    }))
    tmp.rename(CKPT)                                  # 原子替换

host_fd = os.open(LOCK, os.O_CREAT | os.O_RDONLY)
try:
    fcntl.flock(host_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)   # 双宿主设防（D6）
except BlockingIOError:
    print(json.dumps({"verdict": "retry-later", "stage": "host",
                      "why": "another host holds host.lock"}))
    return 0

params = {"goal": goal, "repo": repo, "run_dir": str(run_dir),
          "agent": agent, "reviewer": reviewer}        # ← fresh 必传（D1/T5）
ex = FlowExecution(callback_handlers=[_Adapter(StepTracker(state_path)),
                                      LangfuseCallback()])          # §6
saved = _ckpt_load()                                  # §4：位置/版本/worktree 三重闸
node_retries: dict[str, int] = {}                     # 按节点计数（D3/R2/T2）
while True:
    try:
        r = (ex.run_distributed(fl, saved_context=saved) if saved
             else ex.run_distributed(fl, params=params))
    except FlowErrorException as e:                   # 仅异常类失败（§3）
        nid = _last_node_hint(e, saved)               # 归一化丢节点身份→从 checkpoint 反推（O7b）
        if _is_timeout_class(e) or node_retries.get(nid, 0) >= MAX_NODE_RETRIES:
            raise                                     # 超时类/预算尽 → engine_error 老路（D4）
        node_retries[nid] = node_retries.get(nid, 0) + 1
        _tracker_note_retry(nid, node_retries[nid])   # state.json 留痕（§5，T7/O6）
        saved = _ckpt_load()                          # last-success 断点
        continue
    if r.get("is_suspend"):                           # 防御分支（D7）：v2 无 EventNode，
        raise _HostError("unexpected suspend")        #   挂起=图被改，别白烧重试
    if r.get("is_end"):
        CKPT.unlink(missing_ok=True)                  # 终态不落 checkpoint（R1/D2）★
        result = r["result"]
        break
    _ckpt_save(r["context"], r.get("id"))
    saved = r["context"]
finally:
    fcntl.flock(host_fd, fcntl.LOCK_UN)
```

> ★ 终态 checkpoint 是评审抓出的最深陷阱：若终态（含 failed-preserved/
> retry-later）也落盘，keeper 重派会从 End 节点"续走"→ `_get_next_from_last`
> 返回 None → 把整张 `$NODE` 表当 verdict 返回 → 契约破坏且 L1 被永久挡住
> （R1/D2，P0×2 独立发现）。终态一律删 checkpoint，交给 L1 矩阵。

## 3. G1 重试语义：v2.1 修订口径

**机制成立**（评审核实）：节点失败在 `context.update_node_result` 之前抛出
（`runner.py:205-222`）→ 失败节点无 context 条目、`last_node_id` 仍指上一个
成功节点；宿主保存的 checkpoint 即"停在最后成功节点"的合法断点；continue
经 `_get_next_from_last`（`strategies.py:268-288`）解析出**刚失败的那个节点**
原地重执行。continue 守卫（`strategies.py:252`）只拦 EventNode——v2 无。

**边界修订**（吸收 R3/T1——原稿把"业务红"错划进宿主重试）：

| 失败类型 | 例子 | 宿主重试？ |
|---|---|---|
| **异常类**（会 raise） | AGENTRUN 的 AgentRunError（CLI 崩/非零退出/被杀）、CODE python 异常、GIT_PUBLISH/WRITEFILE/preflight 异常 | **是**（G1 覆盖面） |
| **业务红**（正常返回 False） | GATE 红门（`gate.py:90` 返回结构化结果，从不 raise）、评审 NEEDS_FIX | **否**——走 flow 图内修复环/failed-preserved，与 NORMAL 时代相同 |
| **超时类** | impl 撞 7200s 墙 | **否**——确定性信号，原地重试=再烧 2h（D4）；直接 engine_error，由 keeper 层重派走 L1+L2 |
| **协议类** | ResumeError、unmatched branch（图坏了） | **否**——与节点失败共用异常出口（R7），按 above 归入不重试类（`_is_timeout_class` 同款判别扩展） |

回调生命周期（R5/D7/T3）：每次节点失败 `_raise_distributed_error` 会 fire
`on_flow_end(error)`、而续传不再 fire `on_flow_start`——宿主**在终态显式补
`on_flow_end`**，并声明重试后的 Langfuse span 语义 = 同 trace 内分段（接受
一个 ERROR 根 + 后续段，或宿主层面按重试轮次分 trace——实现时二选一，
测试断言事件序列，见 s13）。

## 4. checkpoint 设计（v2.1：位置与组合顺序是本版最大的修正）

- **位置 = keeper 工件根 `~/.issue-keeper/pipeline/recursive-<n>/checkpoint.json`**
  （O1/T4：原稿放 run_dir 是结构性不可达——keeper 重派生成新 run_id，新
  run_dir 里永远查不到旧 checkpoint；工件根按 issue 稳定、reaper 从不清理。
  与 L2 sessions 已有的先例完全同构）。**per-issue 单槽**：新派发发现
  checkpoint 即续（内嵌 run_id 仅作审计）。
- **恢复三重闸**（`_ckpt_load`）：存在 且 flow 指纹一致 且 **worktree 存在**。
  v2.2 修订：worktree 闸查 checkpoint **记录的** `worktree_path`（跨派发可达）；
  旧格式（无该字段）回退检查本进程 `run_dir/worktree`（原行为，零回归）。
- **组合顺序不变量（O2，P0；v2.2 兑现跨派发半边）**：跨派发恢复**必须先 L1
  重建 worktree（wip 分支），再按 checkpoint 跳节点**——终态回收会 rmtree
  worktree，只续 checkpoint 不重建 worktree = 在已删除的路径上跑门禁 = paths
  条件门静默跳过 = 假绿落地。实现上：checkpoint 命中但 worktree 缺失 → 丢弃
  checkpoint 走完整 L1 路径（preflight 节点本就做 worktree add）。二者是叠加
  关系不是二选一。**v2.2 实现语义**：原实现闸查本进程 run_dir/worktree，而
  重派必换 run_id → 新 run_dir 必无 worktree → 「进程崩溃 → 续走」矩阵行
  结构性不可达（checkpoint 入口即弃，永远 L1）。修复 = checkpoint 新增
  `worktree_path` 字段，闸查记录路径：崩溃场景旧 run_dir 幸存（keeper reaper
  只 killpg + 清 run.lock，从不清 `.flowcast/runs/<run_id>`；唯一 rmtree 在
  bridge 终态段，进程死了自然不执行）→ 续跑在旧 worktree 上继续；树已被清
  （终态回收/手工）→ 照旧丢弃走 L1。续跑终态由宿主补回收旧 worktree（main()
  终态回收只看本进程 run_dir；WIP 快照后 rmtree，非终态不回收以保 checkpoint
  可续）。harness s19（跨派发续走）/s20（树已清走 L1）/s22（旧格式回退）。
  - **v2.3 补全（R1-1）：main() 终态回收按 checkpoint 活性豁免**。Track D 交
  接时发现的时序裂缝：v3 宿主对 engine_error（非终态意图）保留 checkpoint
  （只有 is_end 才删），而 main() 终态回收段对所有 verdict 无差别 WIP+rmtree
  ——「fresh run engine_error → checkpoint 存活但 worktree 被删」→ 重派
  `_ckpt_load` 的 worktree 闸必失败 → checkpoint 沦为死据、永远 L1 全量，
  「engine_error 后可续」落空（harness s23 修复前红实证）。修复：回收段对
  「verdict=engine_error 且 checkpoint 可解析且 `ckpt_run_dir` 指向本 run」
  跳过 WIP+rmtree，落 `worktree-preserved.log` 供值守判读；损坏/旧格式（无
  `ckpt_run_dir`，证实不了树归属且跨派发旧闸本就不认）/指向别 run 的一律
  照旧回收。磁盘上界=每 issue 至多多留一棵 worktree（checkpoint 指向的那棵），
  重派续走终态时由既有 `_recycle_resumed_worktree` 收口（s23 断言链闭合）。
  可达性注记：经 flow 走出的 engine_error 若发生在 pre 成功后，checkpoint 必
  已被本 run 刷新为指向本 run——「不合格 checkpoint + 本 run 有树」只见于
  pre 首步即炸（无树）与外部破坏 checkpoint 的形态，豁免判定（s24 决策表）
  对二者保守回收。
- **flow_hash = 图结构指纹**（flow_id + 节点 id 集 + 边表；D8/开放问题 D3
  采纳），并附 `plaita_version`/`plaita_nodes_version`（D8：节点结果 schema
  漂移的假绿风险）。不一致 → 丢弃 → L1。
- **损坏鲁棒**（T6）：`_ckpt_load` 对 JSON 截断/损坏返回 None（吞异常落
  recovery-error.log），与 flow_hash 不符同轨 → L1。

**组合矩阵（v2.1 修订）**：

| 场景 | 兜底层 |
|---|---|
| 节点**异常**（AGENTRUN 崩、CODE 异常、发布异常） | v3 宿主原地重试（每节点 1 次），成功节点跳过 |
| 宿主重试耗尽（engine_error + node_retry_exhausted） | checkpoint+worktree **保留待续**（v2.3：main() 回收豁免）→ 重派/人工处理后重派即断点续走；keeper 见标记不自动重派直接升级 |
| 业务红（门禁红/评审否决） | flow 图内修复环（原有，不属宿主） |
| impl 超时/被杀（节点内损失） | keeper 重派 → L1（代码）+ L2（会话 resume）；宿主不原地重试（D4）。⚠️ v2.3 注记：超时类经 engine_error 老路出（无 node_retry_exhausted 标记），回收豁免按 verdict 判定对其同样生效 → 重派走 checkpoint 续跑（impl 复得一次新预算）而非 L1 全量；keeper 的 engine_error 自动重派上限（2 次）兜住总代价，若值守判定必须强制 L1，删 per-issue checkpoint.json 即可（runbook 口径） |
| 进程崩溃/断电（图中断） | v3 checkpoint（工件根）+ worktree 在 → 续走（v2.2 兑现：闸查 worktree_path，跨派发可达；s19） |
| keeper 终态回收后重派 | checkpoint 丢弃（worktree 闸）→ L1 完整路径 |
| flow/plaita 版本漂移 | flow_hash/版本闸 → 丢弃 → L1 |
| 宿主进程被 kill -9 / KeyboardInterrupt（R8） | 无收尾——靠盘上 checkpoint + keeper 重派（写明，接受） |
| 双宿主并发 | host.lock → 后到者 retry-later 退出（D6） |

## 5. 契约（keeper 侧改动收敛为一个字段 + 一个已修 bug）

- **state.json/RESULT 契约不变**；~~run_dir 新增 `host.lock`~~ **v2.2 修订：
  host.lock 在 per-issue 工件根**（`ISSUE_ROOT/host.lock`）——原 run_dir 位置
  形同虚设：每次派发（含重派）run_id 必不同（keeper v2_bridge.py:91），双宿主
  各锁各的 run_dir 互不互斥，而被保护的 checkpoint/sessions/续跑 worktree 全是
  per-issue 资源（harness s21 实证）。语义不变：后到者 retry-later；flock 随
  进程死自动释放；⚠️ 持锁期间不得 unlink 锁文件（换 inode = 互斥失效），
  keeper 收尾只清 run.lock、永不触碰 host.lock。run_dir 下不再创建 host.lock；
- **node_retries 留痕契约**（T7/O6 落地）：`state.json.node_retries[node_id] =
  {count, last_error}`，StepTracker 增加 `note_retry`；`currentStep` 语义补充
  `<node>#attempt2` 后缀；**v2.2 修订：node_id 归因修复**——原实现取
  `saved.last_node`，但 saved 是 plaita context（无该键），计数自上线起恒记
  "unknown" 名下；现按 异常链 `__cause__.node`（raise 点本体）→ on_node_start
  流水 → checkpoint 反推 三级归因（harness s13 断言）；
- **台账 extra**：`node_retry_exhausted: true`（宿主重试耗尽）→ keeper 见标记
  跳过 engine_error 自动重派直接升级（O3 的预算墙另见 §8）；
- **keeper 已修 bug（评审 O3 发现，先行落地 929088e）**：engine_error 自动
  重试 off-by-one——台账终态行在计数前已落、决策又 +1 → 首败即升级、
  重试分支自 e2129da 起零触发（日志实证 0/24）。修复=决策直接用 trailing
  计数。**此修复是本方案叠加数学的前提**；
- **dispatch 链新增 `engine_env` 透传**（O4）：`PipelineRepoConfig.engine_env:
  dict` → payload → `v2_bridge` 的 `subprocess.run(env={**os.environ,
  **engine_env})`——照 agent/reviewer 字段先例（config.py:313 → keeper.py:1609
  → v2_bridge.py:97）。**灰度口径修订**（O4）：env 是 per-repo 旋钮，灰度=
  recursive 整仓开（日限/在途闸兜底），不做 per-issue。

## 6. Langfuse 接线（净增观测 + 生命周期归宿主）

`plaita.obs.LangfuseCallback` 加入 callback_handlers（LANGFUSE_* env 已在
bridge 注入链）。生命周期语义（R5/D7/T3）：DISTRIBUTED 下成功 End 不触发
`on_flow_end`、节点失败触发 error 版——**宿主在终态（成功或耗尽）显式补
`on_flow_end`**；重试轮次的 trace 分段策略在实现时定死并写进 s13 断言。

**v2.3 落地口径（R1-2）**：引擎侧事实（plaita `_error_normalization.py`）——
每次失败步 `run_distributed` 统一出口 `_raise_distributed_error` 已 fire
error 版 `on_flow_end`；正常 End 步 DISTRIBUTED 从不 fire（strategies.py 只造
end output）。宿主据此**只在非异常终态补发**（is_end / is_suspend /
deadline 墙，经 `ex.callback_manager` 对全部 handlers 分发，fail-open），
异常终态（重试耗尽 / 超时类 re-raise）不补——补发=Langfuse 根 span 二次
end。trace 分段取「同 trace 内分段」：1×on_flow_start（fresh）+ N×error
end（每失败步）+ 1×success end（宿主补发）；跨派发续跑轮 on_flow_start 不
再 fire，Langfuse 靠惰性重建挂回同 trace（obs.py 既有语义）。断言：s25
（正常终态恰补发一次，result=verdict）/ s26（异常终态恰为引擎版、宿主零
增减；变异验证：宿主多补一次即红）。childflow（has_changes/gate_once）在
NORMAL 段 fire 自己的流级事件，属引擎既有行为，断言按 flow_id 过滤根流。

## 7. 测试计划（v2.1：按 T1/R3 推翻重写 s13/s14）

桩机制经核实两策略一致（`NodeRunner.run_node → Node.execute` 类级替换拦得到；
childflow 在 DISTRIBUTED 下整段 NORMAL 跑完）。修订集（~13 场景）：

- **s11 等价性**：s1+s2 两条在宿主循环下终态 dict 与 CALLS 序列一致，且
  impl 收到 goal/repo（兼验 params 首传，D1/T5）；**全部场景挂生产 handlers**
  （StepTracker + 离线 LangfuseCallback，T3）；
- **s12 崩溃恢复（升级）**：两个 bridge 子进程——第一个 kill -9 于
  checkpoint.json 出现后（N 固定选**修复环中段**：首门红+fix 完成后复检前，
  必穿 last_branch 还原，T8），第二个同参重入 → 续走 → committed；
- **s13/s14 推翻重写（T1/R3）**：桩加 `@RAISE` 哨兵（`_route_agent` 抛异常
  模拟 CLI 崩溃）——s13 = impl 抛一次 → 宿主重试 → committed，断言失败前
  节点未重复执行 + 回调事件序列；s14 = 连抛 → engine_error + checkpoint
  保留 + `node_retry_exhausted` 落台账；gate 红类失败已由 s2-s5 覆盖，勿混；
- **s15 扩为 checkpoint 不可信家族**（T6）：flow_hash 不符 / JSON 截断损坏 /
  遗留 .tmp → 同一断言（load 返回 None → 全新 L1 → committed）；
- **s16 终态后重派**（R1/D2）：committed 终态后 checkpoint 已删 → 重派走
  L1 → 不复现 `$NODE` 表 verdict；
- **s17 worktree 闸**（O2）：checkpoint 命中但 worktree 已被终态回收删除 →
  丢弃 → L1 → 不假绿；
- **v2.2 新增 s19-s22**：s19 跨派发续走（崩溃形态旧 worktree 幸存 → 新 run_id
  从断点续走、活落旧树、终态回收旧树）；s20 跨派发树已清 → 丢弃走 L1；
  s21 双宿主互斥（per-issue host.lock，后到者 retry-later 且零节点执行）；
  s22 旧格式 checkpoint 回退 run_dir 闸（同 run_dir 续跑零回归）；
  s13 补 node_retries 归因断言（键=失败节点 impl，非 unknown）；
- **v2.3 新增 s23-s27**：s23 engine_error+存活 checkpoint → main() 回收豁免
  （worktree 幸存 + 标记日志）→ 重派断点续走 → 终态 `_recycle_resumed_worktree`
  收口旧树（修复前红实证）；s24 回收豁免判定决策表（损坏/旧格式/别 run 不
  豁免，含可达性注记）；s25 正常终态宿主恰补发一次 on_flow_end（根流过滤）；
  s26 异常终态恰为引擎 error 版、宿主零增减（变异验证过断言有效性）；
  s27 dry-run+v3 真 bridge 无桩冒烟（见上）。
- **新增廉价冒烟**：真 bridge `--dry-run` + `RECURSIVE_HOST_V3=1`（无桩），
  断言 verdict/checkpoint/state 契约（T5）。**v2.3 落地（R1-4）**：原实现
  `use_v3 = (not dry_run) and ...` 把 dry_run 排除在 v3 外，本冒烟结构性走
  不到 v3（设计-实现矛盾）。核实后放开：dry_run 经 `global_context →
  setup_flow → $GLOBAL`，DISTRIBUTED fresh 与 NORMAL 同一条 setup 路径、
  节点侧同款 `get_global_variable("dry_run")` 消费，且 `$GLOBAL` 随 checkpoint
  续跑带回——无实质障碍。harness s27 = 子进程真 bridge（无 harness 桩，
  HOME 收口 tmp 防写生产工件区），断言 exit 0 / skip-commit / host.lock 存在
  （v3 指纹）/ checkpoint 已删 / worktree 照旧回收 / state.dry_run=true。

## 8. 预算墙与三口时钟（O3/O5）

三口独立时钟现状：keeper `pipeline_timeout_secs`（8h，killpg 整组、timed_out
路径**直接消费无自动重试**）、v2_bridge `V2_TIMEOUT_SECS`（8h）、v3 宿主
重试预算（隐含 ≥ Σ节点预算×2）。**不变量**：`pipeline_timeout_secs ≥
Σ(节点预算)×(1+MAX_NODE_RETRIES) + 固定开销`；宿主在每次节点执行与重试前
检查**run 级 deadline**（dispatch payload 注入 epoch，engine_env 同链路），
不足该节点预算即快速失败。impl 4h + 门禁环 + 评审环逼近 8h 的组合由 deadline
显式拦截而非靠 keeper 杀进程的 timed_out 路径（那条路无重试且只有兜底评论）。

## 9. 工作量（v2.1 修订后）

| 项 | 估时 |
|---|---|
| 宿主循环（含 flock/三重闸/按节点重试/超时类判别/deadline） | ~3h |
| Langfuse 接线 + 生命周期 | ~0.5h |
| `engine_env` 透传（keeper config/keeper/v2_bridge 三点 + 测试） | ~1h |
| harness s11-s17 + 生产 handlers 接入 | ~3h |
| 三过 + canary（recursive 整仓开 env） | 1 个观察班次 |
| **合计** | **~0.8 天编码 + 1 个观察班次** |

## 10. 开放问题（v2.1 更新）

- D1 node_retries 已升级为验收项（§5 契约）——值守判读口径待班次演练；
- D2 checkpoint 是否记 impl session_id：暂缓（preflight 扫描已够用，多会话
  并存场景出现再议）；
- D3 ~~flow_hash 口径~~ → 已采纳图结构指纹 + 版本号（§4）；
- D4 console schema 平移：仍后置；
- **D5（新）WorkspaceLease** → **closed（2026-10-02，R1-3 核实废止）**。原担忧：
  checkpoint 恢复还原同一 execution_id → 撞死进程的文件租约（TTL≈2h）→
  SandboxLeaseError 空转。核实结论（证据链）：
  ① 租约机制根本不在 v2/v3 执行路径上——`WorkspaceLease`/`FileLeaseStore`/
  `SandboxLeaseError` 定义于 plaita-nodes `sandbox.py`（sandbox 驱动族配套），
  全大仓 grep 无任何消费方；v2 flow 的节点（AGENTRUN/GATE/CODE/GIT_PUBLISH）
  均不 import sandbox，recursive（Rust）侧亦无 lease 概念（初查命中全是
  release 误匹配）。
  ② 执行器侧唯一真实互斥是 agentproc `run_lock`（agent_run.py:57 遗言锁）：
  普通文件 + pid 存活探测的 kill-before-start 设计——死进程残留锁被
  `cleanup_stale_run` 按 stale 即时清理放行（自愈，无 TTL 停滞窗口），活进程
  被 killpg 接管；与本设计的双宿主竞态正交且不空转。
  ③ 同 issue 双宿主竞态已被 v2.2 的 per-issue `host.lock`（flock，进程死内核
  自动释放）结构性覆盖——checkpoint 恢复与持租者并存的前提（双活宿主）不再
  成立。
  ④ 退一万步：即便未来接 sandbox 且撞上死进程租约，`FileLeaseStore.acquire`
  第二次尝试即「过期抢占」（expires_at 过 → unlink → 重抢），D5 选型 (c)
  「TTL 停滞窗口」是现状内置行为，(a) 存活探测无对象可施。若日后 sandbox
  路线接入 v2 flow 再重开。
- **D6（新）keeper 自动重试修复（929088e）与 v3 宿主重试的叠加**：宿主重试
  是节点级、keeper 是派发级，二者语义正交；但 `node_retry_exhausted` 标记
  的消费行为（跳过自动重派）需在 keeper reaper 加一个 if——这打破了 §5
  「零改动」？否：该标记走台账 extra，reaper 判断是既有升级路径的前置过滤，
  实现时连同 O3 修复合入。

## 11. 评审记录

四路交叉评审（架构 / 重试语义 / 运维契约 / 测试策略），全部
APPROVE-WITH-CHANGES，共 32 条意见（D1-D8 / R1-R8 / O1-O8 / T1-T8）。
本版已吸收全部 P0×5（params 遗漏 D1/T5/O7、终态 checkpoint 陷阱 R1/D2、
checkpoint 位置不可达 O1/T4、组合顺序假绿 O2、测试空洞 T1/R3）与全部 P1；
P2 中 T6/T8/R7/R8 已并入正文，其余（含 keeper 自动重试死分支的生产修复
929088e）随实现落地。意见全文见值守会话记录。
