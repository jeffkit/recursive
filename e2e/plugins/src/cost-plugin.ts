/**
 * @module recursive-cost plugin
 * Assertion plugin for Recursive agent cost.json validation.
 *
 * Validates cost tracking output:
 * - cost.json exists and is valid JSON
 * - Has token usage data (prompt_tokens, completion_tokens)
 * - Optional: cost_usd field, model field, budget check
 */

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execSync } from 'node:child_process';
import type { AssertionPlugin, AssertionResult } from 'argusai-core';

// =====================================================================
// Types
// =====================================================================

export interface CostAssertionConfig {
  /** cost.json must exist (default: true) */
  exists?: boolean;
  /** Minimum prompt tokens (> 0 means LLM was called) */
  minPromptTokens?: number;
  /** Maximum total tokens (budget guard) */
  maxTotalTokens?: number;
  /** Maximum cost in USD (budget guard) */
  maxCostUsd?: number;
  /** Model name must match */
  model?: string;
}

// =====================================================================
// Plugin
// =====================================================================

export const recursiveCostPlugin: AssertionPlugin = {
  name: 'recursive-cost',

  assert(type: string, input: unknown, config: unknown): AssertionResult[] {
    if (type !== 'recursive-cost') return [];

    // ArgusAI passes the entire step body as `input` when used as a plugin step type.
    let costPath: string;
    let options: CostAssertionConfig;

    if (typeof input === 'object' && input !== null && 'input' in input) {
      const stepBody = input as Record<string, unknown>;
      costPath = stepBody.input as string;
      const container = stepBody.container as string | undefined;
      const { container: _, input: __, ...rest } = stepBody;
      options = rest as CostAssertionConfig;

      if (container && costPath.startsWith('/')) {
        costPath = copyFromContainer(container, costPath);
      }
    } else if (typeof input === 'string') {
      costPath = input;
      options = (config ?? {}) as CostAssertionConfig;
    } else {
      return [{
        path: 'recursive-cost',
        operator: 'parse',
        expected: 'string path or {input: string, ...config}',
        actual: typeof input,
        passed: false,
        message: `Invalid input: expected string or object with 'input' field, got ${typeof input}`,
      }];
    }

    return assertRecursiveCost(costPath, options);
  },
};

// =====================================================================
// Implementation
// =====================================================================

