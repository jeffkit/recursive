/**
 * @module recursive-session plugin
 * Assertion plugin for Recursive agent session JSONL validation.
 *
 * Validates the session directory structure:
 * - .meta.json: session_id, goal, model, status, created_at, message_count
 * - transcript.jsonl: valid JSON lines with role, content, tool_calls
 */

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execSync } from 'node:child_process';
import type { AssertionPlugin, AssertionResult } from 'argusai-core';

// =====================================================================
// Types
// =====================================================================

export interface SessionAssertionConfig {
  /** Session status must equal this (e.g., "completed", "success") */
  status?: string | string[];
  /** Minimum message count in transcript */
  minMessages?: number;
  /** Maximum message count */
  maxMessages?: number;
  /** These tool names must appear in at least one tool_call */
  hasToolCalls?: string[];
  /** These tool names must NOT appear */
  noToolCalls?: string[];
  /** Transcript must have messages with these roles */
  hasRoles?: string[];
  /** Model name must match */
  model?: string;
  /** Whether session must be finalized (status != "active") */
  finalized?: boolean;
}

interface SessionMeta {
  session_id: string;
  goal: string;
  model: string;
  provider: string;
  created_at: string;
  updated_at: string;
  message_count: number;
  status: string;
}

interface TranscriptEntry {
  role: string;
  content: string;
  tool_calls?: Array<{ id: string; name: string; arguments: unknown }>;
}

// =====================================================================
// Plugin
// =====================================================================

export const recursiveSessionPlugin: AssertionPlugin = {
  name: 'recursive-session',

  assert(type: string, input: unknown, config: unknown): AssertionResult[] {
    if (type !== 'recursive-session') return [];

    // ArgusAI passes the entire step body as `input` when used as a plugin step type.
    // Extract the actual path from `input.input` and use remaining fields as config.
    let sessionDir: string;
    let options: SessionAssertionConfig;

    if (typeof input === 'object' && input !== null && 'input' in input) {
      const stepBody = input as Record<string, unknown>;
      sessionDir = stepBody.input as string;
      const container = stepBody.container as string | undefined;
      // Everything except 'container' and 'input' is assertion config
      const { container: _, input: __, ...rest } = stepBody;
      options = rest as SessionAssertionConfig;

      // If container specified, docker cp the path to a local tmpdir
      if (container && sessionDir.startsWith('/')) {
        sessionDir = copyFromContainer(container, sessionDir);
      }
    } else if (typeof input === 'string') {
      sessionDir = input;
      options = (config ?? {}) as SessionAssertionConfig;
    } else {
      return [{
        path: 'recursive-session',
        operator: 'parse',
        expected: 'string path or {input: string, ...config}',
        actual: typeof input,
        passed: false,
        message: `Invalid input: expected string or object with 'input' field, got ${typeof input}`,
      }];
    }

    return assertRecursiveSession(sessionDir, options);
  },
};

// =====================================================================
// Implementation
// =====================================================================

