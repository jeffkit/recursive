#!/usr/bin/env node
/**
 * recursive-self-improve flow — 把 recursive 的 self-improve.sh（49KB bash 自改循环）
 * 忠实迁移成可审计 / 可观测 / 可断点续跑的 flowx JS flow。
 *
 * 关键路径（对齐 self-improve.sh）：
 *   baseline/clean 预检
 *     → 构建 system prompt（注入 AGENTS.md 契约 + 最近 journal + 上次失败上下文）
 *     → 跑 recursive 二进制
 *     → BudgetExceeded 自动 resume（一次）
 *     → 质量门（test / clippy / fmt / e2e，各带 N 轮 resume-fix 循环，喂最新 stderr）
 *     → 跨 provider self-review（NEEDS_FIX 也走 N 轮 resume-fix，不再直接回滚）
 *     → verdict（committed / failed-preserved / skip-commit / panic-preserved）
 *
 * 失败不再硬回滚：门禁/评审循环耗尽后 verdict=failed-preserved，保留 worktree +
 * refs/preserve/<run-id> 分支 + preserved.diff + <gate>-failure.log，供人或更强 agent
 * 经 --resume-preserve / --land-preserve / --prune-preserve 消费。回滚动作基本被消灭。
 *
 * 不改 recursive 的 Rust kernel 一行；recursive 二进制仅作为被调度的执行器。
 *
 * 用法（在 recursive 仓根目录执行；先 `cd .dev/flows && npm install` 链好 flowx）：
 *   node .dev/flows/self-improve.flow.js --goal "<目标>" --provider deepseek
 *   node .dev/flows/self-improve.flow.js --run-id <id>            # 断点续跑
 *   node .dev/flows/self-improve.flow.js --list
 *
 * 详见 .dev/flows/SELF_IMPROVE.md。
 */

import { parseArgs } from 'util'
import { readdirSync, readFileSync, writeFileSync, rmSync, existsSync, statSync } from 'fs'
import { join, relative } from 'path'
import { execFileSync } from 'child_process'

import {
  Checkpoint,
  runAgent, recursiveProviderEnv, setWorkdir, setHitlBackend, notify, waitForInput,
  captureBaseline,
  runGate, loadGates, mergeGates,
  writeFailureContext, readAndConsumeFailureContext,
  loadProviders, resolveProvider,
  flowcastDir,
  gitWorktreeAdd, gitWorktreeRemove,
} from 'flowcast'

// flowcast 0.6 把 per-CLI adapter 换成了 agentproc in-process executor；
// `recursive` 不再是顶层可调用导出，统一走 runAgent({cli:'recursive', ...})。
// 这里保留老 `recursive(prompt, opts)` 调用形态，让下方 5 个 call site 零改动。
const recursive = (prompt, opts) => runAgent(prompt, { cli: 'recursive', ...opts })

// ── CLI 参数 ─────────────────────────────────────────────────────
const { values: opts } = parseArgs({
  options: {
    'run-id':   { type: 'string' },
    repo:       { type: 'string', default: process.cwd() },
    bin:        { type: 'string' },                 // recursive 二进制路径（worktree 无预编译时复用 main 的）
    goal:       { type: 'string' },
    'goal-file':{ type: 'string' },
    provider:   { type: 'string' },                 // 注入 recursive 的 provider（env）
    model:      { type: 'string' },
    budget:     { type: 'string' },                 // 兼容旧名；同 --max-steps → RECURSIVE_MAX_STEPS
    'max-steps':{ type: 'string' },                 // agent 步数上限（RECURSIVE_MAX_STEPS）
    'reviewer-provider': { type: 'string' },         // 跨 provider self-review（用 recursive 执行器）
    'reviewer-agent':   { type: 'string' },          // 跨 agent self-review（如 claude，自管鉴权）
    'reviewer-max-steps': { type: 'string' },        // reviewer 步数上限（防止只读 reviewer 空转）
    hitl:       { type: 'string', default: 'terminal' }, // terminal | wecom | ilink
    'project-name': { type: 'string', default: 'recursive' },
    'no-review':{ type: 'boolean', default: false },
    'no-commit':{ type: 'boolean', default: false },
    list:       { type: 'boolean', default: false },
    'commit-pending': { type: 'boolean', default: false }, // 快速补提交：跳过 agent/质量门，直接提交当前工作树改动
    // ── 失败保留与消费（g325：把"程序化回滚"改成"agentic 修复 loop + preserve 后盾"）──
    timeout:        { type: 'string' },              // agent 单次 run 硬超时 ms（默认 2h）
    'max-fix-rounds': { type: 'string' },            // 门禁/评审 resume-fix 循环上限（默认 3）
    'fixer-provider': { type: 'string' },            // resume-fix 轮用的 provider（默认同主 agent）
    'resume-preserve': { type: 'string' },           // 从 refs/preserve/<run-id> 接着修
    'prune-preserve':  { type: 'string' },           // 清掉一个 preserve 现场（run-id）
    'land-preserve':   { type: 'string' },           // 把 preserve 现场跑门后落地
  },
})

if (opts.list) { listRuns(); process.exit(0) }

const runId = opts['run-id'] ?? `selfimprove-${Date.now()}`
const repo = opts.repo

// ── 失败保留相关常量（g325）─────────────────────────────────────
// agent 单次 run 硬超时：默认 2h（旧值 30min 对真实 goal 太短，曾导致 agent 被杀在半路、
// 半成品过门必红、1 轮 resume-fix 救不回 → 回滚丢成果）。--timeout ms 覆盖。
const RUN_TIMEOUT_MS = Number(opts.timeout) || 7_200_000
// 门禁 / 评审 resume-fix 循环上限：旧实现只 1 轮就回滚；改成 N 轮，每轮喂最新 stderr/评审意见。
const MAX_FIX_ROUNDS = Number(opts['max-fix-rounds']) || 3
// 失败现场保留目录：与活跃 self-improve worktree 分开命名空间，不污染新 run。
const PRESERVE_DIR = join(repo, '.worktrees', 'preserve')

// Batch-run constraint prepended to every system prompt:
// plan mode tools block indefinitely when no interactive channel is present.
const HEADLESS_CONSTRAINT = `# Headless batch-run constraints

You are running non-interactively (no human in the loop).

**DO NOT call \`enter_plan_mode\` or \`exit_plan_mode\`.** These tools block
forever waiting for a human to approve the plan — in batch mode there is no
approval channel, so calling them causes an unrecoverable deadlock.

Implement directly: read → think → patch → test. No plan-mode ceremony needed.

# Mandatory self-verification before stopping (do NOT skip)

Before you declare the goal done and stop calling tools, you MUST run all
three quality gates yourself in the worktree and make them green:

1. \`cargo fmt --all\`                       (format — run, don't just --check)
2. \`cargo clippy --all-targets --all-features -- -D warnings\`
3. \`cargo test --workspace\`

The flow runs these again after you stop, but they are a *backstop*, not the
first check. If you stop with clippy lints or failing tests still in the tree,
your work gets preserved as \`failed-preserved\` instead of landed, and a
weaker model may not get a second chance to fix it. So:

- Run clippy yourself, read every \`error:\` line, fix the underlying source,
  re-run clippy until it is clean. Do NOT silence lints with \`#[allow]\` to
  make the noise go away — fix the code.
- Common clippy fixes for this repo:
  - \`clippy::unwrap_used\` on \`Mutex::lock()\`: replace \`.lock().unwrap()\`
    with \`.lock().unwrap_or_else(|e| e.into_inner())\` (poison-recovery;
    also satisfies invariant #5 — no \`unwrap()\` in product code).
  - \`clippy::expect_used\`: same idea — recover or propagate via \`?\`/\`match\`.
  - \`clippy::empty_line_after_doc_comments\`: remove the blank line, or change
    the section-divider \`///\` to a plain \`//\` comment.
  - \`clippy::cloned_ref_to_slice_refs\`: \`&[x.clone()]\` → \`std::slice::from_ref(&x)\`.
- Run \`cargo test --workspace\` yourself and fix every \`FAILED\` / compile
  error before stopping. If a test is genuinely flaky, document it in the
  journal; do not leave a red test tree.

Only stop once fmt + clippy + test are all green by your own hand.`

// provider 定义不再硬编码在 flow 里：从 ~/.flowcast/providers.* + <repo>/.flowcast/providers.* 加载（向后兼容 .flowx/）。
// 顶层 await（在 main() 前）拿到 map，buildEnv 同步消费。
const PROVIDERS = await loadProviders({ repo })

// flowcast 数据目录：新项目 .flowcast/，旧项目自动黏住 .flowx/（dirs.js 兜底）。
// 显式传 stateDir，避免依赖 process.cwd()（--repo 可能与 cwd 不同）。
const FC_DIR = flowcastDir(repo)                       // 绝对路径
const FC_REL = (relative(repo, FC_DIR) || '.flowcast') // 仓内相对目录名（.flowcast / .flowx）
const cp = new Checkpoint(runId, join(FC_DIR, 'runs'))

// 续跑时从 pauseContext 恢复 goal
const goal = resolveGoal() ?? cp.getPauseContext().goal
if (!goal) { console.error('缺少 --goal 或 --goal-file'); process.exit(1) }

setWorkdir(repo)
configureHitl()

console.log(`\n▶ recursive-self-improve  run=${runId}  repo=${repo}  status=${cp.status}`)
console.log(`  goal: ${goal.slice(0, 80)}${goal.length > 80 ? '…' : ''}\n`)

