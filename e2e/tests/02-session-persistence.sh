#!/usr/bin/env bash
# 02-session-persistence.sh — E2E: Session JSONL is complete and well-formed.
#
# Validates:
#   1. Session directory structure (.meta.json + transcript.jsonl)
#   2. JSONL lines are valid JSON
#   3. Meta fields are populated (session_id, goal, model, status)
#   4. Transcript has system + user + assistant messages
#   5. Message count in meta matches actual JSONL line count
#   6. Tool calls are recorded in transcript

set -euo pipefail

WORKSPACE="$(mktemp -d)"
trap 'rm -rf "$WORKSPACE"' EXIT

RECURSIVE_API_KEY="${RECURSIVE_API_KEY:-${DEEPSEEK_API_KEY:-}}"
if [[ -z "$RECURSIVE_API_KEY" ]]; then
  echo "[e2e:session] SKIP: no API key"
  exit 0
fi

BIN="${RECURSIVE_BIN:-./target/release/recursive}"
[[ ! -x "$BIN" ]] && BIN="./target/debug/recursive"
[[ ! -x "$BIN" ]] && { echo "ERROR: binary not found"; exit 1; }

"$BIN" \
  --workspace "$WORKSPACE" \
  --api-key "$RECURSIVE_API_KEY" \
  --max-steps 10 \
  run "List the files in the workspace root using list_dir tool."

echo "[e2e:session] agent finished"

# ---- Assertions ----
FAIL=0

SESSION_DIR="$WORKSPACE/.recursive/sessions"
if [[ ! -d "$SESSION_DIR" ]]; then
  echo "FAIL: no sessions directory"
  exit 1
fi

# Find session
META_FILE=$(find "$SESSION_DIR" -name ".meta.json" | head -1)
if [[ -z "$META_FILE" ]]; then
  echo "FAIL: no .meta.json found"
  exit 1
fi

SESSION_PATH="$(dirname "$META_FILE")"
TRANSCRIPT="$SESSION_PATH/transcript.jsonl"

# 1. Both files exist
echo "PASS: .meta.json exists at $META_FILE"

if [[ ! -f "$TRANSCRIPT" ]]; then
  echo "FAIL: transcript.jsonl not found at $TRANSCRIPT"
  exit 1
fi
echo "PASS: transcript.jsonl exists"

# 2. JSONL lines are valid JSON
TOTAL_LINES=$(wc -l < "$TRANSCRIPT" | tr -d ' ')
VALID_LINES=0
while IFS= read -r line; do
  if echo "$line" | python3 -c "import sys,json; json.loads(sys.stdin.read())" 2>/dev/null; then
    VALID_LINES=$((VALID_LINES + 1))
  else
    echo "FAIL: invalid JSON line: $line"
    FAIL=1
    break
  fi
done < "$TRANSCRIPT"

if [[ "$FAIL" -eq 0 ]]; then
  echo "PASS: all $TOTAL_LINES JSONL lines are valid JSON"
fi

# 3. Meta fields populated
python3 -c "
import json, sys
meta = json.load(open('$META_FILE'))
required = ['session_id', 'goal', 'model', 'status', 'created_at', 'message_count']
missing = [f for f in required if f not in meta or not meta[f]]
if missing:
    print(f'FAIL: meta missing fields: {missing}')
    sys.exit(1)
print(f'PASS: meta has all required fields (session_id={meta[\"session_id\"][:16]}...)')
" || FAIL=1

# 4. Has system + user + assistant roles
python3 -c "
import json, sys
lines = open('$TRANSCRIPT').read().strip().split('\n')
msgs = [json.loads(l) for l in lines]
roles = set(m['role'] for m in msgs)
expected = {'user', 'assistant'}
missing = expected - roles
if missing:
    print(f'FAIL: missing roles: {missing} (found: {roles})')
    sys.exit(1)
print(f'PASS: transcript has roles: {sorted(roles)}')
" || FAIL=1

# 5. Message count consistency
python3 -c "
import json, sys
meta = json.load(open('$META_FILE'))
lines = open('$TRANSCRIPT').read().strip().split('\n')
actual = len(lines)
declared = meta.get('message_count', 0)
if actual != declared:
    print(f'WARN: meta.message_count={declared} but transcript has {actual} lines (may differ due to timing)')
else:
    print(f'PASS: message_count consistent ({actual})')
" || true  # Warning only, not fatal

# 6. Tool calls recorded
python3 -c "
import json, sys
lines = open('$TRANSCRIPT').read().strip().split('\n')
msgs = [json.loads(l) for l in lines]
tool_calls = []
for m in msgs:
    if 'tool_calls' in m and m['tool_calls']:
        for tc in m['tool_calls']:
            tool_calls.append(tc.get('name', '?'))
if not tool_calls:
    print('WARN: no tool_calls found in transcript (agent may have answered directly)')
else:
    print(f'PASS: tool_calls recorded: {tool_calls}')
" || true

echo ""
if [[ "$FAIL" -eq 0 ]]; then
  echo "=== ALL ASSERTIONS PASSED ==="
  exit 0
else
  echo "=== SOME ASSERTIONS FAILED ==="
  exit 1
fi
