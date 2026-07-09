Evaluator 跑结构化输出失败（3 次重试仍非合法 JSON）。这通常是 evaluator 模型自身的输出格式问题，不一定是代码错。

请 generator 重新自评一遍 contract 各验收点，确认实现无误；下一轮 evaluator 会重试。

错误：runStructured: 3 次尝试后仍不符合 schema — 输出不是合法 JSON，请只输出一个 JSON（可包在 ```json 代码块里），不要任何解释文字。