// ── --commit-pending 快速补提交模式 ──────────────────────────────
// 专为「质量门全绿但 skip-commit（reviewer unavailable）」设计：
//   1. 跑完整质量门（含项目自定义门 e2e/tui，不止 cargo 三件套）确认工作树仍健康
//   2. 直接提交，不重跑 agent，不等 reviewer
// 用法：node self-improve.flow.js --run-id <old-id> --commit-pending
if (opts['commit-pending']) {
  await commitPending()
  process.exit(process.exitCode ?? 0)
}

// ── preserve 消费模式（g325）：从失败现场接着修 / 落地 / 清理 ──────────
if (opts['resume-preserve']) {
  await resumePreserve(opts['resume-preserve'])
  process.exit(process.exitCode ?? 0)
}
if (opts['land-preserve']) {
  await landPreserve(opts['land-preserve'])
  process.exit(process.exitCode ?? 0)
}
if (opts['prune-preserve']) {
  await prunePreserve(opts['prune-preserve'])
  process.exit(process.exitCode ?? 0)
}

await main()

// ── --commit-pending 补提交实现 ───────────────────────────────────

/**
 * 快速补提交：跳过 agent，直接对当前工作树改动跑完整质量门（含项目自定义门），
 * 全绿则提交。任一门红灯则保留改动、通知 + 写 failure context，不提交（exitCode=1）。
 *
 * 与主流程的差异：不再喂回 agent 修（没 agent 在跑），所以所有门统一 onFail='rollback'
 * 即「红灯即抛」；门链复用 builtin + .flowcast/gates.json，覆盖 e2e/tui 等硬门。
 */
async function commitPending() {
  const diff = execFileSync('git', ['-C', repo, 'diff', '--stat', 'HEAD'], { encoding: 'utf8' }).trim()
  if (!diff) { console.log('工作树无改动，无需补提交。'); return }
  console.log(`[commit-pending] 检测到未提交改动：\n${diff}\n`)
  console.log('[commit-pending] 跑完整质量门验证（含项目自定义门）…')
  try {
    const builtin = qualityGatesFor(repo)
    const projectGates = await loadGates({ repo })
    const gates = mergeGates(builtin, projectGates).map(g => ({ cwd: repo, ...g }))
    for (const g of gates) {
      // commit-pending 不喂回 agent：任何门红灯直接抛错，改动保留待人处理
      await cp.step(`commit-pending.gate.${g.name}`, () =>
        runGate({ ...g, onFail: 'rollback' }, { resumeFix: null }),
      )
    }
  } catch (err) {
    const msg = `[commit-pending] 质量门红灯（${err.gate ?? 'unknown'}）：${err.message}\n改动未提交，请手动修复后重试。`
    console.error(msg)
    writeFailureContext(cp.dir, 'recursive', {
      reason: `commit-pending gate '${err.gate}' failed`, tailLog: (err.output ?? '').slice(-2000),
      provider: opts.provider, model: opts.model,
    })
    await notify(`❌ recursive self-improve commit-pending 失败\n仓库: ${repo}\n${msg}\nrun: ${cp.dir}`)
    process.exitCode = 1
    return
  }
  console.log('[commit-pending] 质量门全绿，提交改动…')
  execFileSync('git', ['-C', repo, 'add', '-A'])
  execFileSync('git', ['-C', repo, 'commit', '-m', `self-improve: ${goalSubject()} [commit-pending]`])
  const sha = execFileSync('git', ['-C', repo, 'rev-parse', '--short', 'HEAD'], { encoding: 'utf8' }).trim()
  console.log(`[commit-pending] ✅ 已提交 ${sha}`)
  await notify(`✅ recursive self-improve commit-pending 落地\n仓库: ${repo}\n提交: ${sha}\ngoal: ${goal.slice(0, 80)}`)
}

// ── 失败现场保留（g325）──────────────────────────────────────────
//
// 把失败时的 worktree 现场保留下来，供人 / 更强 agent 经 --resume-preserve / --land-preserve
// 消费。代替旧实现的「硬回滚丢 worktree」。保留三样：
//   1. refs/preserve/<run-id> 分支      —— 完整代码状态（哪怕测试红也先 commit）
//   2. <run-dir>/preserved.diff          —— baseline..HEAD 的可读 diff（grep 友好）
//   3. <run-dir>/<tag>-failure.log       —— 完整失败输出（门禁 stderr / 评审意见 / panic tail）
// worktree 本体能挪就挪到 .worktrees/preserve/<run-id>/（保留热构建缓存）；挪不动（被锁）
// 就原地保留，main() 的 finally 对 *-preserved verdict 跳过 cleanupWt。

function preserveScene({ worktreeDir, baseline, reason, failureOutput, tag = 'fail', verdict = 'failed-preserved' }) {
  writeFailureContext(cp.dir, 'recursive', {
    reason, tailLog: (failureOutput ?? '').slice(-2000), provider: opts.provider, model: opts.model,
  })
  // ① worktree 内提交 WIP（哪怕测试红）——拿到完整代码状态
  try { git(['add', '-A'], worktreeDir) } catch { /* 无改动也能 commit 下一行会失败，忽略后用 HEAD */ }
  let wtSha
  try {
    git(['commit', '-m', `preserve: ${reason}`], worktreeDir)
    wtSha = git(['rev-parse', 'HEAD'], worktreeDir)
  } catch { wtSha = git(['rev-parse', 'HEAD'], worktreeDir) }
  // ② 打 preserve 分支（ref，不占 worktree slot，可被多处引用）
  const ref = `refs/preserve/${runId}`
  try { git(['update-ref', ref, wtSha], repo) } catch (e) { console.warn(`  [preserve] update-ref failed: ${e.message}`) }
  // ③ 导出 diff + 完整失败日志到 run 目录（持久，即使 worktree 被手动删也还在）
  try { writeFileSync(join(cp.dir, 'preserved.diff'), git(['diff', `${baseline}..${wtSha}`], repo) || '') } catch { /* baseline 不可达时忽略 */ }
  try { writeFileSync(join(cp.dir, `${tag}-failure.log`), String(failureOutput ?? '')) } catch { /* best-effort */ }
  // ④ worktree 挪到 preserve 命名空间（与活跃 self-improve worktree 分开，不污染新 run）
  let preserveWt = worktreeDir
  const target = join(PRESERVE_DIR, runId)
  try {
    execFileSync('mkdir', ['-p', PRESERVE_DIR], { encoding: 'utf8' })
    git(['worktree', 'move', worktreeDir, target], repo)
    preserveWt = target
  } catch (e) {
    // 挪不动（子进程锁文件 / worktree 脏）：原地保留，ref+diff+log 仍是后盾
    console.warn(`  [preserve] worktree move failed, kept in place: ${e.message}`)
  }
  const detail = `${reason}\n  ref: ${ref}\n  worktree: ${preserveWt}\n  diff: ${join(cp.dir, 'preserved.diff')}\n  failure: ${join(cp.dir, `${tag}-failure.log`)}`
  return { verdict, detail, preserve: { ref, worktree: preserveWt } }
}

// ── preserve 消费命令 ─────────────────────────────────────────────

/** 从 refs/preserve/<run-id> 接着修：用该现场做 worktree，注入失败上下文，跑 fixer + 门禁 + 评审。 */
async function resumePreserve(preserveRunId) {
  const ref = `refs/preserve/${preserveRunId}`
  let sha
  try { sha = git(['rev-parse', ref], repo) } catch { throw new Error(`preserve ref 不存在: ${ref}`) }
  const runDir = join(FC_DIR, 'runs', preserveRunId)
  const origGoal = readRunGoal(preserveRunId) ?? goal
  const failureLog = readFailureLog(runDir)
  const resumeGoal =
    `A previous self-improve attempt was preserved at this state (it did NOT pass gates/review).\n` +
    `Continue from the current worktree state — do NOT start over. The prior code is already on disk.\n\n` +
    `--- original goal ---\n${origGoal}\n\n--- prior failure context ---\n${failureLog ?? '(see <run-dir>/*-failure.log)'}\n\n` +
    `Finish the goal, fix the prior failure, run \`cargo test --workspace\` / \`cargo clippy --all-targets --all-features -- -D warnings\` / \`cargo fmt --all -- --check\` yourself and ensure they pass before stopping.`
  const newRunId = `resume-${preserveRunId}-${Date.now()}`
  const wtDir = join(repo, '.worktrees', newRunId)
  await cp.step('resume.worktree', () => {
    execFileSync('mkdir', ['-p', join(repo, '.worktrees')], { encoding: 'utf8' })
    gitWorktreeAdd(repo, wtDir, { ref: sha })
  })
  const baseline = git(['rev-parse', 'HEAD'], repo)
  const sysPromptFile = buildSystemPrompt()
  const transcriptOut = join(cp.dir, 'transcript.json')
  let result
  try {
    result = await runAttemptWithGoal({ sysPromptFile, transcriptOut, baseline, worktreeDir: wtDir, goalOverride: resumeGoal })
  } catch (err) {
    result = preserveScene({ worktreeDir: wtDir, baseline, reason: `resume attempt error: ${err.message}`, failureOutput: String(err.stack ?? err), tag: 'resume-error' })
  } finally {
    if (result?.verdict !== 'failed-preserved' && result?.verdict !== 'panic-preserved') {
      try { gitWorktreeRemove(repo, wtDir) } catch { /* already gone */ }
    }
  }
  await announce(result, baseline)
  console.log(`\n✓ resume-preserve 结束  verdict=${result.verdict}`)
}

