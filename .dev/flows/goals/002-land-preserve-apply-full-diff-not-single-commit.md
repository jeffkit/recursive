# Goal: land-preserve 落地完整改动树，而非 cherry-pick 单个 commit

给 `.dev/flows/self-improve.flow.js` 的 `landPreserve()` 修一个丢改动的 bug：当 preserve
worktree 在初次 preserve 之后又被追加了 commit（人工修正、或 `--resume-preserve` 产生
的新 commit），`refs/preserve/<run-id>` 指向的是**链顶最后一个** commit，而
`landPreserve` 现在用 `git cherry-pick <sha>` 只取这一个 commit 相对其父的增量，**丢失
了同一链上更早的 commit 内容**。

真实案例：goal 334（compaction upgrade）的人工救回过程中，preserve worktree 上有 3 个
commit（agent 原始实现 + wiring 修正 + 补测试）。`land-preserve` 只 cherry-pick 了最后一
个（测试 commit），核心实现（reinject.rs / runtime.rs wiring）全部丢失，main 上只剩 75
行测试代码、缺 591 行实现。靠事后用 `git diff baseline..sha` 手动 patch 才救回。

`preserveScene()` 自己其实做对了（line 498：`git(['diff', \`${baseline}..${wtSha}\`]` 导出
完整 diff 到 `preserved.diff`），但 `landPreserve()` 没复用这个思路。

## 背景与现状

`landPreserve()`（self-improve.flow.js 约 line 556-589）当前落地逻辑：

```js
try { git(['cherry-pick', '--no-commit', sha], repo) } catch (err) {
  try { git(['cherry-pick', '--abort'], repo) } catch { /* */ }
  throw new Error(`cherry-pick conflict landing ${sha.slice(0, 8)}: ${err.message}`)
}
git(['commit', '-m', `self-improve: ${goalSubject()} [land-preserve ${preserveRunId.slice(-6)}]`], repo)
```

`sha` = `git rev-parse refs/preserve/<run-id>` = preserve 链顶 commit。问题：

- preserve 链只有 1 个 commit 时（最常见：`preserveScene` 一次性 commit WIP），cherry-pick
  正确（该 commit 的父就是 run baseline，单 commit 含全部改动）。
- preserve 链有 ≥2 个 commit 时（人工在 preserve worktree 追加修正，或 resume-preserve 产
  生新 commit 后再 preserve），cherry-pick 单个 sha 只带最后一个增量，**丢前面的**。

注意：`resume-preserve` 路径（`runAttemptWithGoal`）在成功时走的是自己的 commit 逻辑（它
在 resume worktree 里 `git add -A && git commit` 后 cherry-pick 那个新 commit），也有同
样的单-commit 问题——但 resume 场景下 worktree 通常只有一个累积 commit（add -A 把所有改
动打包成一个），所以实际风险较低。本 goal 聚焦 `landPreserve`。

## Requirements

1. **落地完整改动树**：`landPreserve` 不再 `cherry-pick <sha>`。改为把 `sha` 相对 main
   的**完整改动**应用到 main 工作树并提交。用「共同祖先到 sha」的 diff，这样不管
   preserve 链上有几个 commit，都拿到累积的全部改动。

   推荐实现（不需要显式存 baseline）：

   ```js
   // 全绿 → 把 preserve 的完整改动树应用到 main
   // 用 merge-base 求出 sha 相对 main 的共同祖先（= run baseline 或更早），
   // diff 祖先..sha 就是 preserve 上累积的全部改动，与 preserveScene 导出
   // preserved.diff 的 baseline..wtSha 思路一致，但不依赖 state 里存 baseline。
   const mainHead = git(['rev-parse', 'HEAD'], repo)
   const ancestor = git(['merge-base', mainHead, sha], repo)
   // 等价于把 preserve 链的完整改动应用到 main，单个干净 commit 落地。
   if (ancestor === sha) {
     // sha 已经是 main 的祖先（无可落地改动）——理论上不该发生（门刚过），防御性报错。
     throw new Error(`land-preserve: ${sha.slice(0,8)} is already an ancestor of main; nothing to land`)
   }
   // 用 cherry-pick 的「范围」形式落地祖先之后的全部 commit，保留单 commit 的原子性。
   // cherry-pick A..B 落地 (A, B] 即 A 之后到 B 的所有 commit。
   // 但我们想要「压成单个 commit」，所以用 diff 应用 + 单次 commit（不保留中间历史）：
   const fullDiff = git(['diff', `${ancestor}..${sha}`], repo)
   if (fullDiff.trim()) {
     try { git(['apply'], repo, { stdin: fullDiff }) } catch (err) {
       throw new Error(`land-preserve apply conflict (${ancestor.slice(0,8)}..${sha.slice(0,8)}): ${err.message}`)
     }
   }
   git(['add', '-A'], repo)
   git(['commit', '-m', `self-improve: ${goalSubject()} [land-preserve ${preserveRunId.slice(-6)}]`], repo)
   ```

   关键不变量：
   - 用 `git merge-base <mainHead> <sha>` 求共同祖先（不依赖 state 存 baseline）。
   - 用 `git diff ancestor..sha` 取完整累积改动（含文件增删，`--no-commit` 不需要）。
   - 用 `git apply` 应用（支持二进制/重命名；若需更稳可加 `--3way`，但 `--3way` 在冲突时
     会留下冲突标记，需权衡——首版用裸 `apply`，失败即报清晰错误，和现在 cherry-pick
     冲突的处理一致）。
   - 单次 `git add -A && git commit` 压成一个干净 commit（落地语义不变：一个 goal 一个
     commit）。

