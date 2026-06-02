/**
 * @module agent-judge plugin
 * Agent-as-judge assertion: spawns a Recursive agent to evaluate test outcomes
 * by reading session transcripts and output files, then produces a structured
 * verdict with evidence that can guide agent improvement.
 *
 * Differences from llm-judge:
 *  - The judge is a full Recursive agent (not a raw LLM call), so it can use
 *    tools (read_file, list_dir, search_files) to actively verify outputs.
 *  - Produces structured evidence alongside the verdict.
 *  - Requires a real `recursive` binary + API key; skips gracefully otherwise.
 *
 * Usage in YAML:
 * ```yaml
 * - name: "Agent Judge approves"
 *   agent-judge:
 *     container: recursive-e2e
 *     input: /workspace/test/.recursive/sessions
 *     workspace: /workspace/test
 *     goal: "Create hello.txt with content 'world'"
 *     minScore: 3
 * ```
 */

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execSync } from 'node:child_process';
import type { AssertionPlugin, AssertionResult } from 'argusai-core';

export interface AgentJudgeConfig {
  goal: string;
  workspace?: string;
  minScore?: number;
  requireCompleted?: boolean;
  apiBase?: string;
  apiKey?: string;
  model?: string;
  maxSteps?: number;
}

interface JudgeVerdict {
  completed?: boolean;
  score?: number;
  reason?: string;
  evidence?: string[];
}

export const agentJudgePlugin: AssertionPlugin = {
  name: 'agent-judge',

  assert(type: string, input: unknown, _config: unknown): AssertionResult[] {
    if (type !== 'agent-judge') return [];

    let sessionsDir: string;
    let workspaceDir: string | undefined;
    let options: AgentJudgeConfig;
    let tmpDir: string | undefined;

    if (typeof input === 'object' && input !== null && 'input' in input) {
      const stepBody = input as Record<string, unknown>;
      sessionsDir = stepBody.input as string;
      const container = stepBody.container as string | undefined;
      workspaceDir = stepBody.workspace as string | undefined;
      const { container: _c, input: _i, workspace: _w, ...rest } = stepBody;
      options = rest as unknown as AgentJudgeConfig;

      if (container) {
        tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'argusai-agent-judge-'));
        sessionsDir = copyFromContainer(container, sessionsDir, tmpDir, 'sessions');
        if (workspaceDir) {
          workspaceDir = copyFromContainer(container, workspaceDir, tmpDir, 'workspace');
        }
      }
    } else if (typeof input === 'string') {
      sessionsDir = input;
      options = (_config ?? {}) as AgentJudgeConfig;
      workspaceDir = options.workspace;
    } else {
      return [{
        path: 'agent-judge',
        operator: 'parse',
        expected: 'string or {input, goal, ...}',
        actual: typeof input,
        passed: false,
        message: `Invalid input type: ${typeof input}`,
      }];
    }

    const apiKey = options.apiKey
      ?? process.env['JUDGE_API_KEY']
      ?? process.env['DEEPSEEK_API_KEY'];

    if (!apiKey) {
      return [{
        path: 'agent-judge',
        operator: 'skip',
        expected: 'API key available',
        actual: 'no key',
        passed: true,
        message: 'Agent Judge skipped: no JUDGE_API_KEY or DEEPSEEK_API_KEY set',
      }];
    }

    if (!options.goal) {
      return [{
        path: 'agent-judge.goal',
        operator: 'required',
        expected: 'goal string',
        actual: 'undefined',
        passed: false,
        message: 'agent-judge requires a "goal" field',
      }];
    }

    const apiBase = options.apiBase
      ?? process.env['JUDGE_API_BASE']
      ?? process.env['DEEPSEEK_API_BASE']
      ?? 'https://api.deepseek.com/v1';
    const model = options.model ?? process.env['JUDGE_MODEL'] ?? 'deepseek-chat';
    const minScore = options.minScore ?? 3;
    const requireCompleted = options.requireCompleted !== false;
    const maxSteps = options.maxSteps ?? 8;

    // Find transcript
    const transcriptPath = findFile(sessionsDir, 'transcript.jsonl');
    if (!transcriptPath) {
      if (tmpDir) tryCleanup(tmpDir);
      return [{
        path: 'agent-judge.transcript',
        operator: 'exists',
        expected: 'transcript.jsonl',
        actual: 'not found',
        passed: false,
        message: `No transcript.jsonl found under ${sessionsDir}`,
      }];
    }

    // Create isolated workspace for the judge agent
    const judgeWorkspace = fs.mkdtempSync(path.join(os.tmpdir(), 'agent-judge-ws-'));

    try {
      // Copy transcript into judge workspace for read_file access
      const transcriptCopy = path.join(judgeWorkspace, 'transcript.jsonl');
      fs.copyFileSync(transcriptPath, transcriptCopy);

      // Copy tested workspace into judge workspace if available
      let testedWorkspacePath = '';
      if (workspaceDir && fs.existsSync(workspaceDir)) {
        const dest = path.join(judgeWorkspace, 'tested-workspace');
        fs.mkdirSync(dest, { recursive: true });
        copyDirLocal(workspaceDir, dest, 3);
        testedWorkspacePath = dest;
      }

      const workspaceNote = testedWorkspacePath
        ? `\nThe agent's output files are in: ${testedWorkspacePath}`
        : '';

      // Build judge prompt — agent must output ONLY raw JSON as final message
      const prompt = [
        `You are evaluating whether an AI agent completed its task correctly.`,
        ``,
        `Task assigned to the agent: "${options.goal}"`,
        ``,
        `The agent's session transcript is at: ${transcriptCopy}`,
        workspaceNote,
        ``,
        `Instructions:`,
        `1. Use read_file to read transcript.jsonl (each line is a JSON event)`,
        `2. Examine output files in the tested-workspace to verify actual results`,
        `3. Assess whether the agent genuinely completed the task`,
        ``,
        `Output ONLY valid JSON on the last line (no markdown, no prose after it):`,
        `{"completed": true/false, "score": 1-5, "reason": "brief", "evidence": ["finding1", "finding2"]}`,
        ``,
        `Score: 1=nothing done  2=attempted/failed  3=partial  4=mostly done  5=fully done`,
      ].filter(Boolean).join('\n');

      const safePrompt = prompt.replace(/'/g, "'\\''");
      const cmdOutput = execSync(
        `recursive --workspace ${judgeWorkspace} ` +
        `--api-base ${apiBase} ` +
        `--api-key ${apiKey} ` +
        `-m ${model} ` +
        `--max-steps ${maxSteps} ` +
        `run '${safePrompt}'`,
        { encoding: 'utf-8', timeout: 90000 },
      );

      const verdict = extractJudgeVerdict(cmdOutput);
      if (!verdict) {
        return [{
          path: 'agent-judge.verdict',
          operator: 'parse',
          expected: 'JSON verdict',
          actual: cmdOutput.slice(-400),
          passed: false,
          message: 'Agent judge did not produce a parseable JSON verdict',
        }];
      }

      const { completed, score = 0, reason = '', evidence = [] } = verdict;
      const evidenceStr = evidence.length > 0 ? evidence.join('; ') : '(none)';

      const results: AssertionResult[] = [];

      if (requireCompleted) {
        results.push({
          path: 'agent-judge.completed',
          operator: 'eq',
          expected: true,
          actual: !!completed,
          passed: !!completed,
          message: completed
            ? `Agent completed task. Reason: ${reason}`
            : `Agent did NOT complete task. Reason: ${reason}`,
        });
      }

      results.push({
        path: 'agent-judge.score',
        operator: 'gte',
        expected: minScore,
        actual: score,
        passed: score >= minScore,
        message: score >= minScore
          ? `Score ${score}/5 ≥ ${minScore}. Evidence: ${evidenceStr}`
          : `Score ${score}/5 < ${minScore}. ${reason}. Evidence: ${evidenceStr}`,
      });

      return results;
    } catch (e) {
      return [{
        path: 'agent-judge.run',
        operator: 'exec',
        expected: 'successful judge run',
        actual: (e as Error).message,
        passed: false,
        message: `Agent judge run failed: ${(e as Error).message}`,
      }];
    } finally {
      tryCleanup(judgeWorkspace);
      if (tmpDir) tryCleanup(tmpDir);
    }
  },
};

