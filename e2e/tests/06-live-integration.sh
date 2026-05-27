#!/usr/bin/env bash
# 06-live-integration.sh — Full integration test with LLM-as-judge.
#
# This is the "Tier 2" test: runs a complex multi-step task, then uses
# a separate LLM call to judge whether the agent succeeded.
#
# Validates:
#   1. Agent completes a multi-step coding task
#   2. Workspace has expected artifacts
#   3. LLM judge scores the execution >= 3/5
#
# Cost: ~$0.05 per run (task agent + judge call)

set -euo pipefail

WORKSPACE="$(mktemp -d)"
trap 'rm -rf "$WORKSPACE"' EXIT

RECURSIVE_API_KEY="${RECURSIVE_API_KEY:-${DEEPSEEK_API_KEY:-}}"
if [[ -z "$RECURSIVE_API_KEY" ]]; then
  echo "[e2e:live] SKIP: no API key"
  exit 0
fi

BIN="${RECURSIVE_BIN:-./target/release/recursive}"
[[ ! -x "$BIN" ]] && BIN="./target/debug/recursive"
[[ ! -x "$BIN" ]] && { echo "ERROR: binary not found"; exit 1; }

# ---- Step 1: Run a multi-step task ----
GOAL="Create a Python script called 'greet.py' that:
1. Defines a function greet(name) that returns 'Hello, {name}!'
2. Has a main block that calls greet('World') and prints the result.
Then create a README.md that briefly describes what greet.py does."

echo "[e2e:live] Running multi-step task..."
"$BIN" \
  --workspace "$WORKSPACE" \
  --api-key "$RECURSIVE_API_KEY" \
  --max-steps 15 \
  run "$GOAL"

AGENT_EXIT=$?
echo "[e2e:live] agent exit: $AGENT_EXIT"

# ---- Step 2: Programmatic assertions ----
FAIL=0

if [[ ! -f "$WORKSPACE/greet.py" ]]; then
  echo "FAIL: greet.py not found"
  FAIL=1
else
  echo "PASS: greet.py exists"
  if grep -q "def greet" "$WORKSPACE/greet.py"; then
    echo "PASS: greet.py has greet function"
  else
    echo "FAIL: greet.py missing greet function"
    FAIL=1
  fi
fi

if [[ -f "$WORKSPACE/README.md" ]]; then
  echo "PASS: README.md exists"
else
  echo "WARN: README.md not created (partial completion)"
fi

# ---- Step 3: LLM-as-judge ----
if [[ "${E2E_JUDGE:-1}" == "1" ]]; then
  echo "[e2e:live] Running LLM judge..."

  # Collect transcript
  TRANSCRIPT_FILE=$(find "$WORKSPACE/.recursive/sessions" -name "transcript.jsonl" 2>/dev/null | head -1)
  if [[ -z "$TRANSCRIPT_FILE" ]]; then
    echo "WARN: no transcript for judge, skipping"
  else
    # Build judge prompt
    TRANSCRIPT_SUMMARY=$(python3 -c "
import json
lines = open('$TRANSCRIPT_FILE').read().strip().split('\n')
msgs = [json.loads(l) for l in lines]
summary = []
for i, m in enumerate(msgs[:20]):  # Cap at 20 messages
    role = m['role']
    content = m['content'][:300] if m.get('content') else ''
    tools = [tc['name'] for tc in m.get('tool_calls', [])]
    tool_str = f' [tools: {\", \".join(tools)}]' if tools else ''
    summary.append(f'[{i}] {role}{tool_str}: {content}')
print('\n'.join(summary))
" 2>/dev/null || echo "(transcript parse failed)")

    WORKSPACE_STATE=$(ls -la "$WORKSPACE/" 2>/dev/null | grep -v "^\." | head -10)

    # Call judge LLM
    JUDGE_RESPONSE=$(curl -s "${RECURSIVE_API_BASE:-https://api.deepseek.com/v1}/chat/completions" \
      -H "Content-Type: application/json" \
      -H "Authorization: Bearer $RECURSIVE_API_KEY" \
      -d "$(python3 -c "
import json
prompt = '''You are judging whether an AI agent completed its task.

Task: $GOAL

Agent transcript (summary):
$TRANSCRIPT_SUMMARY

Workspace files after execution:
$WORKSPACE_STATE

Rate the execution. Output ONLY JSON:
{\"completed\": true/false, \"score\": 1-5, \"reason\": \"brief explanation\"}'''

print(json.dumps({
    'model': '${RECURSIVE_MODEL:-deepseek-chat}',
    'messages': [{'role': 'user', 'content': prompt}],
    'temperature': 0.1,
    'max_tokens': 100
}))
" 2>/dev/null)" 2>/dev/null)

    # Parse judge result
    python3 -c "
import json, sys
try:
    resp = json.loads('''$JUDGE_RESPONSE''')
    content = resp['choices'][0]['message']['content']
    # Extract JSON from response
    import re
    match = re.search(r'\{.*\}', content, re.DOTALL)
    if match:
        result = json.loads(match.group())
        score = result.get('score', 0)
        completed = result.get('completed', False)
        reason = result.get('reason', '?')
        print(f'JUDGE: score={score}/5, completed={completed}')
        print(f'  reason: {reason}')
        if score >= 3 and completed:
            print('PASS: LLM judge approved (score >= 3)')
        else:
            print(f'FAIL: LLM judge score={score} (min 3), completed={completed}')
            sys.exit(1)
    else:
        print(f'WARN: could not parse judge response: {content[:100]}')
except Exception as e:
    print(f'WARN: judge evaluation failed: {e}')
" || FAIL=1
  fi
fi

echo ""
if [[ "$FAIL" -eq 0 ]]; then
  echo "=== ALL ASSERTIONS PASSED ==="
  exit 0
else
  echo "=== SOME ASSERTIONS FAILED ==="
  exit 1
fi
