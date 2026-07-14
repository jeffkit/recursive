Quality gate "e2e" failed (exit n/a):

```
[e2e-gate] cargo build 失败

```


## Evaluator 输出失败

Evaluator 跑结构化输出失败（3 次重试仍非合法 JSON）。这通常是 evaluator 模型自身的输出格式问题，不一定是代码错。
请 generator 重新自评一遍 contract 各验收点，确认实现无误。