/**
 * Recursive E2E assertion plugins for ArgusAI.
 *
 * Registers three plugin step types:
 * - `recursive-session:` — Session JSONL structure validation
 * - `recursive-cost:` — Cost tracking validation
 * - `llm-judge:` — LLM-as-judge semantic evaluation
 */

import { execSync } from 'node:child_process';
import path from 'node:path';
import type { PluginModule } from 'argusai-core';
import { recursiveSessionPlugin } from './session-plugin.js';
import { recursiveCostPlugin } from './cost-plugin.js';
import { llmJudgePlugin } from './llm-judge-plugin.js';

const plugin: PluginModule = {
  name: 'recursive-agent',

  async setup() {
    // Auto-start aimock container if not running
    try {
      const running = execSync('docker ps --filter name=aimock --format "{{.Names}}"', { encoding: 'utf-8' }).trim();
      if (!running.includes('aimock')) {
        const fixturesDir = path.resolve(import.meta.dirname, '../../fixtures');
        const network = execSync('docker network ls --filter name=e2e-network --format "{{.Name}}"', { encoding: 'utf-8' }).trim();
        const networkFlag = network ? `--network ${network}` : '';
        execSync(
          `docker run -d --name aimock ${networkFlag} -v "${fixturesDir}:/fixtures" ghcr.io/copilotkit/aimock -f /fixtures -h 0.0.0.0`,
          { stdio: 'pipe' },
        );
        // Wait for aimock to be ready
        await new Promise(resolve => setTimeout(resolve, 2000));
        console.log('[recursive-agent] aimock container started');
      } else {
        console.log('[recursive-agent] aimock already running');
      }
    } catch (e) {
      console.warn(`[recursive-agent] aimock auto-start failed: ${(e as Error).message}`);
    }

    console.log('[recursive-agent] Plugin loaded — session, cost & llm-judge assertions registered');
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
  ],
};

export default plugin;
export { recursiveSessionPlugin } from './session-plugin.js';
export { recursiveCostPlugin } from './cost-plugin.js';
export { llmJudgePlugin } from './llm-judge-plugin.js';