/** 把 preserve 现场跑门后落地到 main：对 refs/preserve/<run-id> 的树跑完整门，全绿则提交。 */
async function landPreserve(preserveRunId) {
  const ref = `refs/preserve/${preserveRunId}`
  let sha
  try { sha = git(['rev-parse', ref], repo) } catch { throw new Error(`preserve ref 不存在: ${ref}`) }
  console.log(`[land-preserve] ${ref} -> ${sha.slice(0, 8)}，跑完整质量门验证…`)
  const wtDir = join(repo, '.worktrees', `land-${preserveRunId}`)
  try {
    gitWorktreeAdd(repo, wtDir, { ref: sha })
    const builtin = qualityGatesFor(wtDir)
    const projectGates = await loadGates({ repo })
    const gates = mergeGates(builtin, projectGates).map(g => ({ cwd: wtDir, onFail: 'rollback', ...g }))
    for (const g of gates) {
      await cp.step(`land.gate.${g.name}`, () => runGate(g, { resumeFix: null }))
    }
    // 全绿 → cherry-pick 到 main
    const mainHead = git(['rev-parse', 'HEAD'], repo)
    try { git(['cherry-pick', '--no-commit', sha], repo) } catch (err) {
      try { git(['cherry-pick', '--abort'], repo) } catch { /* 未进入 cherry-pick */ }
      throw new Error(`cherry-pick conflict landing ${sha.slice(0, 8)}: ${err.message}`)
    }
    git(['commit', '-m', `self-improve: ${goalSubject()} [land-preserve ${preserveRunId.slice(-6)}]`], repo)
    const landed = git(['rev-parse', '--short', 'HEAD'], repo)
    console.log(`[land-preserve] ✅ 已落地 ${landed}`)
    await notify(`✅ land-preserve 落地\n仓库: ${repo}\n提交: ${landed}\n来源: ${ref}`)
  } finally {
    try { gitWorktreeRemove(repo, wtDir) } catch { /* already gone */ }
  }
}

/** 清掉一个 preserve 现场：删 ref + 移除 preserve worktree（若有）。 */
async function prunePreserve(preserveRunId) {
  const ref = `refs/preserve/${preserveRunId}`
  let removed = []
  try { git(['update-ref', '-d', ref], repo); removed.push(ref) } catch { /* ref 不存在 */ }
  const wtDir = join(PRESERVE_DIR, preserveRunId)
  if (existsSync(wtDir)) {
    try { git(['worktree', 'remove', '--force', wtDir], repo); removed.push(wtDir) }
    catch (e) { console.warn(`  [prune-preserve] worktree remove failed: ${e.message}`) }
  }
  console.log(`[prune-preserve] 已清理: ${removed.join(', ') || '(无)'}`)
}

function readRunGoal(preserveRunId) {
  try {
    const s = JSON.parse(readFileSync(join(FC_DIR, 'runs', preserveRunId, 'state.json'), 'utf8'))
    return s.summary?.goal ?? null
  } catch { return null }
}

function readFailureLog(runDir) {
  if (!existsSync(runDir)) return null
  for (const f of readdirSync(runDir)) {
    if (f.endsWith('-failure.log')) {
      try { return readFileSync(join(runDir, f), 'utf8') } catch { /* skip */ }
    }
  }
  return null
}

/** runAttempt 的 resume 变体：允许覆盖 goal（--resume-preserve 注入失败上下文用）。 */
async function runAttemptWithGoal({ sysPromptFile, transcriptOut, baseline, worktreeDir, goalOverride }) {
  const env = buildEnv()
  const resolvedBin = opts.bin ?? join(repo, 'target', 'release', 'recursive')
  const base = () => ({ cwd: worktreeDir, workspace: '.', bin: resolvedBin, systemPromptFile: sysPromptFile, pricingFile: pricingFileOf(repo), env, onData: tee, timeout: RUN_TIMEOUT_MS })
  const g = goalOverride ?? goal
  const runMeta = await cp.step('run.recursive', async () => (await recursive(g, { ...base(), transcriptOut }))._meta)
  if (runMeta.panicked) return preserveScene({ worktreeDir, baseline, reason: `resume panic exit ${runMeta.exitCode}`, failureOutput: tailOf(transcriptOut), tag: 'resume-panic', verdict: 'panic-preserved' })
  if (gitClean(worktreeDir)) return { verdict: 'skip-commit', detail: 'no changes produced (resume)' }
  let latestTranscript = transcriptOut
  try {
    await runQualityGates({ sysPromptFile, transcriptOut: latestTranscript, env, worktreeDir, baseline })
  } catch (err) {
    return preserveScene({ worktreeDir, baseline, reason: err.reason ?? `resume gate '${err.gate}' failed after ${MAX_FIX_ROUNDS} fix rounds`, failureOutput: err.output ?? '', tag: `resume-gate-${err.gate}` })
  }
  if (!opts['no-review']) {
    for (let round = 0; round <= MAX_FIX_ROUNDS; round++) {
      const r = await cp.step(round === 0 ? 'review' : `review.fix-${round}`, () => reviewWithRetry(worktreeDir))
      if (r.decision === 'PASS' || r.decision === 'UNAVAILABLE') break
      if (round === MAX_FIX_ROUNDS) return preserveScene({ worktreeDir, baseline, reason: `resume self-review NEEDS_FIX after ${MAX_FIX_ROUNDS} fix rounds`, failureOutput: r.text, tag: 'resume-review' })
      latestTranscript = await runFixRound({ transcriptOut: latestTranscript, sysPromptFile, env, worktreeDir, fixGoal: `Reviewer feedback (resume):\n${r.text}\n\nAddress every issue.`, tag: 'review' })
    }
  }
  // 全绿 → cherry-pick 到 main
  await cp.step('commit', () => {
    git(['add', '-A'], worktreeDir)
    git(['commit', '-m', `wt: ${goalSubject()}`], worktreeDir)
    const wtSha = git(['rev-parse', 'HEAD'], worktreeDir)
    const mainHead = git(['rev-parse', 'HEAD'], repo)
    if (mainHead !== baseline) {
      throw new Error(`main checkout moved since baseline (baseline=${baseline.slice(0, 8)} HEAD=${mainHead.slice(0, 8)}); refusing to cherry-pick. Worktree commit: ${wtSha.slice(0, 8)}`)
    }
    try { git(['cherry-pick', '--no-commit', wtSha], repo) } catch (err) {
      try { git(['cherry-pick', '--abort'], repo) } catch { /* */ }
      throw new Error(`cherry-pick conflict landing ${wtSha.slice(0, 8)}: ${err.message}`)
    }
    git(['commit', '-m', `self-improve: ${goalSubject()} [resume-preserve]`], repo)
    return git(['rev-parse', 'HEAD'], repo)
  })
  return { verdict: 'committed' }
}

// ── 主流程 ───────────────────────────────────────────────────────

