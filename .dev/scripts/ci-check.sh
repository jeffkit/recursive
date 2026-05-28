#!/usr/bin/env bash
# ci-check.sh — run the same checks as .github/workflows/ci.yml locally.
#
# Usage:
#   .dev/scripts/ci-check.sh
#
# Optional git pre-push hook (one-time setup):
#   git config core.hooksPath .githooks
#   chmod +x .dev/scripts/ci-check.sh .githooks/pre-push

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

export CARGO_TERM_COLOR=always
export RUSTFLAGS="-D warnings"

echo "==> cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> cargo clippy --workspace --all-targets --all-features -- -D warnings"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "==> cargo build --workspace"
cargo build --workspace

echo "==> cargo test --workspace"
cargo test --workspace

echo "All CI checks passed."
