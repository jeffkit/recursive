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
 *     → 质量门（test / clippy / fmt / e2e，各带一次 resume-fix）
 *     → 跨 provider self-review
 *     → verdict（committed / rolled-back / skip-commit / panic-preserved）
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
import { readdirSync, readFileSync, writeFileSync, existsSync, statSync } from 'fs'
import { join, relative } from 'path'
import { execFileSync } from 'child_process'

import {
  Checkpoint,
  recursive, recursiveProviderEnv, claude, setWorkdir, setHitlBackend, notify, waitForInput,
  captureBaseline,
  runGate, loadGates, mergeGates,
  writeFailureContext, readAndConsumeFailureContext,
  loadProviders, resolveProvider,
  flowcastDir,
  gitWorktreeAdd, gitWorktreeRemove,
} from 'flowcast'

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
  },
})

if (opts.list) { listRuns(); process.exit(0) }

const runId = opts['run-id'] ?? `selfimprove-${Date.now()}`
const repo = opts.repo

// Batch-run constraint prepended to every system prompt:
// plan mode tools block indefinitely when no interactive channel is present.
const HEADLESS_CONSTRAINT = `# Headless batch-run constraints

You are running non-interactively (no human in the loop).

**DO NOT call \`enter_plan_mode\` or \`exit_plan_mode\`.** These tools block
forever waiting for a human to approve the plan — in batch mode there is no
approval channel, so calling them causes an unrecoverable deadlock.

Implement directly: read → think → patch → test. No plan-mode ceremony needed.`

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
  // 不再用 withSelfModGuard 包裹：worktree 本身就是沙箱，agent 改动隔离在 worktreeDir，
  // 非 committed verdict 时直接丢弃 worktree 即「回滚」，main checkout 全程不被触碰
  // （cherry-pick 只在 committed 路径发生）。这避免了旧设计里 guard 的 reset --hard
  // 在 .worktrees/ 仍注册时抛「回滚后工作树仍脏」、以及在 main 被并发推进时吃掉
  // 无关提交的两个破坏性问题。cherry-pick 前显式校验 main 未移动、冲突时 abort 而非 reset。
  const transcriptOut = join(cp.dir, 'transcript.json')
  let result
  try {
    result = await runAttempt({ sysPromptFile, transcriptOut, baseline, worktreeDir })
  } catch (err) {
    // 基础设施级失败（spawn 崩、gate 脚手架异常等）：丢弃 worktree，记录上下文，按回滚处理
    writeFailureContext(cp.dir, 'recursive', {
      reason: `attempt error: ${err.message}`, tailLog: String(err.stack ?? err).slice(-2000),
      provider: opts.provider, model: opts.model,
    })
    result = { verdict: 'rolled-back', detail: err.message }
  } finally {
    cleanupWt()
    unregisterCleanup()
  }

  // ── 收尾：metrics + 报告 + 落地指针 / 升级通知 ──────────────────
  const metrics = computeMetrics(baseline, result)
  cp.done({ goal: goal.slice(0, 120), verdict: result.verdict, ...metrics })

  await announce(result, baseline)
  console.log(`\n✓ recursive-self-improve 结束  verdict=${result.verdict}`)
}

/**
 * 单次尝试：跑 recursive → budget resume → 质量门 → review → 产出 verdict。
 * 注意：本函数不在 withSelfModGuard 内（worktree 即沙箱）。返回 verdict 对象，
 * main() 据此收尾——非 committed 时丢弃 worktree 即「回滚」，main checkout 全程不动。
 *
 * agent 改动全部发生在 worktreeDir（隔离）；质量门也在 worktreeDir 内跑；
 * 最终 cherry-pick 回 repo（main checkout）再提交，保持 main 始终干净。
 * cherry-pick 前校验 main 未被推进，冲突时 abort 而非 reset（绝不吃掉别人的提交）。
 */
