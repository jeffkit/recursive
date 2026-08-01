/**
 * Recursive E2E assertion plugins for ArgusAI.
 *
 * Registers four plugin step types:
 * - `recursive-session:` — Session JSONL structure validation
 * - `recursive-cost:` — Cost tracking validation
 * - `llm-judge:` — LLM-as-judge semantic evaluation
 * - `agent-judge:` — Agent-as-judge evaluation with tool use + structured evidence
 *
 * Supports two modes (controlled by E2E_RECORD env var):
 * - replay (default): aimock serves fixtures from /fixtures directory
 * - record (E2E_RECORD=1): aimock proxies to real LLM, records responses
 */

import { execSync } from 'node:child_process';
import path from 'node:path';
import type { PluginModule } from 'argusai-core';
import { recursiveSessionPlugin } from './session-plugin.js';
import { recursiveCostPlugin } from './cost-plugin.js';
import { llmJudgePlugin } from './llm-judge-plugin.js';
import { agentJudgePlugin } from './agent-judge-plugin.js';
import { deferredToolOrderPlugin, deferredToolAbsentPlugin } from './deferred-tool-plugin.js';

const plugin: PluginModule = {
  name: 'recursive-agent',

  async setup() {
    const recordMode = process.env['E2E_RECORD'] === '1';
    // aimock's `--provider-openai` appends `/v1/chat/completions` itself, so
    // strip a trailing `/v1` from the configured base to avoid `/v1/v1/`.
    const rawBase = process.env['DEEPSEEK_API_BASE'] ?? 'https://api.deepseek.com/v1';
    const realApiBase = rawBase.replace(/\/v1\/?$/, '');
    const apiKey = process.env['DEEPSEEK_API_KEY'] ?? '';

    // Auto-start aimock container, joining the correct Docker network.
    // If aimock is already running but on a different network (e.g. a stale
    // container from a previous worktree run), remove it and restart on the
    // correct network so the recursive-e2e container can reach it.
    //
    // The plugin is the sole owner of the aimock container: e2e.yaml's
    // `mocks` section no longer declares aimock (argus-setup would otherwise
    // clobber it with replay-only args and could not inject OPENAI_API_KEY).
    // The network name mirrors argusai's deriveNetworkName so the service
    // container (started by argus-setup) lands on the same network:
    //   WORKTREE_ID set   → argusai-<WORKTREE_ID>-network
    //   WORKTREE_ID unset → argusai-<project-slug>-network (recursive-agent)
    // The network is created here if missing (argus-setup's ensureNetwork is
    // a no-op when it already exists), so record mode works on the MCP path
    // exactly like on the CLI path.
    try {
      const allNetworks = execSync('docker network ls --format "{{.Name}}"', { encoding: 'utf-8' }).trim().split('\n');
      const worktreeId = process.env['WORKTREE_ID'];
      const projectSlug = 'recursive-agent';
      const candidateNetworks = [
        worktreeId ? `argusai-${worktreeId}-network` : null,
        `argusai-${projectSlug}-network`,
        'e2e-network',
      ].filter((n): n is string => n !== null);
      // Prefer an already-existing candidate (any of them); otherwise create
      // the one argus-setup will use, derived from WORKTREE_ID/project slug.
      const targetNetwork = candidateNetworks.find((n) => allNetworks.includes(n))
        ?? (worktreeId ? `argusai-${worktreeId}-network` : `argusai-${projectSlug}-network`);
      if (!allNetworks.includes(targetNetwork)) {
        try {
          execSync(`docker network create "${targetNetwork}"`, { stdio: 'pipe' });
          console.log(`[recursive-agent] created network ${targetNetwork}`);
        } catch (createErr) {
          // Network may have been created concurrently (e.g. argus-setup);
          // verify it exists now before failing.
          const check = execSync('docker network ls --format "{{.Name}}"', { encoding: 'utf-8' }).trim().split('\n');
          if (!check.includes(targetNetwork)) {
            throw createErr;
          }
          console.log(`[recursive-agent] network ${targetNetwork} already exists`);
        }
      }
      const networkFlag = `--network ${targetNetwork}`;

      // Desired aimock mode: record only when E2E_RECORD=1 AND a real key is
      // present (otherwise record mode would proxy with no key and 401).
      const wantRecord = recordMode && !!apiKey;

      // Check if aimock is already running; if so, verify BOTH its network and
      // its mode. A stale aimock from a previous run (argus-clean does NOT
      // touch the plugin-owned container — plugin teardown is not invoked on
      // the MCP path) would otherwise silently replay old fixtures while the
      // user believes they are recording: a false green. Remove and restart if
      // either property mismatches.
      const running = execSync('docker ps --filter name=aimock --format "{{.Names}}"', { encoding: 'utf-8' }).trim();
      let restart = false;
      if (running.includes('aimock')) {
        // NOTE: use `{{json ...}}` (no `$` template variables) — docker's
        // `{{range $k, $v := ...}}` templates get their `$k`/`$v` expanded to
        // empty by the shell inside execSync, producing a silent template
        // parse error (exit 64) that the outer try/catch swallows. That made
        // the stale-aimock check a no-op and record mode quietly replay old
        // fixtures: the exact false-green this check exists to prevent.
        const aimockNetworks = Object.keys(
          JSON.parse(execSync('docker inspect aimock --format "{{json .NetworkSettings.Networks}}"', { encoding: 'utf-8' }).trim())
        );
        const aimockCmd = execSync('docker inspect aimock --format "{{.Config.Cmd}}"', { encoding: 'utf-8' }).trim();
        const isRecord = aimockCmd.includes('--record');
        if (!aimockNetworks.includes(targetNetwork)) {
          console.log(`[recursive-agent] aimock on wrong network (${aimockNetworks.join(', ')}); restarting on ${targetNetwork}`);
          restart = true;
        } else if (wantRecord !== isRecord) {
          console.log(`[recursive-agent] aimock running in ${isRecord ? 'RECORD' : 'REPLAY'} mode but ${wantRecord ? 'RECORD' : 'REPLAY'} is requested; restarting`);
          restart = true;
        }
        if (restart) {
          execSync('docker rm -f aimock', { stdio: 'pipe' });
        }
      }

      const stillRunning = execSync('docker ps --filter name=aimock --format "{{.Names}}"', { encoding: 'utf-8' }).trim();
      if (!stillRunning.includes('aimock')) {
        const fixturesDir = path.resolve(import.meta.dirname, '../../fixtures');
        const recordedDir = path.resolve(fixturesDir, 'recorded');

        let aimockCmd: string;
        if (wantRecord) {
          // Record mode: proxy to real LLM, record new fixtures. aimock's
          // `--record` saves unmatched requests to the FIRST `-f` path, so we
          // put the recorded/ dir first (writable) and the curated /fixtures
          // second (existing fixtures still replay). Note: aimock has no
          // `--record-path` flag — the first `-f` IS the record target.
          execSync(`mkdir -p "${recordedDir}"`);
          aimockCmd = `docker run -d --name aimock ${networkFlag} ` +
            `-v "${fixturesDir}:/fixtures" ` +
            `-e "OPENAI_API_KEY=${apiKey}" ` +
            `ghcr.io/copilotkit/aimock ` +
            `--record ` +
            `--provider-openai ${realApiBase} ` +
            `-f /fixtures/recorded -f /fixtures -h 0.0.0.0`;
          console.log('[recursive-agent] aimock starting in RECORD mode (proxying to real LLM)');
        } else {
          // Replay mode: serve fixtures deterministically
          aimockCmd = `docker run -d --name aimock ${networkFlag} ` +
            `-v "${fixturesDir}:/fixtures" ` +
            `ghcr.io/copilotkit/aimock -f /fixtures -h 0.0.0.0`;
          console.log('[recursive-agent] aimock starting in REPLAY mode');
        }

        execSync(aimockCmd, { stdio: 'pipe' });
        // Wait for aimock to be ready
        await new Promise(resolve => setTimeout(resolve, 2000));
        console.log('[recursive-agent] aimock container started');
      } else {
        console.log('[recursive-agent] aimock already running');
      }
    } catch (e) {
      console.warn(`[recursive-agent] aimock auto-start failed: ${(e as Error).message}`);
    }

    if (recordMode) {
      console.log('[recursive-agent] Plugin loaded (RECORD mode) — session, cost, llm-judge & agent-judge assertions registered');
    } else {
      console.log('[recursive-agent] Plugin loaded — session, cost, llm-judge & agent-judge assertions registered');
    }
  },

  async teardown() {
    // Stop aimock container
    try {
      execSync('docker rm -f aimock', { stdio: 'pipe' });
      console.log('[recursive-agent] aimock container stopped');
    } catch { /* ignore if not running */ }
  },

  assertionPlugins: [
    recursiveSessionPlugin,
    recursiveCostPlugin,
    llmJudgePlugin,
    agentJudgePlugin,
    deferredToolOrderPlugin,
    deferredToolAbsentPlugin,
  ],
};

export default plugin;
export { recursiveSessionPlugin } from './session-plugin.js';
export { recursiveCostPlugin } from './cost-plugin.js';
export { llmJudgePlugin } from './llm-judge-plugin.js';
export { agentJudgePlugin } from './agent-judge-plugin.js';
export { deferredToolOrderPlugin, deferredToolAbsentPlugin } from './deferred-tool-plugin.js';
