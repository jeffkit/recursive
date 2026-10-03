# manual-20261003-artifact-canonical-producer

## Date
2026-10-03

## Goal
#84 chore(flows): self-improve-v2 编译产物正典生产者拍板 + 重建同步（产物落后源码）

## Files touched
- `.dev/flows/compile_v2.py`（新增，正典编译器）
- `.dev/flows/self_improve_flow_v2.py`（`__main__` 转发 compile_v2；新增 `CHILDFLOW_BY_NODE` 映射）
- `.dev/flows/self-improve-v2.plaita.json`（正典路径重建，151 行语义 diff）
- `.dev/OPERATIONS.md`（§11 警告段改为重建命令 + byte-stable 校验 + console 发布协调口径）

## 拍板内容
- **正典格式 = console 发布 definition 形态**（生产运行时真相是 console 上
  发布的 version；keeper `engine=v2-console` 派发消费它）。
- **正典生产者 = 仓内 `compile_v2.py`**：`Flow.model_dump(by_alias=True)` 的
  IR → console 形态（`type` 首键、null 剔除、timeout/error_handler/branches/
  output_type/sandbox_backend/max_retries/dry_run 簿记剥离、`resultType`/
  `childFlow`/`inputType` 别名键、`source_line` 与「（第 N 行）」注解加
  装饰器行-1 偏移成源文件绝对行号）→ `json.dumps(indent=2, ensure_ascii=False)`
  落盘。
- 已验证：e03e0f5e 时点源码 + 本序列化器 = e03e0f5e 产物 **byte 级相等**
  （复刻正确性的最强证据）；当前源码 + 本序列化器与线上 console 1.0.0
  definition parse 后逐字段一致（`/api/flows/self-improve-v2/versions/1.0.0`）。

## Tests added
- 无新增测试文件；验收即回归：
  1. 连续两次重建 byte 相同（`diff` 为空）✓
  2. `compile_v2.py --check`：产物同步时绿、注入脏字节后红 ✓
  3. 产物 parse 回 `Flow.model_validate` + register_code_node：62 节点加载 OK，
     `impl.timeout_secs == $NODE.impl_timeout` ✓
  4. 归一化图相等（剥 source_line、行注解截断）：源码 `__plaita_ir__` vs 产物
     62 节点 0 差异 ✓
  5. `flow_v2_paths.py` 27/27 ✓；`self_improve_bridge_v2.py --dry-run`
     verdict=skip-commit ✓（产物不参与 bridge 运行时，纯防回归）

## Notes
- 产物 151 行 diff 全部是 54b1967f 的语义（`impl_timeout` 赋值节点 +
  `impl.timeout_secs` 接线）+ 行号平移，格式零翻转——审查可读，正是本次拍板的目的。
- 字节稳定性来源：无时间戳/版本字段、dict 插入序固定、节点序 = 编译序。
  若某次重建无源码变更却出 diff = console 序列化规则漂移，先拍板再同步。
- console 发布协调（G 系列）：改源码后需发布新 console version，definition
  必须与仓内产物逐字节一致（用 compile_v2 输出导入，勿在画布重序列化）；
  发布前 console 派发的 run 跑的是旧 definition，属真实产物落后，勿误判为代码回归。
- 旧 `__main__` 的 `json.dumps(model_dump(by_alias=True), indent=1)` snake_case
  路径已废弃移除；`python3 self_improve_flow_v2.py` 现在产出正典格式。
