#!/usr/bin/env bash
# 01-basic-tools.sh — E2E: Agent uses write_file tool to create a file.
#
# This test runs with a REAL LLM (requires DEEPSEEK_API_KEY).
# The task is simple enough that any capable model can complete in 2-3 steps.
#
# Assertions:
#   1. Agent exits successfully (code 0)
#   2. hello.txt exists in workspace
#   3. hello.txt contains "world"
#   4. Session JSONL was created
#   5. Session meta shows status=completed

set -euo pipefail

WORKSPACE="$(mktemp -d)"
trap 'rm -rf "$WORKSPACE"' EXIT

echo "[e2e:basic-tools] workspace: $WORKSPACE"

# Run agent with a deterministic goal
RECURSIVE_API_BASE="${RECURSIVE_API_BASE:-https://api.deepseek.com/v1}"
RECURSIVE_API_KEY="${RECURSIVE_API_KEY:-${DEEPSEEK_API_KEY:-}}"
RECURSIVE_MODEL="${RECURSIVE_MODEL:-deepseek-chat}"
RECURSIVE_MAX_STEPS="${RECURSIVE_MAX_STEPS:-10}"

if [[ -z "$RECURSIVE_API_KEY" ]]; then
  echo "[e2e:basic-tools] SKIP: no API key available"
  exit 0
fi

BIN="${RECURSIVE_BIN:-./target/release/recursive}"
if [[ ! -x "$BIN" ]]; then
  BIN="./target/debug/recursive"
fi
if [[ ! -x "$BIN" ]]; then
  echo "[e2e:basic-tools] ERROR: recursive binary not found (build first)"
  exit 1
fi

"$BIN" \
  --workspace "$WORKSPACE" \
  --api-base "$RECURSIVE_API_BASE" \
  --api-key "$RECURSIVE_API_KEY" \
  -m "$RECURSIVE_MODEL" \
  --max-steps "$RECURSIVE_MAX_STEPS" \
  run "Create a file called hello.txt in the workspace root with exactly the content 'world' (no newline). Use write_file tool. Do NOT use any other tool after that."

AGENT_EXIT=$?

echo "[e2e:basic-tools] agent exit code: $AGENT_EXIT"

# ---- Assertions ----

FAIL=0

# 1. Exit success
if [[ "$AGENT_EXIT" -ne 0 ]]; then
  echo "FAIL: agent exited with code $AGENT_EXIT"
  FAIL=1
fi

# 2. File exists
if [[ ! -f "$WORKSPACE/hello.txt" ]]; then
  echo "FAIL: hello.txt does not exist"
  ls -la "$WORKSPACE/"
  FAIL=1
else
  echo "PASS: hello.txt exists"
fi

# 3. File content
if [[ -f "$WORKSPACE/hello.txt" ]]; then
  CONTENT="$(cat "$WORKSPACE/hello.txt")"
  if [[ "$CONTENT" == "world" ]] || [[ "$CONTENT" == *"world"* ]]; then
    echo "PASS: hello.txt contains 'world'"
  else
    echo "FAIL: hello.txt content is '$CONTENT', expected 'world'"
    FAIL=1
  fi
fi

# 4. Session created
SESSION_DIR="$WORKSPACE/.recursive/sessions"
if [[ -d "$SESSION_DIR" ]]; then
  SESSION_COUNT=$(find "$SESSION_DIR" -name ".meta.json" | wc -l | tr -d ' ')
  if [[ "$SESSION_COUNT" -gt 0 ]]; then
    echo "PASS: session created ($SESSION_COUNT session(s))"
  else
    echo "FAIL: session directory exists but no .meta.json found"
    FAIL=1
  fi
else
  echo "FAIL: no .recursive/sessions/ directory"
  FAIL=1
fi

# 5. Session status = completed
if [[ -d "$SESSION_DIR" ]]; then
  META_FILE=$(find "$SESSION_DIR" -name ".meta.json" | head -1)
  if [[ -n "$META_FILE" ]]; then
    STATUS=$(python3 -c "import json; print(json.load(open('$META_FILE'))['status'])" 2>/dev/null || echo "?")
    if [[ "$STATUS" == "completed" ]] || [[ "$STATUS" == "success" ]]; then
      echo "PASS: session status is 'completed'"
    else
      echo "FAIL: session status is '$STATUS', expected 'completed'"
      FAIL=1
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
