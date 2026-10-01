"""codeflow 三种执行形态演示：NORMAL 一口气 vs DISTRIBUTED 逐节点+可恢复。
同一个 flow、同一个 FlowExecution 引擎——差异只在驾驶策略与宿主循环。"""
import json
from plaita.dsl.codeflow import CODE, EVENT, flow
from plaita.node import register_code_node
register_code_node(default_backend="unsafe")   # 演示用免沙箱

# ── 被试 flow：impl（改数）→ 事件门（人工审批挂起点）→ publish ──
@flow("dist-demo")
def dist_demo():
    a = CODE(id="impl", lang="python",
             code="def run(input):\n    return {'n': 41 + 1}")
    e = EVENT(id="approve", type="approval")
    p = CODE(id="publish", lang="python",
             input={"n": a.n},
             code="def run(input):\n    return {'shipped': input['n'] == 42}")
    return {"shipped": p.shipped}

from plaita.core.executor import FlowExecution

print("═══ 形态一：NORMAL（default）——一次 execute 跑到终局 ═══")
ex = FlowExecution()
out = ex.execute(dist_demo, params={})
print("结果:", out, "\n（宿主只调一次；中途崩了就全重来的正是这层）\n")

print("═══ 形态二：DISTRIBUTED——宿主逐节点驱动，checkpoint=宿主手里的 dict ═══")
ex2 = FlowExecution()
CKPT = "/tmp/dist-demo-ckpt.json"

def drive(saved=None, **kw):
    """宿主单步：喂 checkpoint → 引擎推进一个节点 → 还回新 checkpoint。"""
    r = ex2.run_distributed(dist_demo, saved_context=saved,
                            **({"resume_type": kw["resume_type"],
                                "resume_data": kw["resume_data"]} if kw else {}))
    ctx = r["context"]
    open(CKPT, "w").write(json.dumps(ctx))          # ← 宿主持久化（DB/文件随意）
    print(f"  推进节点: {r.get('id')}  result={str(r.get('result'))[:30]}  "
          f"is_suspend={r.get('is_suspend')}  is_end={r.get('is_end')}")
    return r

r = drive()                                        # 步进直到挂起或完成
while not r.get("is_end"):
    if r.get("is_suspend"):
        print("  >>> 挂起在 EventNode（人工/外部事件位）→ 事件恢复 <<<")
        r = drive(r["context"], resume_type="event", resume_data={"approved": True})
    else:
        r = drive(r["context"])
print("终局:", r.get("result"))
