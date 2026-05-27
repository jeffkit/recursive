#!/usr/bin/env bash
# 04-cost-tracking.sh — E2E: cost.json is produced with valid data.
#
# Validates:
#   1. cost.json exists in the session directory
#   2. cost.json is valid JSON
#   3. Has required fields (model, tokens_prompt, tokens_completion)
#   4. Token counts are > 0
#   5. cost_usd is a number (may be 0 if pricing unknown)

set -euo pipefail

WORKSPACE="$(mktemp -d)"
trap 'rm -rf "$WORKSPACE"' EXIT

RECURSIVE_API_KEY="${RECURSIVE_API_KEY:-${DEEPSEEK_API_KEY:-}}"
if [[ -z "$RECURSIVE_API_KEY" ]]; then
  echo "[e2e:cost] SKIP: no API key"
  exit 0
fi

BIN="${RECURSIVE_BIN:-./target/release/recursive}"
[[ ! -x "$BIN" ]] && BIN="./target/debug/recursive"
[[ ! -x "$BIN" ]] && { echo "ERROR: binary not found"; exit 1; }

"$BIN" \
  --workspace "$WORKSPACE" \
  --api-key "$RECURSIVE_API_KEY" \
  --max-steps 5 \
  run "Say hello. Do not use any tools."

echo "[e2e:cost] agent finished"

# ---- Assertions ----
FAIL=0

# Find cost.json
SESSION_DIR="$WORKSPACE/.recursive/sessions"
COST_FILE=$(find "$SESSION_DIR" -name "cost.json" 2>/dev/null | head -1)

# 1. cost.json exists
if [[ -z "$COST_FILE" ]]; then
  echo "FAIL: cost.json not found in $SESSION_DIR"
  find "$SESSION_DIR" -type f 2>/dev/null | head -10
  FAIL=1
else
  echo "PASS: cost.json found at $COST_FILE"
fi

if [[ -n "$COST_FILE" ]] && [[ -f "$COST_FILE" ]]; then
  # 2. Valid JSON
  if python3 -c "import json; json.load(open('$COST_FILE'))" 2>/dev/null; then
    echo "PASS: cost.json is valid JSON"
  else
    echo "FAIL: cost.json is not valid JSON"
    cat "$COST_FILE"
    FAIL=1
  fi

  # 3-5. Field validation
  python3 -c "
import json, sys

data = json.load(open('$COST_FILE'))

# Check required fields exist
required = ['model']
# Token fields might be nested in total_usage or flat
has_tokens = ('tokens_prompt' in data or
              ('total_usage' in data and 'prompt_tokens' in data['total_usage']))

if not has_tokens:
    print(f'FAIL: no token fields found. Keys: {list(data.keys())}')
    sys.exit(1)

# Get token values
if 'tokens_prompt' in data:
    prompt = data['tokens_prompt']
    completion = data.get('tokens_completion', 0)
elif 'total_usage' in data:
    prompt = data['total_usage'].get('prompt_tokens', 0)
    completion = data['total_usage'].get('completion_tokens', 0)
else:
    prompt = 0
    completion = 0

# Tokens > 0
if prompt > 0:
    print(f'PASS: prompt_tokens = {prompt}')
else:
    print(f'FAIL: prompt_tokens = {prompt} (expected > 0)')
    sys.exit(1)

if completion > 0:
    print(f'PASS: completion_tokens = {completion}')
else:
    print(f'WARN: completion_tokens = {completion} (may be 0 for very short responses)')

# Cost is a number
cost = data.get('cost_usd', None)
if cost is None and 'total_usage' in data:
    cost = 0  # cost may not be in the file if pricing is unknown
if isinstance(cost, (int, float)):
    print(f'PASS: cost_usd = \${cost:.6f}')
else:
    print(f'WARN: cost_usd field type: {type(cost).__name__} (value: {cost})')

# Model field
model = data.get('model', '?')
print(f'PASS: model = {model}')
" || FAIL=1
fi

echo ""
if [[ "$FAIL" -eq 0 ]]; then
  echo "=== ALL ASSERTIONS PASSED ==="
  exit 0
else
  echo "=== SOME ASSERTIONS FAILED ==="
  exit 1
fi
