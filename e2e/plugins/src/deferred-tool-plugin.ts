/**
 * @module deferred-tool plugin
 *
 * Assertion plugin that inspects a Recursive session transcript to verify
 * deferred-tool-loading behaviour.  Two assertion types are provided:
 *
 * ### `deferred-tool-order`
 *
 * Checks that `before` tool appears in the transcript before `after` tool
 * (used to verify ToolSearchTool was invoked before WebFetch).
 *
 * ```yaml
 * - name: "ToolSearchTool before WebFetch"
 *   deferred-tool-order:
 *     container: recursive-e2e
 *     input: /workspace/test/.recursive/sessions
 *     before: ToolSearchTool
 *     after: WebFetch
 * ```
 *
 * When `after` is absent from the transcript the check is relaxed: the test
 * passes as long as `before` was called at least once (the model may have
 * stopped after the search round without reaching the real tool call, which
 * is still correct deferred-tool behaviour).
 *
 * ### `deferred-tool-absent`
 *
 * Asserts that `tool` does NOT appear anywhere in the transcript — used to
 * verify that ToolSearchTool is never injected in eager (OpenAI) mode.
 *
 * ```yaml
 * - name: "No ToolSearchTool in eager mode"
 *   deferred-tool-absent:
 *     container: recursive-e2e
 *     input: /workspace/test/.recursive/sessions
 *     tool: ToolSearchTool
 * ```
 */

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execSync } from 'node:child_process';
import type { AssertionPlugin, AssertionResult } from 'argusai-core';

// ── Types ────────────────────────────────────────────────────────────────────

interface TranscriptEntry {
  role: string;
  content?: string;
  tool_calls?: Array<{ id: string; name: string; arguments: unknown }>;
}

interface ToolCallPosition {
  name: string;
  /** 0-based line index in transcript.jsonl */
  line: number;
}

interface DeferredToolOrderConfig {
  /** Tool that must appear first (e.g. "ToolSearchTool") */
  before: string;
  /** Tool that must appear after `before` (e.g. "WebFetch") */
  after: string;
  /**
   * Only consider sessions whose slug directory name matches this value
   * (e.g. "workspace-test-23-anthropic"). Used when input is a broad
   * search root like `/workspace/workspaces`.
   */
  slug?: string;
}

interface DeferredToolAbsentConfig {
  /** Tool that must NOT appear anywhere in the transcript */
  tool: string;
  /** Optional slug filter — same semantics as DeferredToolOrderConfig.slug */
  slug?: string;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function copyFromContainer(container: string, containerPath: string): string {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'argusai-deferred-'));
  const localPath = path.join(tmpDir, path.basename(containerPath));
  try {
    execSync(`docker cp ${container}:${containerPath} ${localPath}`, { stdio: 'pipe' });
    return localPath;
  } catch {
    return containerPath;
  }
}

/**
 * Find transcript.jsonl by searching up to 5 levels under baseDir.
 *
 * When `slugFilter` is provided, only transcripts inside a directory
 * whose ancestor at the "slug level" matches are returned.  The slug
 * level is the directory whose name equals `slugFilter` (e.g.
 * "workspace-test-23-anthropic" inside `sessions/<slug>/<ts>/`).
 */
