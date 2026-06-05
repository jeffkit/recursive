# Manual edit: self-improve-dashboard

**Date**: 2026-06-05
**Goal**: 为 Recursive 自我迭代流程构建可观测 Dashboard
**Files touched**:
- `.dev/dashboard.html` — 新增 57KB 自包含 HTML Dashboard

**Tests added**: none（纯观测工具，不影响产品代码）

**Notes**:
Dashboard 包含以下模块：

## 功能模块
1. **总览 Tab** — KPI 卡片（成功率/代码产出/总工具调用/平均耗时）+ 7 张图表
   - 运行结果时间线（绿=提交 红=回滚）
   - 各 Provider 成功率 vs 平均步数（双轴图）
   - 步数使用分布（步数 vs 预算折线）
   - 代码改动量（新增/删除/累计净增）
   - 结果分布 Doughnut
   - 退出原因分布
   - 各 Provider 平均耗时水平条形图

2. **运行记录 Tab** — 完整运行列表，支持 全部/已提交/已回滚 筛选

3. **失败分析 Tab** — 
   - 回滚运行详情（2 次：provider-presets / external-hook-process）
   - 高错误率运行列表（error_count > 10）
   - 各 Provider 风险分析
   - 改进建议

4. **实时监控 Tab** — 接入 HTTP /metrics Prometheus 端点（Phase 15.2）
   - 8 个实时指标显示
   - 可配置服务器地址
   - 5s 自动刷新
   - 实时趋势折线图（最近 60 个数据点）

5. **Roadmap Tab** — Phase 14-20 完成进度条 + 堆叠条形图 + Batch 进度图

## 发现的数据质量问题
- `tokens_prompt`/`tokens_completion`/`cost_usd` 全为 0，self-improve.sh 尚未写回真实数据
- `batch` 字段所有记录均为 36，未自动递增
- 2 次回滚均为 cargo test 失败（非 Stuck），细分分类有价值

## 修复 (2026-06-05)
- **token/cost 提取** (`self-improve.sh emit_metrics`): 修复 jq 查询逻辑
  - 主路径: 从 `session: recording to PATH` 提取 session_dir → 读取 `cost.json`
  - 备用路径: 用 `rg -oP 'cost: \$\K[0-9.]+'` 从 LOG 文件提取 cost_usd
  - 原因: 旧逻辑依赖 transcript messages[]?.usage，但 transcript 不含 usage 字段
- **batch 字段动态化** (`self-improve.sh`): 从 `.dev/current-batch` 文件读取
  - 初始值: 36（与原硬编码一致）
  - 调整方法: `echo N > .dev/current-batch`
  - 创建 `.dev/current-batch` 文件（内容: 36）

## 后续建议
- 考虑将 dashboard.html 加入 .gitignore 或改为 git-tracked 的观测工具
- 当开始新 batch 时，更新 `.dev/current-batch` 文件值
