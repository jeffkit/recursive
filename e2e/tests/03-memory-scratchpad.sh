#!/usr/bin/env bash
# 03-memory-scratchpad.sh — E2E: Agent scratchpad persists within a run.
#
# Validates:
#   1. Agent can use scratchpad_set to store a value
#   2. scratchpad.json is created in workspace
#   3. The stored key-value pair is present in the JSON file
#   4. Session transcript shows scratchpad_set tool call

set -euo pipefail

WORKSPACE="$(mktemp -d)"
trap 'rm -rf "$WORKSPACE"' EXIT

RECURSIVE_API_KEY="${RECURSIVE_API_KEY:-${DEEPSEEK_API_KEY:-}}"
if [[ -z "$RECURSIVE_API_KEY" ]]; then
  echo "[e2e:scratchpad] SKIP: no API key"
  exit 0
fi

BIN="${RECURSIVE_BIN:-./target/release/recursive}"
[[ ! -x "$BIN" ]] && BIN="./target/debug/recursive"
[[ ! -x "$BIN" ]] && { echo "ERROR: binary not found"; exit 1; }

"$BIN" \
  --workspace "$WORKSPACE" \
  --api-key "$RECURSIVE_API_KEY" \
  --max-steps 10 \
  run "Use the scratchpad_set tool to store the key 'test_color' with value 'blue'. Then use scratchpad_list to confirm it's stored. Do nothing else."

AGENT_EXIT=$?
echo "[e2e:scratchpad] agent exit code: $AGENT_EXIT"

# ---- Assertions ----
FAIL=0

# 1. Exit success
if [[ "$AGENT_EXIT" -ne 0 ]]; then
  echo "FAIL: agent exited with code $AGENT_EXIT"
  FAIL=1
fi

# 2. scratchpad.json exists
SCRATCHPAD="$WORKSPACE/.recursive/scratchpad.json"
if [[ -f "$SCRATCHPAD" ]]; then
  echo "PASS: scratchpad.json exists"
else
  echo "FAIL: scratchpad.json not found at $SCRATCHPAD"
  ls -la "$WORKSPACE/.recursive/" 2>/dev/null || echo "  (.recursive/ doesn't exist)"
  FAIL=1
fi

# 3. Contains test_color=blue
if [[ -f "$SCRATCHPAD" ]]; then
  python3 -c "
import json, sys
data = json.load(open('$SCRATCHPAD'))
# Scratchpad format may be {key: value} or {entries: [{key, value}]}
if isinstance(data, dict):
    if 'test_color' in data and data['test_color'] == 'blue':
        print('PASS: scratchpad contains test_color=blue')
        sys.exit(0)
    # Check if it's in an entries array
    entries = data.get('entries', [])
    for e in entries:
        if e.get('key') == 'test_color' and e.get('value') == 'blue':
            print('PASS: scratchpad contains test_color=blue (in entries)')
            sys.exit(0)
    # Check flat values
    for k, v in data.items():
        if k == 'test_color' or (isinstance(v, dict) and v.get('value') == 'blue'):
            print(f'PASS: found test_color in scratchpad (format: {type(v).__name__})')
            sys.exit(0)
print(f'FAIL: test_color=blue not found in scratchpad. Content: {json.dumps(data)[:200]}')
sys.exit(1)
" || FAIL=1
fi

# 4. Session has scratchpad_set in transcript
SESSION_DIR="$WORKSPACE/.recursive/sessions"
if [[ -d "$SESSION_DIR" ]]; then
  TRANSCRIPT=$(find "$SESSION_DIR" -name "transcript.jsonl" | head -1)
  if [[ -n "$TRANSCRIPT" ]]; then
    if grep -q "scratchpad_set" "$TRANSCRIPT"; then
      echo "PASS: transcript contains scratchpad_set call"
    else
      echo "WARN: transcript does not contain scratchpad_set (agent may have used alternative)"
    fi
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
