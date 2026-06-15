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
  recursive, recursiveProviderEnv, setWorkdir, setHitlBackend, notify, waitForInput,
  withSelfModGuard, captureBaseline,
  runGate, loadGates, mergeGates,
  writeFailureContext, readAndConsumeFailureContext,
  loadProviders, resolveProvider,
  flowcastDir,
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
    budget:     { type: 'string' },                 // 美元预算上限（env）
    'reviewer-provider': { type: 'string' },         // 跨 provider self-review
    hitl:       { type: 'string', default: 'terminal' }, // terminal | wecom
    'project-name': { type: 'string', default: 'recursive' },
    'no-review':{ type: 'boolean', default: false },
    'no-commit':{ type: 'boolean', default: false },
    list:       { type: 'boolean', default: false },
  },
})

if (opts.list) { listRuns(); process.exit(0) }

const runId = opts['run-id'] ?? `selfimprove-${Date.now()}`
const repo = opts.repo

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

await main()

// ── 主流程 ───────────────────────────────────────────────────────

async function main() {
  // 只本地排除「运行产物」子目录 <FC>/runs/，不排除整个 .flowcast/——
  // 因为项目配置（providers/agents/gates.json）就放在 .flowcast/ 下且应 committed。
  // 排除 runs/ 既避免 run 产物污染 clean 检查，又不挡住配置文件入仓。
  ensureGitExclude(repo, FC_REL + '/runs/')

  // ── 预检：捕获 baseline（持久化，续跑复用同一 baseline）──────────
  const baseline = await cp.step('preflight.baseline', () =>
    captureBaseline(repo, { requireClean: true }),
  )
  console.log(`  baseline: ${baseline}`)

  // ── 构建 system prompt（注入契约 + journal + 上次失败上下文）─────
  const sysPromptFile = await cp.step('preflight.system-prompt', () =>
    buildSystemPrompt(),
  )

  // ── 自改安全沙箱：整个尝试在 guard 内执行，verdict 决定提交/回滚 ──
  const transcriptOut = join(cp.dir, 'transcript.json')
  const result = await withSelfModGuard(
    async () => runAttempt({ sysPromptFile, transcriptOut, baseline }),
    { repo, baseline, requireClean: false }, // 续跑时工作树可能脏，由 baseline 兜底
  )

  // ── 收尾：metrics + 报告 + 落地指针 / 升级通知 ──────────────────
  const metrics = computeMetrics(baseline, result)
  cp.done({ goal: goal.slice(0, 120), verdict: result.verdict, ...metrics })

  await announce(result, baseline)
  console.log(`\n✓ recursive-self-improve 结束  verdict=${result.verdict}`)
}

/**
 * 单次尝试：跑 recursive → budget resume → 质量门 → review → 产出 verdict。
 * 注意：本函数运行在 withSelfModGuard 内。返回 verdict 对象，guard 据此回滚/保留。
 */