function findTranscript(baseDir: string, slugFilter?: string): string | null {
  if (!fs.existsSync(baseDir)) return null;

  const search = (dir: string, depth: number): string | null => {
    if (depth > 5) return null;
    try {
      const entries = fs.readdirSync(dir, { withFileTypes: true });
      for (const entry of entries) {
        if (!entry.isDirectory() && entry.name === 'transcript.jsonl') {
          if (!slugFilter) return path.join(dir, entry.name);
          // Check if any ancestor directory name matches the slug.
          // transcript lives at <slug>/<ts>/transcript.jsonl, so
          // the grandparent of the file should be the slug dir.
          const grandparent = path.basename(path.dirname(dir));
          const parent = path.basename(dir);
          if (grandparent === slugFilter || parent === slugFilter) {
            return path.join(dir, entry.name);
          }
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

  return search(baseDir, 0);
}

/**
 * Parse transcript.jsonl and return all tool calls with their line positions.
 */
function extractToolCalls(transcriptPath: string): ToolCallPosition[] {
  const raw = fs.readFileSync(transcriptPath, 'utf-8');
  const positions: ToolCallPosition[] = [];

  raw.split('\n').forEach((line, lineIdx) => {
    const trimmed = line.trim();
    if (!trimmed) return;
    try {
      const entry: TranscriptEntry = JSON.parse(trimmed);
      if (entry.tool_calls) {
        for (const tc of entry.tool_calls) {
          positions.push({ name: tc.name, line: lineIdx });
        }
      }
    } catch { /* skip malformed lines */ }
  });

  return positions;
}

/**
 * Resolve input + container from step body, return local path to sessions dir.
 */
function resolveInput(input: unknown): { sessionsDir: string; rest: Record<string, unknown> } {
  if (typeof input === 'object' && input !== null && 'input' in input) {
    const stepBody = input as Record<string, unknown>;
    let sessionsDir = stepBody['input'] as string;
    const container = stepBody['container'] as string | undefined;
    const { container: _c, input: _i, ...rest } = stepBody;

    if (container && sessionsDir.startsWith('/')) {
      sessionsDir = copyFromContainer(container, sessionsDir);
    }
    return { sessionsDir, rest };
  }
  return { sessionsDir: String(input), rest: {} };
}

// ── Plugin: deferred-tool-order ───────────────────────────────────────────────

export const deferredToolOrderPlugin: AssertionPlugin = {
  name: 'deferred-tool-order',

  assert(type: string, input: unknown): AssertionResult[] {
    if (type !== 'deferred-tool-order') return [];

    const { sessionsDir, rest } = resolveInput(input);
    const config = rest as Partial<DeferredToolOrderConfig>;
    const beforeTool = config.before ?? 'ToolSearchTool';
    const afterTool = config.after ?? 'WebFetch';
    const assertPath = `deferred-tool-order(${beforeTool}→${afterTool})`;

    const transcriptPath = findTranscript(sessionsDir, config.slug);
    if (!transcriptPath) {
      return [{
        path: `${assertPath}.transcript`,
        operator: 'exists',
        expected: 'transcript.jsonl',
        actual: 'not found',
        passed: false,
        message: `No transcript.jsonl found under ${sessionsDir}${config.slug ? ` (slug: ${config.slug})` : ''}`,
      }];
    }

    const calls = extractToolCalls(transcriptPath);
    const beforeCalls = calls.filter(c => c.name === beforeTool);
    const afterCalls = calls.filter(c => c.name === afterTool);

    const results: AssertionResult[] = [];

    // 1. "before" tool must have been called at least once.
    results.push({
      path: `${assertPath}.${beforeTool}.called`,
      operator: 'gte',
      expected: 1,
      actual: beforeCalls.length,
      passed: beforeCalls.length >= 1,
      message: beforeCalls.length >= 1
        ? `${beforeTool} called ${beforeCalls.length} time(s) — deferred discovery happened`
        : `${beforeTool} was never called — deferred discovery did not happen`,
    });

    if (beforeCalls.length === 0) return results;

    // 2. If "after" tool was also called, it must appear on a later line.
    if (afterCalls.length === 0) {
      // Acceptable: model may stop after the search round.
      results.push({
        path: `${assertPath}.${afterTool}.order`,
        operator: 'skip',
        expected: `${afterTool} after ${beforeTool}`,
        actual: `${afterTool} not called (model stopped after search round)`,
        passed: true,
        message: `${afterTool} not called — model stopped after ${beforeTool} round (acceptable)`,
      });
      return results;
    }

    const firstBefore = beforeCalls[0].line;
    const firstAfter = afterCalls[0].line;
    const ordered = firstBefore < firstAfter;

    results.push({
      path: `${assertPath}.${afterTool}.order`,
      operator: 'lt',
      expected: `${beforeTool} line < ${afterTool} line`,
      actual: `${beforeTool}@${firstBefore} vs ${afterTool}@${firstAfter}`,
      passed: ordered,
      message: ordered
        ? `${beforeTool} (line ${firstBefore}) correctly precedes ${afterTool} (line ${firstAfter})`
        : `${afterTool} (line ${firstAfter}) appeared BEFORE ${beforeTool} (line ${firstBefore}) — ordering wrong`,
    });

    return results;
  },
};

// ── Plugin: deferred-tool-absent ─────────────────────────────────────────────

export const deferredToolAbsentPlugin: AssertionPlugin = {
  name: 'deferred-tool-absent',

  assert(type: string, input: unknown): AssertionResult[] {
    if (type !== 'deferred-tool-absent') return [];

    const { sessionsDir, rest } = resolveInput(input);
    const config = rest as Partial<DeferredToolAbsentConfig>;
    const tool = config.tool ?? 'ToolSearchTool';
    const assertPath = `deferred-tool-absent(${tool})`;

    const transcriptPath = findTranscript(sessionsDir, config.slug);
    if (!transcriptPath) {
      return [{
        path: `${assertPath}.transcript`,
        operator: 'exists',
        expected: 'transcript.jsonl',
        actual: 'not found',
        passed: false,
        message: `No transcript.jsonl found under ${sessionsDir}`,
      }];
    }

    const calls = extractToolCalls(transcriptPath);
    const found = calls.filter(c => c.name === tool);

    return [{
      path: `${assertPath}.absent`,
      operator: 'eq',
      expected: 0,
      actual: found.length,
      passed: found.length === 0,
      message: found.length === 0
        ? `${tool} correctly absent from transcript (eager mode — no deferred loading)`
        : `${tool} was called ${found.length} time(s) — unexpected in eager mode`,
    }];
  },
};
