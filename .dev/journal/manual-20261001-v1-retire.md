# Manual note: self-improve v1 退役

- Date: 2026-10-01
- Goal: v1 厚引擎世代退役（jeffkit「V1 本来就应该退役」）——keeper 接单与 recursive 自迭代收敛到 v2 codeflow 单轨
- Files touched: .dev/scripts/launch-flow-plaita.sh（改调 self_improve_bridge_v2.py，v1 旗标翻译/丢弃 + SELF_IMPROVE_AGENT/REVIEWER 默认 glm53-flash）；skill self-improve-supervise 引擎表重写；v1 四文件（bridge/engine/flow/plaita.json）打 DEPRECATED 头标
- Tests added: 适配层冒烟（v1 旗标 --provider/--model/--no-review 进 v2 真启动 ✓，goal-file 路径 ✓）
- Notes: ① console 上的 self-improve v1.0.0 注册成为陈旧残留（勿用，未删）；② launch-flow.sh（flowcast）保留为回滚引擎未动；③ 今天的 flow 修复（门禁环/探针/L1/L2）此前只惠及 keeper 线，本次收敛后自迭代同享。