2. **保持冲突时的错误清晰**：apply 失败时，错误信息要包含 ancestor 和 sha 的短 hash，
   方便排查（和现在 cherry-pick 冲突错误的风格一致）。apply 失败时 main 工作树可能处于半
   应用状态——用 try/catch + `git checkout -- .` / `git clean -fd` 清理未跟踪文件后抛错，
   避免污染 main 工作树（参考现在 cherry-pick --abort 的清理意图）。

3. **`git()` helper 是否支持 stdin**：`git apply` 需要把 diff 内容从 stdin 喂进去。检查
   flowcast 的 `git()`（node_modules/flowcast/git.js:33）是否支持 `{ stdin }` 选项。如果
   不支持：
   - 方案 A（推荐）：把 diff 写到 run 目录的临时文件（`join(cp.dir, 'land.diff')`），
     `git(['apply', diffFile])` 从文件应用。简单、不依赖 git() 改造。
   - 方案 B：给 flowcast 的 git() 加 stdin 支持（改 flowcast 子仓，跨仓）。
   首选方案 A（不碰 flowcast）。

   方案 A 的 diff 文件路径要放 `cp.dir`（run 目录，已 gitignore），不要污染 main 工作树。

4. **merge-base 已是 sha 的防御**：如果 `ancestor === sha`（sha 已在 main 历史里，无可落
   地内容），抛清晰错误而不是静默 `git apply` 空内容后 `git commit` 出一个空 commit。

## 非目标

- 不改 `preserveScene`（它导出 preserved.diff 的逻辑是对的）。
- 不改 `resume-preserve` 的落地路径（`runAttemptWithGoal` 里的 cherry-pick）——resume 场景
  通常单 commit，风险低；如要改另开 goal。但可以在该处加同样的注释指向本 goal。
- 不引入「保留 preserve 链中间 commit 历史」——self-improve 的语义就是一个 goal 一个干
  净 commit，diff-apply + 单 commit 符合现状。
- 不改 flowcast 子仓（用方案 A：diff 写临时文件）。

## Definition of done

- `.dev/flows/self-improve.flow.js` 的 `landPreserve()` 改用 merge-base + diff apply + 单
  commit 落地。
- `git apply` 从临时文件（`cp.dir/land.diff`）读取，不依赖 git() 的 stdin 支持。
- apply 失败时清理 main 工作树（`git checkout -- . && git clean -fd` 或等价）后抛清晰错
  误。
- 测试：在 `.dev/flows/test/` 下加一个用例，构造「preserve worktree 上有 2 个 commit」的
  场景，验证 land-preserve 后 main 上有**两个 commit 的累积改动**（不是只有最后一个）。
  参考 `.dev/flows/test/` 现有用例的假 git 仓 + 假 cargo 模式。
- 现有的 land-preserve 行为（单 commit 场景）不回归。
- `node .dev/flows/self-improve.flow.js --list` 仍正常；flow 自身 E2E（`cd .dev/flows &&
  npm test`）全绿。

## 测试思路（给实现者）

构造一个临时 git 仓当 repo，模拟：
1. baseline commit（main HEAD）。
2. 一个 preserve ref，指向一个链：commit A（加文件 X）→ commit B（改文件 X + 加文件 Y）。
   即 preserve ref = B，B 的父是 A，A 的父是 baseline。
3. 调用改造后的 land 逻辑（merge-base baseline B = baseline；diff baseline..B 含 X 的全
   部内容 + Y）。
4. 断言 land 后 main 工作树有文件 X（含 A 的改动）**和**文件 Y（B 的改动）——证明两个
   commit 的改动都落地了。旧逻辑（cherry-pick B）只会带 B 相对 A 的增量（文件 X 的修改
   + 文件 Y），**丢掉 A 建立文件 X 的事实**（cherry-pick B 时 X 的基线内容不对，可能冲突
   或产出错误内容）。

如果 flowcast 的 git helper 在测试里难以构造多 commit 链，退而用真实临时 git 仓
（`tempdir` + `execFileSync('git', ...)`），和现有 E2E 测试的做法一致。
