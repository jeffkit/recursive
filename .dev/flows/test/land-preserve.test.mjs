// land-preserve.test.mjs — 验证 land-preserve 落地「完整改动树」而非单 commit。
//
// landPreserve() 依赖大量 flow 内部状态（Checkpoint / repo / goal / opts），不便整体
// 调用。这里直接测它核心的落地机制：对一条「多 commit 的 preserve 链」，用 merge-base
// + diff apply 能把链上所有 commit 的累积改动落到 main（旧的 cherry-pick 单 sha 会丢
// 前面的 commit）。这正是 g334 救回时踩过的坑。
//
// 用真实临时 git 仓构造场景，和 e2e.test.js 的做法一致。

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync } from 'fs'
import { tmpdir } from 'os'
import { join } from 'path'
import { execFileSync } from 'child_process'

function git(args, cwd) {
  return execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim()
}

/** 构造一个临时 git 仓，返回仓库路径与初始 baseline sha。 */
function setupRepo() {
  const repo = mkdtempSync(join(tmpdir(), 'land-preserve-'))
  mkdirSync(join(repo, 'src'), { recursive: true })
  writeFileSync(join(repo, 'src', 'lib.rs'), 'pub fn x() {}\n')
  git(['init', '-q', '-b', 'main'], repo)
  git(['config', 'user.email', 't@t'], repo)
  git(['config', 'user.name', 't'], repo)
  git(['add', '.'], repo)
  git(['commit', '-q', '-m', 'init'], repo)
  return { repo, baseline: git(['rev-parse', 'HEAD'], repo) }
}

/**
 * 模拟 landPreserve 的核心落地逻辑（self-improve.flow.js landPreserve() 里「全绿后落地」
 * 那段）：对 preserve sha 求 merge-base，diff ancestor..sha 写文件，git apply，单 commit。
 */
function landFullDiff(repo, sha, commitMsg) {
  const mainHead = git(['rev-parse', 'HEAD'], repo)
  const ancestor = git(['merge-base', mainHead, sha], repo)
  if (ancestor === sha) throw new Error(`${sha.slice(0, 8)} 已是 main 祖先，无可落地`)
  const diff = git(['diff', `${ancestor}..${sha}`], repo)
  const diffFile = join(repo, '.land.diff')
  // git() 对 stdout 做了 .trim()，会剪掉 diff 末尾的换行，导致 `git apply` 报
  // "corrupt patch"。补回末尾换行，保证 patch 格式完整。
  writeFileSync(diffFile, diff.endsWith('\n') ? diff : diff + '\n')
  try {
    git(['apply', diffFile], repo)
  } catch (err) {
    try { git(['checkout', '--', '.'], repo) } catch { /* */ }
    try { git(['clean', '-fd'], repo) } catch { /* */ }
    throw new Error(`apply 冲突 (${ancestor.slice(0, 8)}..${sha.slice(0, 8)}): ${err.message}`)
  }
  git(['add', '-A'], repo)
  git(['commit', '-q', '-m', commitMsg], repo)
}

test('单 commit preserve：落地后 main 含全部改动', () => {
  const { repo } = setupRepo()
  // preserve worktree 上单个 commit：加文件 + 改文件
  const wt = join(repo, '.wt')
  git(['worktree', 'add', '-q', '--detach', wt], repo)
  writeFileSync(join(wt, 'src', 'new.rs'), 'pub fn y() {}\n')
  writeFileSync(join(wt, 'src', 'lib.rs'), 'pub fn x() {}\npub fn z() {}\n')
  git(['add', '-A'], wt)
  git(['commit', '-q', '-m', 'preserve: single'], wt)
  const sha = git(['rev-parse', 'HEAD'], wt)

  landFullDiff(repo, sha, 'land single')

  assert.equal(readFileSync(join(repo, 'src', 'new.rs'), 'utf8'), 'pub fn y() {}\n')
  assert.equal(readFileSync(join(repo, 'src', 'lib.rs'), 'utf8'), 'pub fn x() {}\npub fn z() {}\n')
})

test('多 commit preserve 链：落地后 main 含链上全部 commit 的累积改动（不丢前面的）', () => {
  // 这是 g334 踩过的坑：preserve 上有多个 commit（实现 + 修正 + 测试），
  // 旧的 cherry-pick 单 sha 只带最后一个增量，丢前面的。
  const { repo } = setupRepo()
  const wt = join(repo, '.wt')
  git(['worktree', 'add', '-q', '--detach', wt], repo)

  // commit A：建立文件 X（核心实现）
  writeFileSync(join(wt, 'src', 'X.rs'), 'pub fn core() {}\n')
  git(['add', '-A'], wt)
  git(['commit', '-q', '-m', 'preserve: add core X'], wt)
  // commit B：改 X + 加文件 Y（修正 + 测试）
  writeFileSync(join(wt, 'src', 'X.rs'), 'pub fn core() {}\npub fn fixed() {}\n')
  writeFileSync(join(wt, 'src', 'Y.rs'), 'pub fn test() {}\n')
  git(['add', '-A'], wt)
  git(['commit', '-q', '-m', 'preserve: fix X + add Y'], wt)
  // preserve ref = B（链顶）
  const sha = git(['rev-parse', 'HEAD'], wt)

  landFullDiff(repo, sha, 'land multi-commit chain')

  // 关键断言：X.rs 必须含 commit A 建立的事实（core）+ commit B 的修正（fixed）。
  // 旧 cherry-pick 单 B 会丢掉 A 建立 X.rs 的事实（B 相对 A 的 diff 只是把 core 行
  // 变成 core+fixed，不包含「创建文件」），导致 main 上根本没有 X.rs 或内容错误。
  assert.equal(
    readFileSync(join(repo, 'src', 'X.rs'), 'utf8'),
    'pub fn core() {}\npub fn fixed() {}\n',
    'X.rs 必须含 A 建立的 core + B 的 fixed',
  )
  assert.equal(
    readFileSync(join(repo, 'src', 'Y.rs'), 'utf8'),
    'pub fn test() {}\n',
    'Y.rs 必须含 B 的改动',
  )
})

test('merge-base 正确处理 main 已前进的情况', () => {
  // 场景：preserve 之后 main 又有了新 commit（别的 goal 落地）。
  // merge-base 应正确求出共同祖先，diff 只含 preserve 链的改动，不含 main 新 commit。
  const { repo } = setupRepo()
  const wt = join(repo, '.wt')
  git(['worktree', 'add', '-q', '--detach', wt], repo)
  writeFileSync(join(wt, 'src', 'feat.rs'), 'pub fn feat() {}\n')
  git(['add', '-A'], wt)
  git(['commit', '-q', '-m', 'preserve: feat'], wt)
  const sha = git(['rev-parse', 'HEAD'], wt)

  // main 上前进一个 commit（别的改动）
  writeFileSync(join(repo, 'src', 'lib.rs'), 'pub fn x() {}\n// main advanced\n')
  git(['add', '-A'], repo)
  git(['commit', '-q', '-m', 'main: unrelated advance'], repo)

  landFullDiff(repo, sha, 'land after main advanced')

  // preserve 的 feat.rs 落地；main 的 advance 不丢。
  assert.equal(readFileSync(join(repo, 'src', 'feat.rs'), 'utf8'), 'pub fn feat() {}\n')
  assert.equal(
    readFileSync(join(repo, 'src', 'lib.rs'), 'utf8'),
    'pub fn x() {}\n// main advanced\n',
    'main 自己的 advance 必须保留',
  )
})
