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
    const realApiBase = process.env['DEEPSEEK_API_BASE'] ?? 'https://api.deepseek.com/v1';
    const apiKey = process.env['DEEPSEEK_API_KEY'] ?? '';

    // Auto-start aimock container, joining the correct Docker network.
    // If aimock is already running but on a different network (e.g. a stale
    // container from a previous worktree run), remove it and restart on the
    // correct network so the recursive-e2e container can reach it.
    try {
      // Determine the target network before checking if aimock is running.
      const allNetworks = execSync('docker network ls --format "{{.Name}}"', { encoding: 'utf-8' }).trim().split('\n');
      const worktreeId = process.env['WORKTREE_ID'];
      const namespacedNetwork = worktreeId ? `argusai-${worktreeId}-network` : null;
      const targetNetwork = (namespacedNetwork && allNetworks.includes(namespacedNetwork) ? namespacedNetwork : null)
        || (allNetworks.includes('e2e-network') ? 'e2e-network' : null)
        || '';
      const networkFlag = targetNetwork ? `--network ${targetNetwork}` : '';

      // Check if aimock is already on the correct network; remove if not.
      const running = execSync('docker ps --filter name=aimock --format "{{.Names}}"', { encoding: 'utf-8' }).trim();
      if (running.includes('aimock') && targetNetwork) {
        const aimockNetworks = execSync(
          'docker inspect aimock --format "{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}"',
          { encoding: 'utf-8' }
        ).trim();
        if (!aimockNetworks.includes(targetNetwork)) {
          // Stale aimock on wrong network — remove and let it restart below.
          execSync('docker rm -f aimock', { stdio: 'pipe' });
          console.log(`[recursive-agent] aimock was on wrong network (${aimockNetworks.trim()}); restarting on ${targetNetwork}`);
        }
      }

      const stillRunning = execSync('docker ps --filter name=aimock --format "{{.Names}}"', { encoding: 'utf-8' }).trim();
      if (!stillRunning.includes('aimock')) {
        const fixturesDir = path.resolve(import.meta.dirname, '../../fixtures');
        const recordedDir = path.resolve(fixturesDir, 'recorded');

        let aimockCmd: string;
        if (recordMode && apiKey) {
          // Record mode: proxy to real LLM, record responses to fixtures/recorded/
          execSync(`mkdir -p "${recordedDir}"`);
          aimockCmd = `docker run -d --name aimock ${networkFlag} ` +
            `-v "${fixturesDir}:/fixtures" ` +
            `-e "OPENAI_API_KEY=${apiKey}" ` +
            `ghcr.io/copilotkit/aimock ` +
            `--record --record-path /fixtures/recorded ` +
            `--provider-openai ${realApiBase} ` +
            `-f /fixtures -h 0.0.0.0`;
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
