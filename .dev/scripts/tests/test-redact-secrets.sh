#!/usr/bin/env bash
# test-redact-secrets.sh — unit test for .dev/scripts/redact-secrets.sh.
#
# Pipes a fixture with all known secret patterns through the
# `redact_secrets` function and asserts:
#   - every real-secret pattern is replaced with <REDACTED>
#   - benign text (real prose, short tokens, plain URLs) is untouched
#
# Run:  bash .dev/scripts/tests/test-redact-secrets.sh
# Exits 0 on pass, 1 on first failure.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/../redact-secrets.sh"

PASS=0
FAIL=0

assert_contains() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$actual" == *"$expected"* ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    echo "FAIL [$desc]"
    echo "  expected to contain: $expected"
    echo "  actual:              $actual"
  fi
}

assert_not_contains() {
  local desc="$1" forbidden="$2" actual="$3"
  if [[ "$actual" != *"$forbidden"* ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    echo "FAIL [$desc]"
    echo "  expected NOT to contain: $forbidden"
    echo "  actual:                   $actual"
  fi
}

# 1. The real DeepSeek key that produced the original journal leak.
LEAKED_KEY="sk-2d126c6c3c7e48b68a857219f006c1be"
out="$(printf '%s\n' "api_key = \"$LEAKED_KEY\"" | redact_secrets)"
assert_contains "TOML api_key value redacted" "<REDACTED>" "$out"
assert_not_contains "TOML api_key value: raw key gone" "$LEAKED_KEY" "$out"

# 2. A bare LLM key (no surrounding syntax) on its own line.
out="$(printf 'log line: %s\n' "$LEAKED_KEY" | redact_secrets)"
assert_not_contains "bare sk- key: raw gone" "$LEAKED_KEY" "$out"
assert_contains "bare sk- key: <REDACTED> shown" "<REDACTED>" "$out"

# 3. Any `api_key = "..."` value is redacted, regardless of length.
#    This is intentionally aggressive: a test fixture like
#    `api_key = "sk-from-file"` is exactly the kind of false-negative
#    we'd rather over-redact than miss. A real DeepSeek-shaped key
#    (32+ hex chars) is the typical case, but a paste mistake of
#    a shorter key in a fixture must not survive the journal.
out="$(printf 'api_key = "sk-from-file"\n' | redact_secrets)"
assert_not_contains "short fixture sk-from-file: raw value gone" "sk-from-file" "$out"
assert_contains "short fixture sk-from-file: api_key= line redacted" "api_key=<REDACTED>" "$out"

# 4. api_key in YAML (colon form).
out="$(printf 'api_key: "%s"\n' "$LEAKED_KEY" | redact_secrets)"
assert_not_contains "YAML api_key colon: raw gone" "$LEAKED_KEY" "$out"
assert_contains "YAML api_key colon: <REDACTED> shown" "api_key: <REDACTED>" "$out"

# 5. api_key in single-quoted shell form.
out="$(printf "api_key='%s'\n" "$LEAKED_KEY" | redact_secrets)"
assert_not_contains "shell single-quoted api_key: raw gone" "$LEAKED_KEY" "$out"
assert_contains "shell single-quoted api_key: <REDACTED> shown" "api_key=<REDACTED>" "$out"

# 6. password = "..." in a config file.
out="$(printf 'password = "hunter2-supersecret"\n' | redact_secrets)"
assert_not_contains "password = raw gone" "hunter2-supersecret" "$out"
assert_contains "password = redacted" "password=<REDACTED>" "$out"

# 7. token = "..." variant.
out="$(printf 'token: "ghp_1234567890abcdefghij"\n' | redact_secrets)"
assert_not_contains "token: raw gone" "ghp_1234567890abcdefghij" "$out"
assert_contains "token: redacted" "token: <REDACTED>" "$out"

# 8. Authorization: Bearer header.
out="$(printf 'Authorization: Bearer %s\n' "$LEAKED_KEY" | redact_secrets)"
assert_not_contains "Bearer header: raw gone" "$LEAKED_KEY" "$out"
assert_contains "Bearer header: redacted" "Authorization: Bearer <REDACTED>" "$out"

# 9. URL with embedded creds.
out="$(printf 'POST https://user:pass@api.example.com/v1/chat\n' | redact_secrets)"
assert_not_contains "URL creds: user gone" "user:pass" "$out"
assert_contains "URL creds: <REDACTED>@host preserved" "<REDACTED>@api.example.com" "$out"

# 10. Plain URL without creds is untouched.
out="$(printf 'GET https://api.deepseek.com/v1/chat HTTP/1.1\n' | redact_secrets)"
assert_contains "plain URL untouched" "https://api.deepseek.com/v1/chat" "$out"

# 11. Benign prose is untouched.
out="$(printf 'Here is my plan:\n1. Read the README\n2. Run cargo test\n' | redact_secrets)"
assert_contains "benign prose: plan header" "Here is my plan:" "$out"
assert_contains "benign prose: bullet 2" "Run cargo test" "$out"

# 12. "secret" in prose (a non-assignment) is left alone.
out="$(printf 'The secret ingredient is love.\n' | redact_secrets)"
assert_contains "secret in prose untouched" "The secret ingredient is love." "$out"

# 13. The original journal that produced the leak: cat-style output.
out="$(printf '[provider]\napi_base = "https://api.deepseek.com"\napi_key = "%s"\nmodel = "deepseek-v4-flash"\ntype = "openai"\n' "$LEAKED_KEY" | redact_secrets)"
assert_not_contains "cat-style journal: raw key gone" "$LEAKED_KEY" "$out"
assert_contains "cat-style journal: api_base preserved" 'api_base = "https://api.deepseek.com"' "$out"
assert_contains "cat-style journal: model preserved" 'model = "deepseek-v4-flash"' "$out"
assert_contains "cat-style journal: type preserved" 'type = "openai"' "$out"

# 14. Multiple keys in one stream: all redacted.
out="$(printf '%s and also %s\n' "$LEAKED_KEY" "sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" | redact_secrets)"
count_redacted=$(printf '%s' "$out" | grep -o "<REDACTED>" | wc -l | tr -d ' ')
if [[ "$count_redacted" -ge 2 ]]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1))
  echo "FAIL [multiple keys in one stream: both redacted]"
  echo "  expected >= 2 <REDACTED> markers"
  echo "  actual: $out"
fi

# 15. Empty input passes through.
out="$(printf '' | redact_secrets)"
assert_contains "empty input is empty" "" "$out"

echo ""
echo "Passed: $PASS, Failed: $FAIL"
[[ "$FAIL" -eq 0 ]] && exit 0 || exit 1