function assertRecursiveSession(
  sessionDir: string,
  options: SessionAssertionConfig,
): AssertionResult[] {
  const results: AssertionResult[] = [];
  const basePath = `session:${path.basename(sessionDir)}`;

  // Find session directory (may need to search)
  const resolvedDir = findSessionDir(sessionDir);
  if (!resolvedDir) {
    results.push({
      path: basePath,
      operator: 'exists',
      expected: 'session directory with .meta.json',
      actual: 'not found',
      passed: false,
      message: `No session directory found under ${sessionDir}`,
    });
    return results;
  }

  // Parse meta
  const metaPath = path.join(resolvedDir, '.meta.json');
  if (!fs.existsSync(metaPath)) {
    results.push({
      path: `${basePath}.meta`,
      operator: 'exists',
      expected: true,
      actual: false,
      passed: false,
      message: `.meta.json not found at ${metaPath}`,
    });
    return results;
  }

  let meta: SessionMeta;
  try {
    meta = JSON.parse(fs.readFileSync(metaPath, 'utf-8'));
  } catch (e) {
    results.push({
      path: `${basePath}.meta`,
      operator: 'parse',
      expected: 'valid JSON',
      actual: (e as Error).message,
      passed: false,
      message: `Failed to parse .meta.json: ${(e as Error).message}`,
    });
    return results;
  }

  // Required meta fields
  const requiredFields = ['session_id', 'goal', 'model', 'status', 'created_at', 'message_count'];
  const missing = requiredFields.filter(f => !(f in meta) || !(meta as unknown as Record<string, unknown>)[f]);
  results.push({
    path: `${basePath}.meta.fields`,
    operator: 'hasAll',
    expected: requiredFields,
    actual: missing.length === 0 ? 'all present' : `missing: ${missing.join(', ')}`,
    passed: missing.length === 0,
    message: missing.length === 0
      ? `Meta has all required fields`
      : `Meta missing: ${missing.join(', ')}`,
  });

  // Status check
  if (options.status !== undefined) {
    const acceptable = Array.isArray(options.status) ? options.status : [options.status];
    const passed = acceptable.includes(meta.status);
    results.push({
      path: `${basePath}.meta.status`,
      operator: 'in',
      expected: acceptable,
      actual: meta.status,
      passed,
      message: passed
        ? `Session status "${meta.status}" is acceptable`
        : `Session status "${meta.status}" not in [${acceptable.join(', ')}]`,
    });
  }

  // Model check
  if (options.model !== undefined) {
    const passed = meta.model === options.model;
    results.push({
      path: `${basePath}.meta.model`,
      operator: 'exact',
      expected: options.model,
      actual: meta.model,
      passed,
      message: passed ? `Model is "${options.model}"` : `Model "${meta.model}" != "${options.model}"`,
    });
  }

  // Parse transcript
  const transcriptPath = path.join(resolvedDir, 'transcript.jsonl');
  if (!fs.existsSync(transcriptPath)) {
    results.push({
      path: `${basePath}.transcript`,
      operator: 'exists',
      expected: true,
      actual: false,
      passed: false,
      message: `transcript.jsonl not found at ${transcriptPath}`,
    });
    return results;
  }

  let entries: TranscriptEntry[];
  try {
    const lines = fs.readFileSync(transcriptPath, 'utf-8').trim().split('\n');
    entries = lines.filter((l: string) => l.trim()).map((l: string) => JSON.parse(l));
    results.push({
      path: `${basePath}.transcript.parse`,
      operator: 'valid',
      expected: 'valid JSONL',
      actual: `${entries.length} entries`,
      passed: true,
      message: `Transcript has ${entries.length} valid JSONL entries`,
    });
  } catch (e) {
    results.push({
      path: `${basePath}.transcript.parse`,
      operator: 'valid',
      expected: 'valid JSONL',
      actual: (e as Error).message,
      passed: false,
      message: `Invalid JSONL: ${(e as Error).message}`,
    });
    return results;
  }

  // Message count
  if (options.minMessages !== undefined) {
    const passed = entries.length >= options.minMessages;
    results.push({
      path: `${basePath}.transcript.count`,
      operator: 'gte',
      expected: options.minMessages,
      actual: entries.length,
      passed,
      message: passed
        ? `${entries.length} messages >= ${options.minMessages}`
        : `Only ${entries.length} messages (expected >= ${options.minMessages})`,
    });
  }

  if (options.maxMessages !== undefined) {
    const passed = entries.length <= options.maxMessages;
    results.push({
      path: `${basePath}.transcript.count`,
      operator: 'lte',
      expected: options.maxMessages,
      actual: entries.length,
      passed,
      message: passed
        ? `${entries.length} messages <= ${options.maxMessages}`
        : `${entries.length} messages exceeds max ${options.maxMessages}`,
    });
  }

  // Roles check
  if (options.hasRoles) {
    const actualRoles = new Set(entries.map(e => e.role));
    for (const role of options.hasRoles) {
      const passed = actualRoles.has(role);
      results.push({
        path: `${basePath}.transcript.roles`,
        operator: 'contains',
        expected: role,
        actual: passed ? '(found)' : `(not in [${[...actualRoles].join(', ')}])`,
        passed,
        message: passed
          ? `Role "${role}" found in transcript`
          : `Role "${role}" missing (found: ${[...actualRoles].join(', ')})`,
      });
    }
  }

  // Tool calls check
  if (options.hasToolCalls) {
    const allTools = new Set<string>();
    for (const entry of entries) {
      if (entry.tool_calls) {
        for (const tc of entry.tool_calls) {
          allTools.add(tc.name);
        }
      }
    }
    for (const tool of options.hasToolCalls) {
      const passed = allTools.has(tool);
      results.push({
        path: `${basePath}.transcript.tool_calls`,
        operator: 'contains',
        expected: tool,
        actual: passed ? '(found)' : `(not in [${[...allTools].join(', ')}])`,
        passed,
        message: passed
          ? `Tool "${tool}" was called`
          : `Tool "${tool}" never called (found: ${[...allTools].join(', ')})`,
      });
    }
  }

  // No tool calls check
  if (options.noToolCalls) {
    const allTools = new Set<string>();
    for (const entry of entries) {
      if (entry.tool_calls) {
        for (const tc of entry.tool_calls) {
          allTools.add(tc.name);
        }
      }
    }
    for (const tool of options.noToolCalls) {
      const passed = !allTools.has(tool);
      results.push({
        path: `${basePath}.transcript.tool_calls`,
        operator: 'not_contains',
        expected: `not "${tool}"`,
        actual: passed ? '(absent)' : '(found)',
        passed,
        message: passed
          ? `Tool "${tool}" correctly not called`
          : `Tool "${tool}" was unexpectedly called`,
      });
    }
  }

  // Finalized check
  if (options.finalized === true) {
    const passed = meta.status !== 'active';
    results.push({
      path: `${basePath}.meta.finalized`,
      operator: 'neq',
      expected: 'not active',
      actual: meta.status,
      passed,
      message: passed
        ? `Session finalized (status="${meta.status}")`
        : `Session still active (not finalized)`,
    });
  }

  return results;
}

