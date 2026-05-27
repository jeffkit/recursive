/**
 * @module llm-judge plugin
 * LLM-as-judge assertion: uses a real LLM to semantically evaluate agent behavior.
 *
 * Usage in YAML:
 * ```yaml
 * - name: "Judge approves"
 *   llm-judge:
 *     container: recursive-e2e
 *     input: /workspace/test/.recursive/sessions
 *     goal: "Create hello.txt with content 'world'"
 *     minScore: 3
 * ```
 */

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execSync } from 'node:child_process';
import type { AssertionPlugin, AssertionResult } from 'argusai-core';

export interface LlmJudgeConfig {
  goal: string;
  minScore?: number;
  requireCompleted?: boolean;
  apiBase?: string;
  apiKey?: string;
  model?: string;
}

export const llmJudgePlugin: AssertionPlugin = {
  name: 'llm-judge',

  assert(type: string, input: unknown, config: unknown): AssertionResult[] {
    if (type !== 'llm-judge') return [];

    let sessionsDir: string;
    let options: LlmJudgeConfig;

    if (typeof input === 'object' && input !== null && 'input' in input) {
      const stepBody = input as Record<string, unknown>;
      sessionsDir = stepBody.input as string;
      const container = stepBody.container as string | undefined;
      const { container: _, input: __, ...rest } = stepBody;
      options = rest as unknown as LlmJudgeConfig;

      if (container && sessionsDir.startsWith('/')) {
        sessionsDir = copyFromContainer(container, sessionsDir);
      }
    } else if (typeof input === 'string') {
      sessionsDir = input;
      options = (config ?? {}) as LlmJudgeConfig;
    } else {
      return [{
        path: 'llm-judge',
        operator: 'parse',
        expected: 'string or {input, goal, ...}',
        actual: typeof input,
        passed: false,
        message: `Invalid input type: ${typeof input}`,
      }];
    }

    // Check API key
    const apiKey = options.apiKey ?? process.env['JUDGE_API_KEY'] ?? process.env['DEEPSEEK_API_KEY'];
    if (!apiKey) {
      return [{
        path: 'llm-judge',
        operator: 'skip',
        expected: 'API key available',
        actual: 'no key',
        passed: true, // Skip gracefully, don't fail
        message: 'LLM Judge skipped: no JUDGE_API_KEY or DEEPSEEK_API_KEY set',
      }];
    }

    if (!options.goal) {
      return [{
        path: 'llm-judge.goal',
        operator: 'required',
        expected: 'goal string',
        actual: 'undefined',
        passed: false,
        message: 'llm-judge requires a "goal" field',
      }];
    }

    // Load transcript
    const transcriptPath = findFile(sessionsDir, 'transcript.jsonl');
    if (!transcriptPath) {
      return [{
        path: 'llm-judge.transcript',
        operator: 'exists',
        expected: 'transcript.jsonl',
        actual: 'not found',
        passed: false,
        message: `No transcript.jsonl found under ${sessionsDir}`,
      }];
    }

    // Build transcript summary
    const lines = fs.readFileSync(transcriptPath, 'utf-8').trim().split('\n');
    const msgs = lines.slice(0, 20).map(l => JSON.parse(l) as Record<string, unknown>);
    const summary = msgs.map((m, i) => {
      const role = m.role as string;
      const content = ((m.content as string) ?? '').slice(0, 300);
      const tools = (m.tool_calls as Array<{ name: string }> ?? []).map(t => t.name);
      const toolStr = tools.length ? ` [tools: ${tools.join(', ')}]` : '';
      return `[${i}] ${role}${toolStr}: ${content}`;
    }).join('\n');

    // Call judge LLM (synchronous for simplicity — uses child_process)
    const apiBase = options.apiBase ?? process.env['JUDGE_API_BASE'] ?? 'https://api.deepseek.com/v1';
    const model = options.model ?? process.env['JUDGE_MODEL'] ?? 'deepseek-chat';
    const minScore = options.minScore ?? 3;
    const requireCompleted = options.requireCompleted !== false;

    const prompt = `You are judging whether an AI agent completed its task.

Task: ${options.goal}

Agent transcript:
${summary}

Rate 1-5. Output ONLY JSON: {"completed": true/false, "score": 1-5, "reason": "brief explanation"}`;

    try {
      const payload = JSON.stringify({
        model,
        messages: [{ role: 'user', content: prompt }],
        temperature: 0.1,
        max_tokens: 150,
      });

      const result = execSync(
        `curl -s "${apiBase}/chat/completions" -H "Content-Type: application/json" -H "Authorization: Bearer ${apiKey}" -d '${payload.replace(/'/g, "'\\''")}'`,
        { encoding: 'utf-8', timeout: 30000 },
      );

      const data = JSON.parse(result) as { choices: Array<{ message: { content: string } }> };
      const content = data.choices?.[0]?.message?.content ?? '';

      const jsonMatch = content.match(/\{[\s\S]*\}/);
      if (!jsonMatch) {
        return [{
          path: 'llm-judge.response',
          operator: 'parse',
          expected: 'JSON response',
          actual: content.slice(0, 100),
          passed: false,
          message: `Judge returned non-JSON: ${content.slice(0, 100)}`,
        }];
      }

      const judgeResult = JSON.parse(jsonMatch[0]) as { completed?: boolean; score?: number; reason?: string };
      const score = judgeResult.score ?? 0;
      const completed = judgeResult.completed ?? false;
      const reason = judgeResult.reason ?? '';

      const results: AssertionResult[] = [];

      if (requireCompleted) {
        results.push({
          path: 'llm-judge.completed',
          operator: 'eq',
          expected: true,
          actual: completed,
          passed: completed,
          message: completed ? 'Agent completed the task' : `Agent did NOT complete: ${reason}`,
        });
      }

      results.push({
        path: 'llm-judge.score',
        operator: 'gte',
        expected: minScore,
        actual: score,
        passed: score >= minScore,
        message: score >= minScore
          ? `Score ${score}/5 >= ${minScore} (${reason})`
          : `Score ${score}/5 < ${minScore} (${reason})`,
      });

      return results;
    } catch (e) {
      return [{
        path: 'llm-judge.api',
        operator: 'call',
        expected: 'successful API call',
        actual: (e as Error).message,
        passed: false,
        message: `Judge API call failed: ${(e as Error).message}`,
      }];
    }
  },
};

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
    } catch { /* ignore */ }
    return null;
  };
  return search(dir, 0);
}

function copyFromContainer(container: string, containerPath: string): string {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'argusai-judge-'));
  const localPath = path.join(tmpDir, path.basename(containerPath));
  try {
    execSync(`docker cp ${container}:${containerPath} ${localPath}`, { stdio: 'pipe' });
  } catch {
    fs.rmSync(tmpDir, { recursive: true, force: true });
    return containerPath;
  }
  return localPath;
}