async function main() {
  // 只本地排除「运行产物」子目录 <FC>/runs/ 与 .worktrees/，不排除整个 .flowcast/——
  // 因为项目配置（providers/agents/gates.json）就放在 .flowcast/ 下且应 committed。
  // 排除 runs/ 既避免 run 产物污染 clean 检查，又不挡住配置文件入仓；
  // 排除 .worktrees/ 让 worktree 目录不污染 main checkout 的 clean 预检。
  ensureGitExclude(repo, FC_REL + '/runs/')
  ensureGitExclude(repo, '.worktrees/')

  // ── 殭尸进程清理：杀掉本 repo 下挂起的旧 recursive 进程 ─────────
  killStaleRecursiveProcs(repo, runId)

  // ── 预检：捕获 baseline（持久化，续跑复用同一 baseline）──────────
  const baseline = await cp.step('preflight.baseline', () =>
    captureBaseline(repo, { requireClean: true }),
  )
  console.log(`  baseline: ${baseline}`)

  // ── 预编译最新二进制（确保 agent 用到的是最新代码编译出的版本）────
  // 裸 `cargo build --release` 在 workspace 根只构建根包 (recursive-agent lib)，
  // 不构建 recursive-cli 的 `recursive` bin —— flow 下游 spawn 的正是该 bin。
  // 用 -p recursive-cli 显式构建执行器二进制及其依赖。
  await cp.step('preflight.build', () => {
    console.log('  [preflight.build] cargo build --release -p recursive-cli ...')
    execFileSync('cargo', ['build', '--release', '-p', 'recursive-cli'], { cwd: repo, stdio: 'inherit' })
    console.log('  [preflight.build] ✓ done')
  })

  // ── 创建隔离 worktree（agent 只在 worktree 内改动，main checkout 保持干净）─
  const worktreeDir = join(repo, '.worktrees', runId)
  await cp.step('preflight.worktree', () => {
    gitWorktreeAdd(repo, worktreeDir)
    console.log(`  [preflight.worktree] created ${worktreeDir}`)
  })
  // 续跑时 worktree 可能已被清理：幂等重建
  if (!existsSync(worktreeDir)) {
    gitWorktreeAdd(repo, worktreeDir)
    console.log(`  [preflight.worktree] re-created ${worktreeDir} (resume)`)
  }

  // 注册退出清理钩子（正常退出 / SIGINT / SIGTERM 均清理 worktree）。
  // 用具名引用统一注册/注销，避免旧代码里 exit 与信号 handler 各自 cleanupWt
  // 导致重复清理，以及 runAttempt 后只 remove exit handler、信号 handler 仍挂着
  // 指向已清理 worktree 的闭包泄漏。
  const cleanupWt = () => {
    try { gitWorktreeRemove(repo, worktreeDir) } catch { /* already gone */ }
  }
  const onSigint = () => { cleanupWt(); process.exitCode = 130 }
  const onSigterm = () => { cleanupWt(); process.exitCode = 143 }
  process.once('exit', cleanupWt)
  process.once('SIGINT', onSigint)
  process.once('SIGTERM', onSigterm)
  const unregisterCleanup = () => {
    process.removeListener('exit', cleanupWt)
    process.removeListener('SIGINT', onSigint)
    process.removeListener('SIGTERM', onSigterm)
  }

  // ── 构建 system prompt（注入契约 + journal + 上次失败上下文）─────
  const sysPromptFile = await cp.step('preflight.system-prompt', () =>
    buildSystemPrompt(),
  )

  // ── 预检：provider API 健康探测（快速失败，避免 agent 挂死数分钟）─
  await cp.step('preflight.provider-ping', () => pingProvider(buildEnv()))

  // ── 自改安全沙箱：整个尝试在 worktree 内执行，通过后 cherry-pick 回 main ──
  // worktree 本身就是沙箱，agent 改动隔离在 worktreeDir，main checkout 全程不被触碰
  // （cherry-pick 只在 committed 路径发生）。失败不再硬回滚丢 worktree：failed-preserved
  // 时 preserveScene 把 worktree 挪到 .worktrees/preserve/<run-id>/ 并打 refs/preserve/<run-id>
  // 分支 + 导出 diff/failure.log，供 --resume-preserve / --land-preserve 消费。
  // cherry-pick 前显式校验 main 未移动、冲突时 abort 而非 reset（绝不吃掉别人的提交）。
  const transcriptOut = join(cp.dir, 'transcript.json')
  let result
  try {
    result = await runAttempt({ sysPromptFile, transcriptOut, baseline, worktreeDir })
  } catch (err) {
    // 基础设施级失败（spawn 崩、gate 脚手架异常等）：尽力保留现场，退而求其次才回滚
    try {
      result = preserveScene({ worktreeDir, baseline, reason: `attempt error: ${err.message}`, failureOutput: String(err.stack ?? err) })
    } catch (pErr) {
      writeFailureContext(cp.dir, 'recursive', {
        reason: `attempt error + preserve failed: ${err.message} / ${pErr.message}`, tailLog: String(err.stack ?? err).slice(-2000),
        provider: opts.provider, model: opts.model,
      })
      result = { verdict: 'rolled-back', detail: `${err.message} (preserve failed: ${pErr.message})` }
    }
  } finally {
    // 只对「不留现场」的 verdict 清 worktree；failed-preserved / panic-preserved 的 worktree
    // 已由 preserveScene 挪到 .worktrees/preserve/<run-id>/，这里不能再删。
    if (result?.verdict !== 'failed-preserved' && result?.verdict !== 'panic-preserved') {
      cleanupWt()
    }
    unregisterCleanup()
  }

  // ── 收尾：metrics + 报告 + 落地指针 / 升级通知 ──────────────────
  const metrics = computeMetrics(baseline, result)
  cp.done({ goal: goal.slice(0, 120), verdict: result.verdict, ...metrics })

  await announce(result, baseline)
  console.log(`\n✓ recursive-self-improve 结束  verdict=${result.verdict}`)
}

/**
 * 单次尝试：跑 recursive → budget resume → 质量门（N 轮 fix loop）→ review（N 轮 fix loop）→ verdict。
 * 注意：本函数不在 withSelfModGuard 内（worktree 即沙箱）。返回 verdict 对象，
 * main() 据此收尾——非 committed 且非 *-preserved 时丢弃 worktree；preserved 时 worktree
 * 已由 preserveScene 挪到 .worktrees/preserve/<run-id>/，main() 不再清。
 *
 * 失败不再硬回滚：门禁/评审循环耗尽 → failed-preserved（保留 worktree+分支+diff+stderr）。
 * panic → panic-preserved（同样保留现场，升级旧实现：不只保 transcript，也保代码）。
 */
async function runAttempt({ sysPromptFile, transcriptOut, baseline, worktreeDir }) {
  const env = buildEnv() // provider 配置经 env 注入（RECURSIVE_PROVIDER_TYPE/API_BASE/MODEL/API_KEY）
  // recursive 二进制固定用 main repo 编译的产物（preflight.build 已确保最新）
  const resolvedBin = opts.bin ?? join(repo, 'target', 'release', 'recursive')
  // recursive 调用的公共选项（pricing / system-prompt / 流式输出 / 硬超时）
  // cwd 指向 worktreeDir，让 agent 在隔离目录内读写文件
  const base = () => ({
    cwd: worktreeDir, workspace: '.', bin: resolvedBin, systemPromptFile: sysPromptFile,
    pricingFile: pricingFileOf(repo), env, onData: tee, timeout: RUN_TIMEOUT_MS,
  })

  // ① 跑 recursive 二进制
  const runMeta = await cp.step('run.recursive', async () => {
    const out = await recursive(goal, { ...base(), transcriptOut })
    return out._meta
  })

  // panic：保留现场（代码 + transcript）不回滚，留作诊断 / 接续修
  if (runMeta.panicked) {
    writeFailureContext(cp.dir, 'recursive', {
      reason: 'panic', tailLog: tailOf(transcriptOut), provider: opts.provider, model: opts.model,
    })
    return preserveScene({ worktreeDir, baseline, reason: `panic exit ${runMeta.exitCode}`, failureOutput: tailOf(transcriptOut), tag: 'panic' })
  }

  // ② BudgetExceeded → 自动 resume 一次（写独立 transcript，避免覆盖被 replay 的源）
  // latestTranscript 跟踪「最近一次成功的 transcript 路径」，后续质量门的 resume-fix
  // 必须从它 replay——否则发生过 budget resume 后，resume-fix 会从 resume 之前的 transcript
  // 重放，丢失 resume 阶段全部 tool call，agent 在「忘了刚做啥」的状态下修 bug 必败。
  // 超时（timedOut）也走这条路径：agent 被杀在半路，但半成品已在 worktree 落盘，
  // resume-fix 轮会从 on-disk diff 接着修（transcript 为空时降级走 diff 上下文，见 runFixRound）。
  let lastMeta = runMeta
  let latestTranscript = transcriptOut
  if (runMeta.budgetExceeded || runMeta.timedOut) {
    const resumedTranscript = transcriptOut.replace(/\.json$/, '-resumed.json')
    lastMeta = await cp.step('run.recursive.resume', async () => {
      // transcript 为空（超时未 flush）时不能 replay 空 transcript——replayFrom 传 0
      // 会让 recursive 从空状态起步；此时走 fresh run，靠 worktree on-disk 状态续修。
      const replayFrom = runMeta.transcriptMessages > 0
        ? { transcript: transcriptOut, resumeFrom: runMeta.transcriptMessages }
        : undefined
      const out = await recursive(goal, {
        ...base(), transcriptOut: resumedTranscript,
        ...(replayFrom ? { replayFrom } : {}),
      })
      return out._meta
    })
    latestTranscript = resumedTranscript
    if (lastMeta.panicked) {
      writeFailureContext(cp.dir, 'recursive', {
        reason: 'panic (after resume)', tailLog: tailOf(resumedTranscript),
        provider: opts.provider, model: opts.model,
      })
      return preserveScene({ worktreeDir, baseline, reason: `panic (after resume) exit ${lastMeta.exitCode}`, failureOutput: tailOf(resumedTranscript), tag: 'panic' })
    }
    if (lastMeta.timedOut) {
      // resume 仍超时：不再无限续，保留现场给人/更强 agent 接手
      writeFailureContext(cp.dir, 'recursive', { reason: 'timeout (after resume)', tailLog: tailOf(resumedTranscript) })
      return preserveScene({ worktreeDir, baseline, reason: 'timeout (after resume)', failureOutput: tailOf(resumedTranscript), tag: 'timeout' })
    }
    if (lastMeta.budgetExceeded) {
      writeFailureContext(cp.dir, 'recursive', { reason: 'BudgetExceeded (after resume)', tailLog: tailOf(resumedTranscript) })
      return { verdict: 'skip-commit', detail: 'budget exceeded after one resume' }
    }
  }

  // 若 recursive 没产生任何改动（worktree 干净），跳过提交
  if (gitClean(worktreeDir)) {
    return { verdict: 'skip-commit', detail: 'no changes produced' }
  }

  // ③ 质量门：test / clippy / fmt（+ 可选 e2e），每个门带 N 轮 resume-fix 循环。
  // 所有门在 worktreeDir 内执行，保证测试的是 agent 实际修改的代码。
  // resume-fix 链式 replay 上一轮 fix-transcript（见 runFixRound），保留全部上下文。
  // N 轮仍红 → failed-preserved（不回滚，保留现场）。
  try {
    await runQualityGates({ sysPromptFile, transcriptOut: latestTranscript, env, worktreeDir, baseline })
  } catch (err) {
    // 优先用 runQualityGates 给的细粒度原因（如 "agent made no edits in fix round N"），
    // 否则退回通用 "N 轮仍红"。让 preserve 现场更有诊断价值。
    const reason = err.reason ?? `quality gate '${err.gate}' failed after ${MAX_FIX_ROUNDS} fix rounds`
    writeFailureContext(cp.dir, 'recursive', {
      reason,
      tailLog: (err.output ?? '').slice(-2000),
      provider: opts.provider, model: opts.model,
    })
    return preserveScene({
      worktreeDir, baseline, reason,
      failureOutput: err.output ?? '', tag: `gate-${err.gate}`,
    })
  }

  // ④ 跨 provider self-review（区分「明确 NEEDS_FIX」与「reviewer 不可用 / 未配置」）
  // NEEDS_FIX 不再直接回滚：把评审意见喂回 agent 修，N 轮循环；仍 NEEDS_FIX 才 preserve。
  let reviewFixRan = false
  if (!opts['no-review']) {
    let reviewText = ''
    let reviewDecision = 'PASS'
    let reviewMisconfig = false
    for (let round = 0; round <= MAX_FIX_ROUNDS; round++) {
      const r = await cp.step(round === 0 ? 'review' : `review.fix-${round}`, () => reviewWithRetry(worktreeDir))
      reviewText = r.text; reviewMisconfig = r.misconfig
      if (r.decision === 'PASS' || r.decision === 'UNAVAILABLE') { reviewDecision = r.decision; break }
      // NEEDS_FIX：还有轮次就喂回 agent 修，否则 preserve
      if (round === MAX_FIX_ROUNDS) { reviewDecision = 'NEEDS_FIX'; break }
      latestTranscript = await runFixRound({
        transcriptOut: latestTranscript, sysPromptFile, env, worktreeDir,
        fixGoal: `An independent reviewer rejected this change with NEEDS_FIX. Address every issue below. Do not regress passing checks.\n\n--- reviewer feedback ---\n${r.text}`,
      })
      reviewFixRan = true
    }
    if (reviewDecision === 'NEEDS_FIX') {
      writeFailureContext(cp.dir, 'recursive', { reason: `self-review NEEDS_FIX after ${MAX_FIX_ROUNDS} fix rounds`, tailLog: reviewText.slice(-2000) })
      return preserveScene({ worktreeDir, baseline, reason: `self-review NEEDS_FIX after ${MAX_FIX_ROUNDS} fix rounds`, failureOutput: reviewText, tag: 'review' })
    }
    if (reviewDecision === 'UNAVAILABLE') {
      // reviewer 多次报错（网络/quota）或未配置：质量门已全绿，直接提交并通知。
      // 理由：所有质量门（cargo test/clippy/fmt + 项目门）均已通过，代码可靠性已验证，
      //       reviewer 仅是可选的二次确认层，其不可用不应让成果丢失。
      // 区分「未配置」（建议加 --reviewer-provider 或 --no-review）与「网络 down」，便于排查。
      const tag = reviewMisconfig
        ? 'reviewer 未配置（建议加 --reviewer-provider 或 --no-review 显式跳过）'
        : 'reviewer 不可用（网络/quota）'
      await notify(`ℹ️ self-review ${tag}但质量门全绿，自动提交改动。\nrun: ${cp.dir}`)
    }
  }

  // ④b 评审修过代码 → 重跑门禁确认没回归（评审 fix 可能动了测试涉及的代码）。
  // 没跑过评审 fix（round 0 即 PASS/UNAVAILABLE）则跳过，避免无谓的二次 cargo test。
  if (reviewFixRan) {
    try {
      await runQualityGates({ sysPromptFile, transcriptOut: latestTranscript, env, worktreeDir, baseline })
    } catch (err) {
      writeFailureContext(cp.dir, 'recursive', {
        reason: `quality gate '${err.gate}' regressed after review fix`,
        tailLog: (err.output ?? '').slice(-2000), provider: opts.provider, model: opts.model,
      })
      return preserveScene({ worktreeDir, baseline, reason: `quality gate '${err.gate}' regressed after review fix`, failureOutput: err.output ?? '', tag: `gate-after-review-${err.gate}` })
    }
  }

  // ⑤ 全绿 → 将 worktree 改动 cherry-pick 回 main checkout，再统一提交
  if (opts['no-commit']) return { verdict: 'skip-commit', detail: '--no-commit' }
  await cp.step('commit', () => {
    // 在 worktree 创建一个临时提交，包含 agent 的所有改动（含新增文件）
    git(['add', '-A'], worktreeDir)
    git(['commit', '-m', `wt: ${goalSubject()}`], worktreeDir)
    const wtSha = git(['rev-parse', 'HEAD'], worktreeDir)
    // 安全网：cherry-pick 前确认 main checkout 没在 baseline 之后被推进过
    // （并发 admin merge / 另一个 run 已落地）。若被推进，直接拒绝落地——
    // 绝不 reset --hard baseline（那会吃掉别人的提交）。worktree 提交已在 wtSha 保留。
    const mainHead = git(['rev-parse', 'HEAD'], repo)
    if (mainHead !== baseline) {
      throw new Error(
        `main checkout moved since baseline (baseline=${baseline.slice(0, 8)} HEAD=${mainHead.slice(0, 8)}); ` +
        `refusing to cherry-pick to avoid clobbering unrelated commits. Worktree commit: ${wtSha.slice(0, 8)}`,
      )
    }
    // cherry-pick 到 main checkout（--no-commit 保留暂存区，由我们写最终 message）；
    // 冲突时 abort 进行中的 cherry-pick 保持 repo 索引/工作树干净，再上抛（不 reset）。
    try {
      git(['cherry-pick', '--no-commit', wtSha], repo)
    } catch (err) {
      try { git(['cherry-pick', '--abort'], repo) } catch { /* 未进入 cherry-pick 状态 */ }
      throw new Error(`cherry-pick conflict landing ${wtSha.slice(0, 8)}: ${err.message}`)
    }
    git(['commit', '-m', `self-improve: ${goalSubject()}`], repo)
    return git(['rev-parse', 'HEAD'], repo)
  })
  return { verdict: 'committed' }
}