async function runAttempt({ sysPromptFile, transcriptOut, baseline, worktreeDir }) {
  const env = buildEnv() // provider 配置经 env 注入（RECURSIVE_PROVIDER_TYPE/API_BASE/MODEL/API_KEY）
  // recursive 二进制固定用 main repo 编译的产物（preflight.build 已确保最新）
  const resolvedBin = opts.bin ?? join(repo, 'target', 'release', 'recursive')
  // recursive 调用的公共选项（pricing / system-prompt / 流式输出）
  // cwd 指向 worktreeDir，让 agent 在隔离目录内读写文件
  const base = () => ({
    cwd: worktreeDir, workspace: '.', bin: resolvedBin, systemPromptFile: sysPromptFile,
    pricingFile: pricingFileOf(repo), env, onData: tee,
  })

  // ① 跑 recursive 二进制
  const runMeta = await cp.step('run.recursive', async () => {
    const out = await recursive(goal, { ...base(), transcriptOut })
    return out._meta
  })

  // panic：保留现场不回滚，留作诊断
  if (runMeta.panicked) {
    writeFailureContext(cp.dir, 'recursive', {
      reason: 'panic', tailLog: tailOf(transcriptOut), provider: opts.provider, model: opts.model,
    })
    return { verdict: 'panic-preserved', detail: `exit ${runMeta.exitCode}` }
  }

  // ② BudgetExceeded → 自动 resume 一次（写独立 transcript，避免覆盖被 replay 的源）
  // latestTranscript 跟踪「最近一次成功的 transcript 路径」，后续质量门的 resume-fix
  // 必须从它 replay——否则发生过 budget resume 后，resume-fix 会从 resume 之前的 transcript
  // 重放，丢失 resume 阶段全部 tool call，agent 在「忘了刚做啥」的状态下修 bug 必败。
  let lastMeta = runMeta
  let latestTranscript = transcriptOut
  if (runMeta.budgetExceeded) {
    const resumedTranscript = transcriptOut.replace(/\.json$/, '-resumed.json')
    lastMeta = await cp.step('run.recursive.resume', async () => {
      const out = await recursive(goal, {
        ...base(), transcriptOut: resumedTranscript,
        replayFrom: { transcript: transcriptOut, resumeFrom: runMeta.transcriptMessages },
      })
      return out._meta
    })
    latestTranscript = resumedTranscript
    // resume 自己 panic：同样保留现场，不进质量门（旧代码只查 budgetExceeded，漏了 panic）
    if (lastMeta.panicked) {
      writeFailureContext(cp.dir, 'recursive', {
        reason: 'panic (after resume)', tailLog: tailOf(resumedTranscript),
        provider: opts.provider, model: opts.model,
      })
      return { verdict: 'panic-preserved', detail: `resume exit ${lastMeta.exitCode}` }
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

  // ③ 质量门：test / clippy / fmt（+ 可选 e2e），各带一次 resume-fix
  // 所有门在 worktreeDir 内执行，保证测试的是 agent 实际修改的代码。
  // resume-fix 从 latestTranscript replay（见上），保留 budget-resume 后的全部上下文。
  try {
    await runQualityGates({ sysPromptFile, transcriptOut: latestTranscript, env, worktreeDir })
  } catch (err) {
    // 任意门红灯 → 记录失败上下文，回滚（worktree 由 main() 清理）
    writeFailureContext(cp.dir, 'recursive', {
      reason: `quality gate '${err.gate}' failed`, tailLog: (err.output ?? '').slice(-2000),
      provider: opts.provider, model: opts.model,
    })
    return { verdict: 'rolled-back', detail: err.message }
  }

  // ④ 跨 provider self-review（区分「明确 NEEDS_FIX」与「reviewer 不可用 / 未配置」）
  if (!opts['no-review']) {
    const { decision, text, misconfig } = await cp.step('review', () => reviewWithRetry(worktreeDir))
    if (decision === 'NEEDS_FIX') {
      writeFailureContext(cp.dir, 'recursive', { reason: 'self-review NEEDS_FIX', tailLog: text.slice(-2000) })
      return { verdict: 'rolled-back', detail: 'self-review NEEDS_FIX' }
    }
    if (decision === 'UNAVAILABLE') {
      // reviewer 多次报错（网络/quota）或未配置：质量门已全绿，直接提交并通知。
      // 理由：所有质量门（cargo test/clippy/fmt + 项目门）均已通过，代码可靠性已验证，
      //       reviewer 仅是可选的二次确认层，其不可用不应让成果丢失。
      // 区分「未配置」（建议加 --reviewer-provider 或 --no-review）与「网络 down」，便于排查。
      const tag = misconfig
        ? 'reviewer 未配置（建议加 --reviewer-provider 或 --no-review 显式跳过）'
        : 'reviewer 不可用（网络/quota）'
      await notify(`ℹ️ self-review ${tag}但质量门全绿，自动提交改动。\nrun: ${cp.dir}`)
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

// ── 质量门 ───────────────────────────────────────────────────────

async function runQualityGates({ sysPromptFile, transcriptOut, env, worktreeDir }) {
  const resolvedBin = opts.bin ?? join(repo, 'target', 'release', 'recursive')
  // resume-fix：把失败输出喂回 recursive 修一次（在原 transcript 上续跑，仍在 worktree 内）
  const resumeFix = async (output, gate) => {
    const fixGoal = `The "${gate.name}" check failed. Fix it.\n\n--- check output (tail) ---\n${(output ?? '').slice(-2000)}`
    const fixTranscript = transcriptOut.replace(/\.json$/, `-fix-${gate.name}.json`)
    await recursive(fixGoal, {
      cwd: worktreeDir, workspace: '.', bin: resolvedBin, systemPromptFile: sysPromptFile,
      transcriptOut: fixTranscript, pricingFile: pricingFileOf(repo), env, onData: tee,
      replayFrom: { transcript: transcriptOut, resumeFrom: transcriptMessagesOf(transcriptOut) },
    })
    return true
  }

  // 内置默认门（语言相关：cargo test/clippy/fmt）+ 项目自定义门。
  // 项目门来自 <repo>/.flowcast/gates.json（committed），与 provider/agent 配置对称——
  // recursive 的 argusai E2E 门即在那里声明，不再靠 flow 里硬编码探测脚本路径。
  // 所有门的 cwd 默认为 worktreeDir，保证测试的是 agent 实际修改的代码。
  const builtin = qualityGatesFor(worktreeDir)
  const projectGates = await loadGates({ repo })
  // 项目门未显式写 cwd 时默认在 worktreeDir 跑；写了则尊重项目声明。
  const gates = mergeGates(builtin, projectGates).map(g => ({ cwd: worktreeDir, ...g }))
  for (const g of gates) {
    await cp.step(`gate.${g.name}`, () => runGate(g, { resumeFix }))
  }
}

/** recursive（Rust）内置默认质量门。项目特定门（含 E2E）走 <repo>/.flowcast/gates.json。 */
function qualityGatesFor(repoPath) {
  return [
    { name: 'test',   cmd: 'cargo test --quiet',                                       cwd: repoPath, onFail: 'resume-fix', timeout: 1_200_000 },
    { name: 'clippy', cmd: 'cargo clippy --all-targets --all-features -- -D warnings', cwd: repoPath, onFail: 'resume-fix', timeout: 600_000 },
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
  // diff 取自 worktree（agent 的实际改动），reviewer 在 worktree 内读文件做上下文
  const diff = gitDiff(worktreeDir).slice(0, 20_000)
  const prompt =
    `You are an independent reviewer (different provider). Review the following diff for correctness, ` +
    `regressions and contract violations. Respond with the last line exactly "VERDICT:PASS" or "VERDICT:NEEDS_FIX".\n\n${diff}`

  // --reviewer-agent claude：用 claude CLI 做 review（自管鉴权，不需要外部 provider）
  if (opts['reviewer-agent'] === 'claude') {
    try {
      const text = await claude(prompt, { cwd: worktreeDir, timeout: 120_000 })
      return { text: String(text), ok: true }
    } catch (err) {
      return { text: String(err), ok: false }
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
  } else if (result.verdict === 'panic-preserved') {
    await notify(`⚠️ recursive panic，已保留现场待诊断\n仓库: ${repo}\n详情: ${result.detail}\nrun: ${cp.dir}`)
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