async function runAttempt({ sysPromptFile, transcriptOut }) {
  const env = buildEnv() // provider 配置经 env 注入（RECURSIVE_PROVIDER_TYPE/API_BASE/MODEL/API_KEY）
  // recursive 调用的公共选项（pricing / system-prompt / 流式输出）
  const base = () => ({
    cwd: repo, workspace: '.', bin: opts.bin, systemPromptFile: sysPromptFile,
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
  let lastMeta = runMeta
  if (runMeta.budgetExceeded) {
    const resumedTranscript = transcriptOut.replace(/\.json$/, '-resumed.json')
    lastMeta = await cp.step('run.recursive.resume', async () => {
      const out = await recursive(goal, {
        ...base(), transcriptOut: resumedTranscript,
        replayFrom: { transcript: transcriptOut, resumeFrom: runMeta.transcriptMessages },
      })
      return out._meta
    })
    if (lastMeta.budgetExceeded) {
      writeFailureContext(cp.dir, 'recursive', { reason: 'BudgetExceeded (after resume)', tailLog: tailOf(transcriptOut) })
      return { verdict: 'skip-commit', detail: 'budget exceeded after one resume' }
    }
  }

  // 若 recursive 没产生任何改动，跳过提交
  if (gitClean(repo)) {
    return { verdict: 'skip-commit', detail: 'no changes produced' }
  }

  // ③ 质量门：test / clippy / fmt（+ 可选 e2e），各带一次 resume-fix
  try {
    await runQualityGates({ sysPromptFile, transcriptOut, env })
  } catch (err) {
    // 任意门红灯 → 记录失败上下文，回滚
    writeFailureContext(cp.dir, 'recursive', {
      reason: `quality gate '${err.gate}' failed`, tailLog: (err.output ?? '').slice(-2000),
      provider: opts.provider, model: opts.model,
    })
    return { verdict: 'rolled-back', detail: err.message }
  }

  // ④ 跨 provider self-review（区分「明确 NEEDS_FIX」与「reviewer 不可用」）
  if (!opts['no-review']) {
    const { decision, text } = await cp.step('review', () => reviewWithRetry())
    if (decision === 'NEEDS_FIX') {
      writeFailureContext(cp.dir, 'recursive', { reason: 'self-review NEEDS_FIX', tailLog: text.slice(-2000) })
      return { verdict: 'rolled-back', detail: 'self-review NEEDS_FIX' }
    }
    if (decision === 'UNAVAILABLE') {
      // reviewer 多次报错（如网络）：质量门已全绿，不丢弃成果，保留待人工复核并升级 HITL
      await notify(`⚠️ self-review 不可用（reviewer 多次报错）。质量门全绿但未自动提交，已保留改动待人工复核。\nrun: ${cp.dir}`)
      return { verdict: 'skip-commit', detail: 'reviewer unavailable; changes preserved for human review' }
    }
  }

  // ⑤ 全绿 → 提交
  if (opts['no-commit']) return { verdict: 'skip-commit', detail: '--no-commit' }
  await cp.step('commit', () => {
    git(['add', '-A'], repo)
    git(['commit', '-m', `self-improve: ${goalSubject()}`], repo)
    return git(['rev-parse', 'HEAD'], repo)
  })
  return { verdict: 'committed' }
}

// ── 质量门 ───────────────────────────────────────────────────────

async function runQualityGates({ sysPromptFile, transcriptOut, env }) {
  // resume-fix：把失败输出喂回 recursive 修一次（在原 transcript 上续跑）
  const resumeFix = async (output, gate) => {
    const fixGoal = `The "${gate.name}" check failed. Fix it.\n\n--- check output (tail) ---\n${(output ?? '').slice(-2000)}`
    const fixTranscript = transcriptOut.replace(/\.json$/, `-fix-${gate.name}.json`)
    await recursive(fixGoal, {
      cwd: repo, workspace: '.', bin: opts.bin, systemPromptFile: sysPromptFile,
      transcriptOut: fixTranscript, pricingFile: pricingFileOf(repo), env, onData: tee,
      replayFrom: { transcript: transcriptOut, resumeFrom: transcriptMessagesOf(transcriptOut) },
    })
    return true
  }

  // 内置默认门（语言相关：cargo test/clippy/fmt）+ 项目自定义门。
  // 项目门来自 <repo>/.flowcast/gates.json（committed），与 provider/agent 配置对称——
  // recursive 的 argusai E2E 门即在那里声明，不再靠 flow 里硬编码探测脚本路径。
  const builtin = qualityGatesFor(repo)
  const projectGates = await loadGates({ repo })
  // 项目门未显式写 cwd 时默认在仓根跑；写了则尊重项目声明。
  const gates = mergeGates(builtin, projectGates).map(g => ({ cwd: repo, ...g }))
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
 * 跨 provider self-review，带重试与「不可用」区分。
 * @returns {{decision:'PASS'|'NEEDS_FIX'|'UNAVAILABLE', text:string}}
 *   - PASS         reviewer 明确通过
 *   - NEEDS_FIX    reviewer 明确否决，或正常返回但无 verdict（保守判否）
 *   - UNAVAILABLE  reviewer 多次调用出错（网络/退出码非 0），无法定论 → 不丢弃成果
 */
async function reviewWithRetry(maxAttempts = 2) {
  let lastText = ''
  for (let i = 1; i <= maxAttempts; i++) {
    const { text, ok } = await selfReview()
    lastText = text
    if (/VERDICT:\s*PASS/.test(text)) return { decision: 'PASS', text }
    if (/VERDICT:\s*NEEDS_FIX/.test(text)) return { decision: 'NEEDS_FIX', text }
    if (ok) return { decision: 'NEEDS_FIX', text } // 正常返回却无 verdict → 保守判否
    // reviewer 调用本身出错（网络/退出码）→ 重试
  }
  return { decision: 'UNAVAILABLE', text: lastText }
}

async function selfReview() {
  const diff = gitDiff(repo).slice(0, 20_000)
  const out = await recursive(
    `You are an independent reviewer (different provider). Review the following diff for correctness, ` +
    `regressions and contract violations. Respond with the last line exactly "VERDICT:PASS" or "VERDICT:NEEDS_FIX".\n\n${diff}`,
    {
      cwd: repo, workspace: '.', bin: opts.bin, allowTools: 'Read,Glob,Grep',
      pricingFile: pricingFileOf(repo), env: buildEnv(opts['reviewer-provider']), onData: tee,
    },
  )
  const m = out._meta ?? {}
  const ok = m.exitCode === 0 && !m.spawnError && !m.timedOut
  return { text: String(out), ok }
}

// ── system prompt 构建 ───────────────────────────────────────────

// Batch-run constraint prepended to every system prompt:
// plan mode tools block indefinitely when no interactive channel is present.
const HEADLESS_CONSTRAINT = `# Headless batch-run constraints

You are running non-interactively (no human in the loop).

**DO NOT call \`enter_plan_mode\` or \`exit_plan_mode\`.** These tools block
forever waiting for a human to approve the plan — in batch mode there is no
approval channel, so calling them causes an unrecoverable deadlock.

Implement directly: read → think → patch → test. No plan-mode ceremony needed.`

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
  const files = readdirSync(dir).filter(f => f.endsWith('.md')).sort()
  if (!files.length) return null
  return readFileSync(join(dir, files[files.length - 1]), 'utf8').slice(0, 4000)
}

// ── 收尾：metrics / 通知 ─────────────────────────────────────────

function computeMetrics(baseline, result) {
  let filesChanged = 0
  try {
    const out = git(['diff', '--name-only', baseline], repo)
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
      `READY TO LAND：\n  git -C ${repo} log --oneline ${baseline}..HEAD\n  git -C ${repo} checkout main && git merge ${branch}`,
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

// 从 goal 派生干净的 commit 标题：取首个非空行，去掉 markdown 头与 "Goal:" 前缀。
function goalSubject() {
  const firstLine = goal.split('\n').find(l => l.trim()) ?? goal
  return firstLine.replace(/^#+\s*/, '').replace(/^Goal:\s*/i, '').trim().slice(0, 60)
}

// recursive 通过 env 读取 provider 配置（与 self-improve.sh 一致），不走 --provider flag。
// 通用解析（resolveProvider）+ recursive 专属 env 翻译（recursiveProviderEnv）分两层。
// RECURSIVE_HEADLESS=1：禁用 interactive plan-mode tools（enter/exit_plan_mode），
// 防止 agent 在无人值守的 batch run 中调用 exit_plan_mode 后永久等待审批信号（deadlock）。
function buildEnv(providerOverride) {
  const bundle = resolveProvider(providerOverride ?? opts.provider, PROVIDERS)
  if (!bundle) return { RECURSIVE_HEADLESS: '1' }
  if (opts.model) bundle.model = opts.model // --model 覆盖 profile 默认模型
  return { ...recursiveProviderEnv({ ...bundle, maxSteps: opts.budget }), RECURSIVE_HEADLESS: '1' }
}


function configureHitl() {
  if (opts.hitl === 'wecom') {
    setHitlBackend('wecom', { projectName: opts['project-name'] })
  } else {
    setHitlBackend('terminal')
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
function gitDiff(cwd) { return git(['diff', 'HEAD'], cwd) }
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