// ── 质量门（N 轮 resume-fix 循环）─────────────────────────────────

/**
 * 跑每个门，红灯就走 resume-fix 循环：喂最新 stderr → agent 修 → 重跑门，最多 MAX_FIX_ROUNDS 轮。
 * 旧实现靠 flowcast runGate 内置的「1 轮 resume-fix」——1 轮修不过就抛错回滚，丢全部成果。
 * 现在改成 flow 自己控循环：runGate 用 onFail='rollback'（纯检查，红灯即抛），catch 后跑 runFixRound，
 * 链式 replay 上一轮 fix-transcript，再重跑。N 轮仍红 → 抛带 .exhausted 的错，让 runAttempt preserve。
 *
 * 关键：每轮喂「最新」stderr（不是首轮的）——agent 上一轮可能修了一部分，新一轮错误已变。
 * 链式 replay：第 k 轮 fix 从第 k-1 轮的 fix-transcript replay，agent 记得自己上轮试过啥，不重复踩。
 */
async function runQualityGates({ sysPromptFile, transcriptOut, env, worktreeDir, baseline }) {
  let curTranscript = transcriptOut
  const builtin = qualityGatesFor(worktreeDir)
  const projectGates = await loadGates({ repo })
  // 项目门未显式写 cwd 时默认在 worktreeDir 跑；写了则尊重项目声明。
  // onFail 统一 'rollback'：runGate 只做「跑 + 红灯抛错」，循环由本函数控。
  const gates = mergeGates(builtin, projectGates).map(g => ({ cwd: worktreeDir, onFail: 'rollback', ...g }))

  for (const g of gates) {
    let lastOutput = ''
    for (let attempt = 0; attempt <= MAX_FIX_ROUNDS; attempt++) {
      try {
        await cp.step(attempt === 0 ? `gate.${g.name}` : `gate.${g.name}.fix-${attempt}`, () =>
          runGate(g, { resumeFix: null }),
        )
        break // 本门绿 → 下一道门
      } catch (err) {
        if (err.configError) throw err // autofix 缺 autofixCmd 等配置错，不修，直接抛
        lastOutput = err.output ?? ''
        if (attempt === MAX_FIX_ROUNDS) {
          // N 轮仍红：带上最新 stderr 上抛，由 runAttempt 转 failed-preserved
          err.exhausted = true
          err.output = lastOutput
          throw err
        }
        // 红灯 → 喂「可操作的错误清单」让 agent 修一轮，链式 replay，再回到 for 顶端重跑本门
        const fixGoal = buildFixGoal({ gate: g, output: lastOutput, attempt, worktreeDir })
        curTranscript = await runFixRound({
          transcriptOut: curTranscript, sysPromptFile, env, worktreeDir,
          fixGoal, tag: g.name,
        })
        // 防空转：agent 跑了一轮但工作树没任何改动 → 多半是没真去编辑（弱模型常见），
        // 再喂同样的 stderr 也修不动。提前 break 并标注原因，让 preserve 现场更有诊断价值。
        if (worktreeDirty(worktreeDir) === false) {
          err.exhausted = true
          err.output = lastOutput
          err.reason = `agent made no edits in fix round ${attempt + 1} (likely could not act on ${g.name} output)`
          throw err
        }
      }
    }
  }
}

/**
 * 构造喂给 fixer agent 的 fix goal：把门禁失败输出变成「可操作的错误清单」。
 *
 * 旧实现只 `slice(-3000)` 喂尾部，但 cargo/clippy 的 `error:` 行往往在**头部**，
 * 靠前的 lint（doc-comment、from_ref 等）会被截掉，agent 看不全 → 修不掉。
 * 现在双管齐下：
 *   1. 把完整输出写到 worktree 内的 `.gate-<name>-output.log`，让 agent 用 Read 工具按需看全文；
 *   2. 内联抽取 `error:`/`warning:` + `--> file:line:col` 行（编译器/clippy 的可操作信号），
 *      截到 ~6KB，确保 agent 第一眼就拿到全部出错点；
 *   3. 附门禁专属修复提示（clippy 的 unwrap/空行/from_ref 套路），降低弱模型翻车率。
 */