// ── Helpers ───────────────────────────────────────────────────────────────────

function extractJudgeVerdict(output: string): JudgeVerdict | null {
  // Scan output from the end; return the first valid verdict-shaped JSON
  const matches = [...output.matchAll(/\{[\s\S]*?\}/g)];
  for (let i = matches.length - 1; i >= 0; i--) {
    try {
      const obj = JSON.parse(matches[i][0]) as JudgeVerdict;
      if ('score' in obj || 'completed' in obj) return obj;
    } catch {
      // try next match
    }
  }
  return null;
}

function findFile(dir: string, filename: string): string | null {
  if (!fs.existsSync(dir)) return null;
  const stat = fs.statSync(dir);
  if (!stat.isDirectory()) return null;

  const search = (d: string, depth: number): string | null => {
    if (depth > 4) return null;
    try {
      const entries = fs.readdirSync(d, { withFileTypes: true });
      for (const entry of entries) {
        if (!entry.isDirectory() && entry.name === filename) {
          return path.join(d, entry.name);
        }
      }
      for (const entry of entries) {
        if (entry.isDirectory()) {
          const found = search(path.join(d, entry.name), depth + 1);
          if (found) return found;
        }
      }
    } catch {
      // ignore
    }
    return null;
  };
  return search(dir, 0);
}

function copyFromContainer(
  container: string,
  containerPath: string,
  tmpDir: string,
  localName: string,
): string {
  const localPath = path.join(tmpDir, localName);
  try {
    execSync(`docker cp ${container}:${containerPath} ${localPath}`, { stdio: 'pipe' });
    return localPath;
  } catch {
    return containerPath;
  }
}

function copyDirLocal(src: string, dest: string, maxDepth: number, depth = 0): void {
  if (depth > maxDepth) return;
  try {
    const entries = fs.readdirSync(src, { withFileTypes: true });
    for (const entry of entries) {
      const srcPath = path.join(src, entry.name);
      const destPath = path.join(dest, entry.name);
      if (entry.isDirectory()) {
        fs.mkdirSync(destPath, { recursive: true });
        copyDirLocal(srcPath, destPath, maxDepth, depth + 1);
      } else {
        try { fs.copyFileSync(srcPath, destPath); } catch { /* ignore */ }
      }
    }
  } catch {
    // ignore
  }
}

function tryCleanup(dir: string): void {
  try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* ignore */ }
}