/**
 * Find session directory by searching for .meta.json recursively.
 * Handles the nested structure: sessions/<slug>/<session-id>/.meta.json
 */
function findSessionDir(baseDir: string): string | null {
  if (!fs.existsSync(baseDir)) return null;

  // Direct check
  if (fs.existsSync(path.join(baseDir, '.meta.json'))) {
    return baseDir;
  }

  // Search recursively (max 3 levels)
  const search = (dir: string, depth: number): string | null => {
    if (depth > 3) return null;
    try {
      const entries = fs.readdirSync(dir, { withFileTypes: true });
      for (const entry of entries) {
        if (!entry.isDirectory()) continue;
        const subDir = path.join(dir, entry.name);
        if (fs.existsSync(path.join(subDir, '.meta.json'))) {
          return subDir;
        }
        const found = search(subDir, depth + 1);
        if (found) return found;
      }
    } catch { /* ignore permission errors */ }
    return null;
  };

  return search(baseDir, 0);
}

/**
 * Copy a path from a Docker container to a local temp directory.
 * Returns the local path to the copied content.
 */
function copyFromContainer(container: string, containerPath: string): string {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'argusai-session-'));
  const localPath = path.join(tmpDir, path.basename(containerPath));
  try {
    execSync(`docker cp ${container}:${containerPath} ${localPath}`, { stdio: 'pipe' });
  } catch (e) {
    // If docker cp fails, return the tmpDir anyway (assertions will catch "not found")
    fs.rmSync(tmpDir, { recursive: true, force: true });
    return containerPath; // fallback to original path (will fail with descriptive error)
  }
  return localPath;
}