function buildFixGoal({ gate, output, attempt, worktreeDir }) {
  const logPath = join(worktreeDir, `.gate-${gate.name}-output.log`)
  try { writeFileSync(logPath, output) } catch { /* 只读 worktree 等场景兜底，内联仍有 */ }

  const lines = output.split('\n')
  const actionable = lines
    .filter(l => /^\s*(error|warning)(\[|:)/.test(l) || /^\s*-->\s/.test(l) || /^error:/.test(l))
    .join('\n')
    .slice(0, 6000)

  const hint = GATE_FIX_HINTS[gate.name] ?? ''

  return [
    `The "${gate.name}" check failed (fix round ${attempt + 1}/${MAX_FIX_ROUNDS}).`,
    `Edit the source files to fix every error below, then re-run \`${gate.cmd}\` yourself to verify before stopping.`,
    hint ? `\n${hint}` : '',
    `\n--- actionable error lines ---\n${actionable || '(see full log)'}`,
    `\n--- full check output ---`,
    `Read the file \`.gate-${gate.name}-output.log\` in the worktree for the complete output (incl. notes/help).`,
  ].join('\n')
}

/** 门禁专属修复提示，给弱模型一条明确路径。 */
const GATE_FIX_HINTS = {
  clippy: [
    'These are clippy lints. Fix the SOURCE, never silence with `#[allow]`.',
    '- `clippy::unwrap_used` on `Mutex::lock()`: `.lock().unwrap()` → `.lock().unwrap_or_else(|e| e.into_inner())` (poison recovery; also satisfies invariant #5 — no unwrap in product code).',
    '- `clippy::expect_used`: same — recover or propagate via `?`/`match`.',
    '- `clippy::empty_line_after_doc_comments`: remove the blank line, or change the section-divider `///` to a plain `//` comment.',
    '- `clippy::cloned_ref_to_slice_refs`: `&[x.clone()]` → `std::slice::from_ref(&x)`.',
    'Each `--> file:line:col` above is one lint site. Fix them all in one pass, then re-run clippy.',
  ].join('\n'),
  test: [
    'These are compile/test failures. Read each `error[...]` / `--> file:line` and the `note:` below it.',
    'If a doctest fails to compile (e.g. `missing field`), a struct gained a field — update the doctest example to include it.',
    'Fix the source (or the test if it is wrong), then re-run `cargo test --workspace`.',
  ].join('\n'),
}

/** worktree 是否有未提交改动（fix 轮防空转用）。fileNotFound / git 出错时返回 true（保守不 break）。 */
function worktreeDirty(worktreeDir) {
  try {
    const out = git(['status', '--porcelain'], worktreeDir)
    return out.trim().length > 0
  } catch { return true }
}

/**
 * 跑一轮 resume-fix：把失败上下文喂回 recursive 修，在 worktree 内续跑。
 * 链式 replay：从传入的 transcriptOut replay（它可能是上一轮 fix 的 transcript），
 * agent 记得自己上轮做过啥。transcript 为空（超时未 flush 等场景）时降级走 fresh run，
 * 靠 worktree on-disk 状态续修——返回新 fix-transcript 路径供下一轮链式 replay。
 *
 * fixer-provider 可覆盖（--fixer-provider）：用更强 model 修主 agent 留下的烂摊子。
 */
async function runFixRound({ transcriptOut, sysPromptFile, env, worktreeDir, fixGoal, tag = 'fix' }) {
  const resolvedBin = opts.bin ?? join(repo, 'target', 'release', 'recursive')
  const fixTranscript = transcriptOut.replace(/\.json$/, `-${tag}.json`)
  const fixEnv = opts['fixer-provider'] ? buildEnv(opts['fixer-provider']) : env
  const msgCount = transcriptMessagesOf(transcriptOut)
  const replayFrom = msgCount > 0
    ? { transcript: transcriptOut, resumeFrom: msgCount }
    : undefined
  await recursive(fixGoal, {
    cwd: worktreeDir, workspace: '.', bin: resolvedBin, systemPromptFile: sysPromptFile,
    transcriptOut: fixTranscript, pricingFile: pricingFileOf(repo), env: fixEnv, onData: tee,
    timeout: RUN_TIMEOUT_MS,
    ...(replayFrom ? { replayFrom } : {}),
  })
  return fixTranscript
}

/** recursive（Rust）内置默认质量门。项目特定门（含 E2E）走 <repo>/.flowcast/gates.json。 */
function qualityGatesFor(repoPath) {
  return [
    { name: 'test',   cmd: 'cargo test --quiet',                                       cwd: repoPath, timeout: 1_200_000 },
    { name: 'clippy', cmd: 'cargo clippy --all-targets --all-features -- -D warnings', cwd: repoPath, timeout: 600_000 },
    { name: 'fmt',    cmd: 'cargo fmt --all -- --check',                               cwd: repoPath, onFail: 'autofix', autofixCmd: 'cargo fmt --all' },
  ]
}

// ── 跨 provider self-review ──────────────────────────────────────

/**
 * 跨 provider self-review，带重试与「不可用 / 未配置」区分。
 * @returns {{decision:'PASS'|'NEEDS_FIX'|'UNAVAILABLE', text:string, misconfig?:boolean}}
 *   - PASS         reviewer 明确通过
 *   - NEEDS_FIX    reviewer 明确否决（VERDICT:NEEDS_FIX）
 *   - UNAVAILABLE  reviewer 多次调用出错（网络/退出码非 0）、或正常返回但始终无 verdict
 *                  （如 reviewer 啰嗦/超步数没收尾）、或 reviewer-provider 未配置 → 不丢弃成果
 *
 * 设计取舍：reviewer 正常返回却没按格式给 VERDICT 时，旧代码「保守判否」直接 NEEDS_FIX
 * 会误回滚（reviewer 跑超 BudgetExceeded、exitCode 仍 0 但无 verdict 的场景）。
 * 现改为重试，仍无 verdict 才归 UNAVAILABLE（不丢弃成果），把「reviewer 没遵循格式」
 * 与「代码真的有问题」分开。
 */
async function reviewWithRetry(worktreeDir, maxAttempts = 2) {
  let lastText = ''
  let misconfig = false
  for (let i = 1; i <= maxAttempts; i++) {
    const r = await selfReview(worktreeDir)
    lastText = r.text
    if (r.misconfig) { misconfig = true; break }       // 配置缺失：不重试，直接 UNAVAILABLE
    if (/VERDICT:\s*PASS/.test(r.text)) return { decision: 'PASS', text: r.text }
    if (/VERDICT:\s*NEEDS_FIX/.test(r.text)) return { decision: 'NEEDS_FIX', text: r.text }
    if (r.ok) continue                                  // ok 但无 verdict → 再给一次机会
    // reviewer 调用本身出错（网络/退出码）→ 继续重试
  }
  return { decision: 'UNAVAILABLE', text: lastText, misconfig }
}

async function selfReview(worktreeDir) {
  // 给 reviewer 完整 diff + 改动文件清单，避免旧实现 gitDiff(...).slice(0, 20_000) 截断 diff
  // 导致 reviewer 看不到关键文件 → 假阴性 NEEDS_FIX（g324 deepseek-pro 那次的根因：reviewer
  // 因 diff 截断看不到 src/http/handlers.rs 等改动，保守判 NEEDS_FIX，全绿成果被回滚）。
  // 完整 diff 写到 worktree 内 .review-diff.patch（reviewer 沙箱限在 worktree 内，只能读这里），
  // 评审完即删并撤销可能的 intent-to-add，不污染提交。reviewer 另有 Read/Glob/Grep 可读源文件交叉验证。
  const fullDiff = gitDiff(worktreeDir)
  const stat = git(['diff', '--stat', 'HEAD'], worktreeDir)
  const diffPath = join(worktreeDir, '.review-diff.patch')
  writeFileSync(diffPath, fullDiff)
  try {
    const prompt =
      `You are an independent reviewer (different provider). Review the change for correctness, ` +
      `regressions and contract violations.\n\n` +
      `The FULL diff is at \`.review-diff.patch\` in the workspace root — Read it first (it is NOT truncated). ` +
      `You may also Read any source file in the workspace to cross-check claims in the journal/diff.\n\n` +
      `--- changed files (git diff --stat HEAD) ---\n${stat}\n\n` +
      `Respond with the last line exactly "VERDICT:PASS" or "VERDICT:NEEDS_FIX".`

    // --reviewer-agent claude：claude CLI 鉴权已停用（flowcast 不再导出 claude）。
    // 保留分支以给出明确错误，而非静默跳过——避免用户以为 review 跑了其实没跑。
    if (opts['reviewer-agent'] === 'claude') {
      return {
        text: '[reviewer-agent claude is no longer supported — claude API/CLI has been retired; use --reviewer-provider instead]',
        ok: false,
        misconfig: true,
      }
    }

  // 默认：recursive executor + reviewer-provider（在 worktree 内可 Read/Glob/Grep 查上下文）
  const revEnv = buildEnv(opts['reviewer-provider'])
  // 未配置 reviewer-provider（无 API base/key）时显式跳过并标记 misconfig，
  // 与「网络 down」区分开，避免用户忘了加 --reviewer-provider 时 review 层悄无声息缺席。
  if (!revEnv.RECURSIVE_API_BASE || !revEnv.RECURSIVE_API_KEY) {
    console.warn('  [review] reviewer-provider 未配置（无 RECURSIVE_API_BASE/API_KEY），跳过 self-review。')
    return { text: '[reviewer provider not configured — review skipped]', ok: false, misconfig: true }
  }
  const resolvedBin = opts.bin ?? join(repo, 'target', 'release', 'recursive')
  const out = await recursive(
    prompt,
    {
      cwd: worktreeDir, workspace: '.', bin: resolvedBin, allowTools: 'Read,Glob,Grep',
      // 给 reviewer 一个独立 transcript（审计可见）+ 显式步数上限，防止只读 reviewer 空转挂很久。
      transcriptOut: join(cp.dir, 'review.json'),
      pricingFile: pricingFileOf(repo), env: revEnv, onData: tee,
      // reviewer 不沿用 agent 的 maxSteps（agent 可能 budget 很大）；用独立默认上限。
      ...(opts['reviewer-max-steps'] ? { maxSteps: opts['reviewer-max-steps'] } : {}),
    },
  )
  const m = out._meta ?? {}
  const ok = m.exitCode === 0 && !m.spawnError && !m.timedOut
  return { text: String(out), ok }
  } finally {
    // 评审完清掉 diff 文件 + 撤销 gitDiff 可能给它打的 intent-to-add，避免污染后续 diff / 提交
    try { rmSync(diffPath, { force: true }) } catch { /* 已不在 */ }
    try { git(['reset', '-q', 'HEAD', '--', '.review-diff.patch'], worktreeDir) } catch { /* 未 staged */ }
  }
}

// ── system prompt 构建 ───────────────────────────────────────────

function buildSystemPrompt() {
  const parts = [HEADLESS_CONSTRAINT]
  // 契约：AGENTS.md / CLAUDE.md
  for (const f of ['AGENTS.md', 'CLAUDE.md']) {
    const p = join(repo, f)
    if (existsSync(p)) { parts.push(`# Project contract (${f})\n\n${readFileSync(p, 'utf8')}`); break }
  }
  // 最近 journal
  const recentJournal = latestJournal(repo)
  if (recentJournal) parts.push(`# Recent journal\n\n${recentJournal}`)
  // 上次失败上下文（读取即消费，只注入一次）
  const failCtx = readAndConsumeFailureContext(cp.dir, 'recursive')
  if (failCtx) parts.push(failCtx)

  const file = join(cp.dir, 'system-prompt.md')
  writeFileSync(file, parts.join('\n\n---\n\n') + '\n')
  return file
}

function latestJournal(repoPath) {
  const dir = join(repoPath, '.dev', 'journal')
  if (!existsSync(dir)) return null
  // 按 mtime 排序取最新：journal 命名混排（manual-YYYYMMDD-x.md 与 gNN-…md）时，
  // 字典序 ≠ 时间序，按 mtime 更稳。
  const files = readdirSync(dir)
    .filter(f => f.endsWith('.md'))
    .map(f => ({ f, mtime: statSync(join(dir, f)).mtimeMs }))
    .sort((a, b) => b.mtime - a.mtime)
  if (!files.length) return null
  return readFileSync(join(dir, files[0].f), 'utf8').slice(0, 4000)
}

// ── 收尾：metrics / 通知 ─────────────────────────────────────────

function computeMetrics(baseline, result) {
  let filesChanged = 0
  try {
    // 显式 baseline..HEAD 范围：committed 时反映本次落地提交的真实改动集；
    // 回滚后 HEAD==baseline，区间为空 → 0（baseline 不可达时 catch 兜底）。
    const out = git(['diff', '--name-only', `${baseline}..HEAD`], repo)
    filesChanged = out ? out.split('\n').filter(Boolean).length : 0
  } catch { /* baseline 可能已不可达（回滚后），忽略 */ }
  return {
    files_changed: filesChanged,
    verdict: result.verdict,
    detail: result.detail ?? '',
  }
}

async function announce(result, baseline) {
  const branch = currentBranch(repo)
  if (result.verdict === 'committed') {
    await notify(
      `✅ recursive self-improve 成功落地\n仓库: ${repo}\n分支: ${branch}\ngoal: ${goal.slice(0, 80)}\n` +
      `已通过 worktree cherry-pick 直接落在 main checkout 当前分支（${branch}），无需额外 merge：\n` +
      `  git -C ${repo} log --oneline ${baseline}..HEAD\n  git -C ${repo} push origin ${branch}`,
    )
  } else if (result.verdict === 'failed-preserved' || result.verdict === 'panic-preserved') {
    const resumeHint = result.preserve
      ? `  接手: node .dev/flows/self-improve.flow.js --resume-preserve ${runId} --provider <更强>\n` +
        `  落地: node .dev/flows/self-improve.flow.js --land-preserve ${runId}\n` +
        `  清理: node .dev/flows/self-improve.flow.js --prune-preserve ${runId}`
      : ''
    await notify(`⚠️ recursive self-improve ${result.verdict}（现场已保留，未回滚）\n仓库: ${repo}\n原因: ${result.detail}\n${resumeHint}\nrun: ${cp.dir}`)
  } else if (result.verdict === 'rolled-back') {
    await notify(`🔁 本次自改未通过（已回滚到 baseline）\n仓库: ${repo}\n原因: ${result.detail}\nrun: ${cp.dir}`)
  } else {
    await notify(`ℹ️ recursive self-improve: ${result.verdict}（${result.detail}）\nrun: ${cp.dir}`)
  }
}

// ── 工具 ─────────────────────────────────────────────────────────

function resolveGoal() {
  if (opts.goal) return opts.goal
  if (opts['goal-file'] && existsSync(opts['goal-file'])) return readFileSync(opts['goal-file'], 'utf8').trim()
  return null
}

// 从 goal 派生干净的 commit 标题：优先取首个 markdown 一级标题文本；
// 退化到首个非空行；再去掉 markdown 头与 "Goal:" 前缀。截 60 字符。
function goalSubject() {
  const heading = goal.match(/^#\s+(.+)$/m)
  const firstLine = heading
    ? heading[1]
    : (goal.split('\n').find(l => l.trim()) ?? goal)
  return firstLine.replace(/^#+\s*/, '').replace(/^Goal:\s*/i, '').trim().slice(0, 60)
}

// recursive 通过 env 读取 provider 配置（与 self-improve.sh 一致），不走 --provider flag。
// 通用解析（resolveProvider）+ recursive 专属 env 翻译（recursiveProviderEnv）分两层。
// RECURSIVE_HEADLESS=1：禁用 interactive plan-mode tools（enter/exit_plan_mode），
// 防止 agent 在无人值守的 batch run 中调用 exit_plan_mode 后永久等待审批信号（deadlock）。
function resolveMaxSteps() {
  // 仅当 CLI 显式传入时才覆盖 recursive 默认（0 = 不限步数）。
  return opts['max-steps'] ?? opts.budget ?? null
}

function buildEnv(providerOverride) {
  const maxSteps = resolveMaxSteps()
  const hasMaxSteps = maxSteps != null && maxSteps !== ''
  const headless = { RECURSIVE_HEADLESS: 'true' }
  const bundle = resolveProvider(providerOverride ?? opts.provider, PROVIDERS)
  if (!bundle) {
    // 无 provider bundle：仍要把 CLI 显式 maxSteps 注入（recursive 读 RECURSIVE_MAX_STEPS）
    if (hasMaxSteps) headless.RECURSIVE_MAX_STEPS = String(maxSteps)
    return headless
  }
  if (opts.model) bundle.model = opts.model // --model 覆盖 profile 默认模型
  if (hasMaxSteps) bundle.maxSteps = maxSteps
  // RECURSIVE_MAX_STEPS 统一由 recursiveProviderEnv 从 bundle.maxSteps 注入（单一来源）；
  // headless 在后只补 RECURSIVE_HEADLESS，避免旧代码里 MAX_STEPS 被设两遍的冗余。
  return { ...recursiveProviderEnv(bundle), ...headless }
}


function configureHitl() {
  if (opts.hitl === 'wecom') {
    setHitlBackend('wecom', { projectName: opts['project-name'] })
  } else if (opts.hitl === 'ilink') {
    setHitlBackend(makeIlinkBackend({
      serviceUrl: opts['ilink-service-url'] || process.env.ILINK_HITL_URL || 'http://localhost:8081',
      botKey:     opts['ilink-bot-key']     || process.env.ILINK_HITL_BOT_KEY || '',
      projectName: opts['project-name'],
      waitTimeoutSec: Number(opts['ilink-wait-timeout'] || process.env.ILINK_HITL_WAIT_TIMEOUT || 86400),
      pollIntervalSec: 2,
    }))
  } else {
    setHitlBackend('terminal')
  }
}

// ── ilink HITL backend ────────────────────────────────────────────
// 走 hil-mcp 的 HITL Server HTTP API（与 cursor 的 user-hitl MCP 同一后端）：
//   POST /api/send { message, wait_reply, timeout, bot_key, upstream:'ilink' } → { success, session_id, error? }
//   GET  /api/poll/{session_id} → { has_reply, replies:[{content|text}], status? }
//   POST /api/session/{session_id}/timeout   （超时收尾，best-effort）
// 契约源：infra4agent/hil-mcp/packages/mcp-server-ts/src/engines/ilink.ts
function makeIlinkBackend(cfg) {
  const base = cfg.serviceUrl.replace(/\/$/, '')
  const sleep = ms => new Promise(r => setTimeout(r, ms))
  async function apiFetch(path, init) {
    const res = await fetch(`${base}${path}`, { ...init, signal: AbortSignal.timeout(30_000) })
    if (!res.ok) throw new Error(`ilink HITL HTTP ${res.status} ${path}`)
    return res.json()
  }
  async function send(message, { waitReply }) {
    return apiFetch('/api/send', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        message,
        wait_reply: waitReply,
        timeout: cfg.waitTimeoutSec,
        bot_key: cfg.botKey,
        upstream: 'ilink',
      }),
    })
  }
  return {
    async waitForInput(prompt) {
      const tag = cfg.projectName ? `[${cfg.projectName}] ` : ''
      const r = await send(`${tag}${prompt}`, { waitReply: true })
      if (!r.success) throw new Error(`ilink HITL 发送失败: ${r.error ?? '未知'}`)
      const sid = r.session_id
      if (!sid) throw new Error('ilink HITL: 发送成功但无 session_id')
      const deadline = Date.now() + cfg.waitTimeoutSec * 1000
      while (Date.now() < deadline) {
        const poll = await apiFetch(`/api/poll/${sid}`).catch(e => ({ error: String(e) }))
        if (poll.has_reply) {
          const replies = (poll.replies ?? []).map(x => x.content ?? x.text ?? '').filter(Boolean)
          return replies[0] ?? ''
        }
        if (poll.status === 'not_found') throw new Error('ilink HITL: 会话不存在或已过期')
        await sleep(cfg.pollIntervalSec * 1000)
      }
      await apiFetch(`/api/session/${sid}/timeout`, { method: 'POST' }).catch(() => {})
      throw new Error(`ilink HITL: 等待 ${cfg.waitTimeoutSec}s 超时`)
    },
    async notify(message) {
      const tag = cfg.projectName ? `[${cfg.projectName}] ` : ''
      const r = await send(`${tag}${message}`, { waitReply: false })
      if (!r.success) throw new Error(`ilink HITL notify 失败: ${r.error ?? '未知'}`)
    },
  }
}

