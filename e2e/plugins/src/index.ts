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

    // Auto-start aimock container if not running
    try {
      const running = execSync('docker ps --filter name=aimock --format "{{.Names}}"', { encoding: 'utf-8' }).trim();
      if (!running.includes('aimock')) {
        const fixturesDir = path.resolve(import.meta.dirname, '../../fixtures');
        const recordedDir = path.resolve(fixturesDir, 'recorded');
        const network = execSync('docker network ls --filter name=e2e-network --format "{{.Name}}"', { encoding: 'utf-8' }).trim();
        const networkFlag = network ? `--network ${network}` : '';

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
