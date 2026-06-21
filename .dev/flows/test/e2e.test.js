import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, writeFileSync, chmodSync, existsSync, readFileSync, rmSync } from 'fs'
import { tmpdir } from 'os'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'
import { execFileSync, spawnSync } from 'child_process'

// 结构化 E2E：用假 recursive 二进制 + 假 cargo 在临时 git 仓里跑完整 flow，
// 验证 checkpoint→guard→gate→verdict→回滚 全链路，不依赖真实 LLM / API。

const FLOW = join(dirname(fileURLToPath(import.meta.url)), '..', 'self-improve.flow.js')

function git(args, cwd) {
  return execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim()
}

const FAKE_RECURSIVE = `#!/bin/sh
T=""; prev=""
for a in "$@"; do [ "$prev" = "--transcript-out" ] && T="$a"; prev="$a"; done
[ -n "$T" ] && printf '{"messages":[{"r":1},{"r":2},{"r":3}]}' > "$T"
echo "change $$ $(date +%s)" >> CHANGED.txt
echo "[done after 3 steps] reason: Done"
exit 0
`

const FAKE_CARGO = `#!/bin/sh
sub="$1"
if [ "$CARGO_FAIL" = "$sub" ]; then echo "$sub FAILED"; exit 1; fi
echo "$sub ok"; exit 0
`

/** 建临时 git 仓 + 假 recursive 二进制 + PATH 上的假 cargo。 */
function setup() {
  const root = mkdtempSync(join(tmpdir(), 'flowx-e2e-'))
  const repo = join(root, 'repo')
  mkdirSync(join(repo, 'target', 'release'), { recursive: true })
  mkdirSync(join(repo, 'src'), { recursive: true })

  const recBin = join(repo, 'target', 'release', 'recursive')
  writeFileSync(recBin, FAKE_RECURSIVE); chmodSync(recBin, 0o755)

  const binDir = join(root, 'fakebin')
  mkdirSync(binDir)
  const cargo = join(binDir, 'cargo')
  writeFileSync(cargo, FAKE_CARGO); chmodSync(cargo, 0o755)

  writeFileSync(join(repo, 'src', 'lib.rs'), 'pub fn x() {}\n')
  writeFileSync(join(repo, 'AGENTS.md'), '# contract\nbe good\n')
  git(['init', '-q'], repo)
  git(['config', 'user.email', 't@t'], repo)
  git(['config', 'user.name', 't'], repo)
  git(['add', '.'], repo)
  git(['commit', '-q', '-m', 'init'], repo)

  return { root, repo, binDir }
}

function runFlow({ repo, binDir, runId, cargoFail }) {
  const r = spawnSync('node', [FLOW, '--run-id', runId, '--repo', repo, '--goal', 'add a helper', '--no-review'], {
    cwd: repo,
    encoding: 'utf8',
    env: { ...process.env, PATH: `${binDir}:${process.env.PATH}`, ...(cargoFail ? { CARGO_FAIL: cargoFail } : {}) },
    timeout: 60_000,
  })
  return r
}

test('E2E 成功路径：verdict=committed，落地 commit + report.md', () => {
  const { root, repo, binDir } = setup()
  const baseline = git(['rev-parse', 'HEAD'], repo)
  const runId = 'e2e-ok'

  const r = runFlow({ repo, binDir, runId })
  assert.equal(r.status, 0, `flow 应正常退出:\n${r.stdout}\n${r.stderr}`)

  // 产生了新 commit
  const head = git(['rev-parse', 'HEAD'], repo)
  assert.notEqual(head, baseline, '成功路径应产生 commit')

  // 审计产物齐全（全新临时仓 → flowcast 默认 .flowcast/）
  const runDir = join(repo, '.flowcast', 'runs', runId)
  const state = JSON.parse(readFileSync(join(runDir, 'state.json'), 'utf8'))
  assert.equal(state.status, 'completed')
  assert.equal(state.summary.verdict, 'committed')
  assert.equal(existsSync(join(runDir, 'report.md')), true)
  assert.equal(existsSync(join(runDir, 'run.log.jsonl')), true)

  rmSync(root, { recursive: true, force: true })
})

test('E2E 回滚路径：cargo test 红灯 → verdict=rolled-back，工作树回到 baseline', () => {
  const { root, repo, binDir } = setup()
  const baseline = git(['rev-parse', 'HEAD'], repo)
  const runId = 'e2e-rollback'

  const r = runFlow({ repo, binDir, runId, cargoFail: 'test' })
  assert.equal(r.status, 0, `flow 应正常退出（回滚不算崩溃）:\n${r.stdout}\n${r.stderr}`)

  // HEAD 不变 + 工作树干净（硬回滚生效）
  assert.equal(git(['rev-parse', 'HEAD'], repo), baseline, 'HEAD 应回到 baseline')
  assert.equal(git(['status', '--porcelain'], repo), '', '工作树应干净（含 untracked 被 clean）')

  const state = JSON.parse(readFileSync(join(repo, '.flowcast', 'runs', runId, 'state.json'), 'utf8'))
  assert.equal(state.summary.verdict, 'rolled-back')

  rmSync(root, { recursive: true, force: true })
})