function pricingFileOf(repoPath) {
  for (const rel of ['.dev/pricing.yaml', 'pricing.yaml', 'pricing.json']) {
    const p = join(repoPath, rel)
    if (existsSync(p)) return p
  }
  return undefined
}

function tee(s) { process.stdout.write(s) }

function tailOf(transcriptOut, n = 2000) {
  if (!existsSync(transcriptOut)) return ''
  try { return readFileSync(transcriptOut, 'utf8').slice(-n) } catch { return '' }
}

function transcriptMessagesOf(transcriptOut) {
  if (!existsSync(transcriptOut)) return 0
  try { return JSON.parse(readFileSync(transcriptOut, 'utf8')).messages?.length ?? 0 } catch { return 0 }
}

function git(args, cwd) {
  return execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim()
}

/** 把 pattern 追加进 .git/info/exclude（本地排除，幂等，不动 tracked 文件）。 */
function ensureGitExclude(cwd, pattern) {
  try {
    // worktree 下 info/exclude 在 common git dir，不是 per-worktree 的 --git-dir
    const gitDir = git(['rev-parse', '--git-common-dir'], cwd)
    const excludePath = join(gitDir.startsWith('/') ? gitDir : join(cwd, gitDir), 'info', 'exclude')
    const current = existsSync(excludePath) ? readFileSync(excludePath, 'utf8') : ''
    if (!current.split('\n').includes(pattern)) {
      writeFileSync(excludePath, current + (current.endsWith('\n') || !current ? '' : '\n') + pattern + '\n')
    }
  } catch { /* 非致命：排除失败仅影响 clean 检查整洁度 */ }
}
function gitClean(cwd) { return git(['status', '--porcelain'], cwd) === '' }
function gitDiff(cwd) {
  // Use --intent-to-add so brand-new untracked files appear in `git diff HEAD`.
  // Without this, a goal that only adds new files produces an empty diff and the
  // reviewer sees nothing (causing a spurious NEEDS_FIX on an empty prompt).
  // 只对 status 列出的未跟踪文件做 intent-to-add，避免在大 worktree 上全目录扫描。
  try {
    const status = git(['status', '--porcelain'], cwd)
    const untracked = status.split('\n')
      .filter(l => l.startsWith('??'))
      .map(l => l.slice(3).replace(/^"(.*)"$/, '$1'))
      .filter(Boolean)
    if (untracked.length) git(['add', '--intent-to-add', '--', ...untracked], cwd)
  } catch { /* best-effort */ }
  return git(['diff', 'HEAD'], cwd)
}
function currentBranch(cwd) { try { return git(['rev-parse', '--abbrev-ref', 'HEAD'], cwd) } catch { return '?' } }