function assertRecursiveCost(
  inputPath: string,
  options: CostAssertionConfig,
): AssertionResult[] {
  const results: AssertionResult[] = [];
  const basePath = 'cost';

  // Find cost.json (input may be a session dir or direct file path)
  const costFilePath = findCostJson(inputPath);

  // Existence check
  if (options.exists === false) {
    // Expect it to NOT exist
    const passed = !costFilePath;
    results.push({
      path: `${basePath}.exists`,
      operator: 'eq',
      expected: false,
      actual: !!costFilePath,
      passed,
      message: passed ? 'cost.json correctly absent' : `cost.json unexpectedly exists at ${costFilePath}`,
    });
    return results;
  }

  if (!costFilePath) {
    results.push({
      path: `${basePath}.exists`,
      operator: 'exists',
      expected: true,
      actual: false,
      passed: false,
      message: `cost.json not found under ${inputPath}`,
    });
    return results;
  }

  results.push({
    path: `${basePath}.exists`,
    operator: 'exists',
    expected: true,
    actual: true,
    passed: true,
    message: `cost.json found at ${costFilePath}`,
  });

  // Parse
  let data: Record<string, unknown>;
  try {
    data = JSON.parse(fs.readFileSync(costFilePath, 'utf-8'));
  } catch (e) {
    results.push({
      path: `${basePath}.parse`,
      operator: 'valid',
      expected: 'valid JSON',
      actual: (e as Error).message,
      passed: false,
      message: `Failed to parse cost.json: ${(e as Error).message}`,
    });
    return results;
  }

  // Extract token values (handle both flat and nested formats)
  let promptTokens = 0;
  let completionTokens = 0;
  let totalTokens = 0;

  if ('tokens_prompt' in data) {
    promptTokens = data.tokens_prompt as number;
    completionTokens = (data.tokens_completion as number) ?? 0;
    totalTokens = promptTokens + completionTokens;
  } else if ('total_usage' in data && typeof data.total_usage === 'object') {
    const usage = data.total_usage as Record<string, number>;
    promptTokens = usage.prompt_tokens ?? 0;
    completionTokens = usage.completion_tokens ?? 0;
    totalTokens = usage.total_tokens ?? (promptTokens + completionTokens);
  }

  // Min prompt tokens
  if (options.minPromptTokens !== undefined) {
    const passed = promptTokens >= options.minPromptTokens;
    results.push({
      path: `${basePath}.prompt_tokens`,
      operator: 'gte',
      expected: options.minPromptTokens,
      actual: promptTokens,
      passed,
      message: passed
        ? `prompt_tokens ${promptTokens} >= ${options.minPromptTokens}`
        : `prompt_tokens ${promptTokens} < ${options.minPromptTokens}`,
    });
  }

  // Max total tokens (budget guard)
  if (options.maxTotalTokens !== undefined) {
    const passed = totalTokens <= options.maxTotalTokens;
    results.push({
      path: `${basePath}.total_tokens`,
      operator: 'lte',
      expected: options.maxTotalTokens,
      actual: totalTokens,
      passed,
      message: passed
        ? `total_tokens ${totalTokens} <= ${options.maxTotalTokens} (budget OK)`
        : `total_tokens ${totalTokens} EXCEEDS budget of ${options.maxTotalTokens}`,
    });
  }

  // Max cost USD
  if (options.maxCostUsd !== undefined) {
    const costUsd = (data.cost_usd as number) ?? 0;
    const passed = costUsd <= options.maxCostUsd;
    results.push({
      path: `${basePath}.cost_usd`,
      operator: 'lte',
      expected: options.maxCostUsd,
      actual: costUsd,
      passed,
      message: passed
        ? `cost $${costUsd.toFixed(4)} <= $${options.maxCostUsd} (budget OK)`
        : `cost $${costUsd.toFixed(4)} EXCEEDS budget of $${options.maxCostUsd}`,
    });
  }

  // Model check
  if (options.model !== undefined) {
    const model = data.model as string | undefined;
    const passed = model === options.model;
    results.push({
      path: `${basePath}.model`,
      operator: 'exact',
      expected: options.model,
      actual: model ?? '(not set)',
      passed,
      message: passed ? `Model is "${options.model}"` : `Model "${model}" != "${options.model}"`,
    });
  }

  return results;
}

/**
 * Find cost.json by searching the given path.
 * Handles: direct file path, session directory, or workspace sessions root.
 */
function findCostJson(inputPath: string): string | null {
  // Direct file
  if (inputPath.endsWith('cost.json') && fs.existsSync(inputPath)) {
    return inputPath;
  }

  // Search recursively
  if (!fs.existsSync(inputPath)) return null;

  const stat = fs.statSync(inputPath);
  if (!stat.isDirectory()) return null;

  // Check direct child
  const direct = path.join(inputPath, 'cost.json');
  if (fs.existsSync(direct)) return direct;

  // Recursive search (max 4 levels)
  const search = (dir: string, depth: number): string | null => {
    if (depth > 4) return null;
    try {
      const entries = fs.readdirSync(dir, { withFileTypes: true });
      for (const entry of entries) {
        const fullPath = path.join(dir, entry.name);
        if (!entry.isDirectory() && entry.name === 'cost.json') {
          return fullPath;
        }
      }
      for (const entry of entries) {
        if (entry.isDirectory()) {
          const found = search(path.join(dir, entry.name), depth + 1);
          if (found) return found;
        }
      }
    } catch { /* ignore */ }
    return null;
  };

  return search(inputPath, 0);
}

function copyFromContainer(container: string, containerPath: string): string {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'argusai-cost-'));
  const localPath = path.join(tmpDir, path.basename(containerPath));
  try {
    execSync(`docker cp ${container}:${containerPath} ${localPath}`, { stdio: 'pipe' });
  } catch {
    fs.rmSync(tmpDir, { recursive: true, force: true });
    return containerPath;
  }
  return localPath;
}