test('E2E 项目自定义门：.flowcast/gates.json 声明的门红灯 → rolled-back（验证 loadGates 接线）', () => {
  const { root, repo, binDir } = setup()
  // 业务项目在仓内声明一个必然红灯的自定义门（committed），不改 flow 代码。
  mkdirSync(join(repo, '.flowcast'), { recursive: true })
  writeFileSync(join(repo, '.flowcast', 'gates.json'), JSON.stringify({
    gates: { custombiz: { cmd: 'exit 1', onFail: 'rollback' } },
  }))
  git(['add', '.'], repo)
  git(['commit', '-q', '-m', 'add project gate'], repo)
  const baseline = git(['rev-parse', 'HEAD'], repo)
  const runId = 'e2e-custom-gate'

  // 内置 cargo 门（假 cargo）全绿，自定义门 custombiz 红灯 → 整体回滚。
  const r = runFlow({ repo, binDir, runId })
  assert.equal(r.status, 0, `flow 应正常退出:\n${r.stdout}\n${r.stderr}`)
  assert.equal(git(['rev-parse', 'HEAD'], repo), baseline, 'HEAD 应回到 baseline（自定义门红灯触发回滚）')

  const state = JSON.parse(readFileSync(join(repo, '.flowcast', 'runs', runId, 'state.json'), 'utf8'))
  assert.equal(state.summary.verdict, 'rolled-back')
  // 回滚原因必须正是项目自定义门 custombiz（内置 cargo 门全绿，唯一红灯来源）——
  // 直接证明 loadGates 加载的项目门被合并进门链并真正执行。
  assert.match(
    state.summary.detail ?? '',
    /custombiz/,
    `回滚原因应来自项目门 custombiz，实际 detail: ${state.summary.detail}`,
  )

  rmSync(root, { recursive: true, force: true })
})

test('E2E clippy 门红灯 → verdict=rolled-back，HEAD 回到 baseline', () => {
  // CARGO_FAIL=clippy 让假 cargo clippy 以非零退出，触发 clippy 质量门失败路径。
  const { root, repo, binDir } = setup()
  const baseline = git(['rev-parse', 'HEAD'], repo)
  const runId = 'e2e-clippy-fail'

  const r = runFlow({ repo, binDir, runId, cargoFail: 'clippy' })
  assert.equal(r.status, 0, `flow 应正常退出（clippy 失败不是 crash）:\n${r.stdout}\n${r.stderr}`)

  // clippy 门失败 → 回滚：HEAD 不变
  assert.equal(git(['rev-parse', 'HEAD'], repo), baseline, 'clippy 红灯后 HEAD 应回到 baseline')

  const state = JSON.parse(readFileSync(join(repo, '.flowcast', 'runs', runId, 'state.json'), 'utf8'))
  assert.equal(state.summary.verdict, 'rolled-back', `verdict 应为 rolled-back，实际: ${state.summary.verdict}`)

  // 回滚原因必须来自 clippy 门
  assert.match(
    state.summary.detail ?? '',
    /clippy/i,
    `回滚原因应来自 clippy 门，实际 detail: ${state.summary.detail}`,
  )

  rmSync(root, { recursive: true, force: true })
})

test('E2E fmt 门红灯 → verdict=rolled-back，HEAD 回到 baseline', () => {
  // CARGO_FAIL=fmt 让假 cargo fmt 以非零退出，触发 fmt 质量门失败路径。
  // fmt 门的 cmd 是 `cargo fmt --all -- --check`，$1 = "fmt"，CARGO_FAIL=fmt 即可触发。
  const { root, repo, binDir } = setup()
  const baseline = git(['rev-parse', 'HEAD'], repo)
  const runId = 'e2e-fmt-fail'

  const r = runFlow({ repo, binDir, runId, cargoFail: 'fmt' })
  assert.equal(r.status, 0, `flow 应正常退出（fmt 失败不是 crash）:\n${r.stdout}\n${r.stderr}`)

  // fmt 门失败 → 回滚：HEAD 不变
  assert.equal(git(['rev-parse', 'HEAD'], repo), baseline, 'fmt 红灯后 HEAD 应回到 baseline')

  const state = JSON.parse(readFileSync(join(repo, '.flowcast', 'runs', runId, 'state.json'), 'utf8'))
  assert.equal(state.summary.verdict, 'rolled-back', `verdict 应为 rolled-back，实际: ${state.summary.verdict}`)

  // 回滚原因必须来自 fmt 门
  assert.match(
    state.summary.detail ?? '',
    /fmt/i,
    `回滚原因应来自 fmt 门，实际 detail: ${state.summary.detail}`,
  )

  rmSync(root, { recursive: true, force: true })
})