function listRuns() {
  const dir = join(flowcastDir(opts.repo), 'runs')
  if (!existsSync(dir)) { console.log('无历史 run'); return }
  readdirSync(dir).forEach(id => {
    try {
      const s = JSON.parse(readFileSync(`${dir}/${id}/state.json`, 'utf8'))
      console.log(`${id}  status=${s.status}  verdict=${s.summary?.verdict ?? '-'}  step=${s.currentStep ?? s.status}`)
    } catch { /* 跳过损坏的 state */ }
  })
}

/**
 * 杀掉与本仓库关联的、孤儿 recursive 进程（flow 已死、recursive 仍挂）。
 *
 * 关键：必须区分「孤儿」与「另一个活跃 run 的 recursive」，否则会误杀并发 run。
 * 识别手段：recursive 子进程的 argv 含 `--transcript-out <…>/runs/<runId>/transcript.json`，
 * 从中提取 runId；同时用 pgrep 列出活跃的 self-improve.flow.js 进程及其 `--run-id <id>`。
 *   - proc 的 runId ∈ 活跃 runId 集合 → 属于活跃 run，跳过
 *   - proc 的 runId ∉ 活跃 runId 集合 → 孤儿，杀
 *   - proc 无 runId 标记（手动 recursive / 旧版无 transcript-out）→ 仅当命令行命中本仓路径才保守杀
 * 非致命：失败只打警告，不影响主流程。
 */
function killStaleRecursiveProcs(repoPath, currentRunId) {
  try {
    // 1. 收集所有活跃 self-improve flow 的 runId（含本 run）——它们的 recursive 子进程不许动。
    const liveRunIds = new Set([currentRunId])
    try {
      const flowOut = execFileSync('pgrep', ['-af', 'self-improve.flow.js'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] })
      for (const line of flowOut.trim().split('\n').filter(Boolean)) {
        const m = line.match(/--run-id\s+(\S+)/)
        if (m) liveRunIds.add(m[1])
      }
    } catch { /* pgrep 无匹配时 exit 1，忽略 */ }

    // 2. 列出所有 recursive 二进制进程
    let psOut
    try {
      psOut = execFileSync('pgrep', ['-af', 'recursive'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] })
    } catch { return } // pgrep 无匹配时 exit 1，直接返回
    const lines = psOut.trim().split('\n').filter(Boolean)
    const killed = []
    for (const line of lines) {
      const m = line.match(/^(\d+)\s+/)
      if (!m) continue
      const pid = parseInt(m[1], 10)
      if (pid === process.pid) continue               // 不杀自己
      // 只杀 recursive 二进制进程（不杀 node flow 进程 / pgrep 自身）
      if (!line.includes('/recursive ') && !line.includes('/recursive\t')) continue
      // 从 --transcript-out 路径提取 runId（flowcast runs/<runId>/transcript.json 约定）
      const tm = line.match(/--transcript-out\s+\S*?runs\/([^/\s]+)\//)
      const procRunId = tm ? tm[1] : null
      if (procRunId) {
        if (liveRunIds.has(procRunId)) continue       // 属于活跃 run → 跳过
        // 带 runId 但对应 flow 已不存活 → 孤儿，杀
      } else {
        // 无 runId 标记（手动 recursive / 旧版）：仅当命令行命中本仓才保守杀
        if (!line.includes(repoPath) && !line.includes('target/release/recursive') && !line.includes('target/debug/recursive')) continue
      }
      try {
        process.kill(pid, 'SIGKILL')
        killed.push(pid)
      } catch { /* 进程已消失 */ }
    }
    if (killed.length > 0) {
      console.log(`  [preflight] killed stale recursive procs: ${killed.join(', ')}`)
    }
  } catch (e) {
    console.warn(`  [preflight] stale-proc cleanup failed (non-fatal): ${e.message}`)
  }
}

/**
 * Provider API 健康探测：向 /models 发一个带短超时的 GET 请求。
 * 只要服务器有响应（包括 401/404）就认为 API 可达，挂死或连接拒绝才失败。
 * 这样能提前 10s 而非 5min 发现 API 不可用。
 */
async function pingProvider(env) {
  const apiBase = env.RECURSIVE_API_BASE
  const apiKey  = env.RECURSIVE_API_KEY
  if (!apiBase || !apiKey) { console.log('  [provider-ping] skipped (no provider env)'); return 'skipped' }

  const url = apiBase.replace(/\/$/, '') + '/models'
  console.log(`  [provider-ping] GET ${url} ...`)
  try {
    const resp = await fetch(url, {
      headers: { Authorization: `Bearer ${apiKey}` },
      signal: AbortSignal.timeout(12_000), // 12s 超时，比一次 LLM retry 短
    })
    // 401/403 = 鉴权被拒：key 错/失效，应在 preflight 就 fail-fast，而不是等 agent 跑几分钟后挂掉。
    if (resp.status === 401 || resp.status === 403) {
      throw new Error(`Provider ping rejected auth (HTTP ${resp.status}) — check RECURSIVE_API_KEY for ${apiBase}`)
    }
    // 其余响应（含 404 等）说明服务可达
    console.log(`  [provider-ping] ok (HTTP ${resp.status})`)
    return `ok:${resp.status}`
  } catch (e) {
    const msg = e.name === 'TimeoutError'
      ? `Provider ping timed out after 12s (${apiBase}) — API may be down or unreachable`
      : `Provider ping failed: ${e.message} (${apiBase})`
    throw new Error(msg)
  }
}
