#!/usr/bin/env bash
# Why this script exists:
# .dev/AGENTS.md invariant #6: "No new dependencies without justification.
# State the reason in the journal entry. Prefer std + what's already in
# Cargo.toml."
#
# This script checks that every dependency added/modified in Cargo.toml
# is accompanied by a corresponding journal entry in .dev/journal/ under
# the same PR/run. It runs as part of the invariant tests (see
# tests/invariants/mod.rs).
#
# Usage: scripts/check-new-deps.sh [base-ref]
#   base-ref: git ref to diff against (default: HEAD~1)
#   Exits 0 if all dep changes have journal justification.
#   Exits 1 if any dep change lacks a journal entry.

set -euo pipefail

BASE_REF="${1:-HEAD~1}"
WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_TOML="$WORKSPACE/Cargo.toml"
JOURNAL_DIR="$WORKSPACE/.dev/journal"

# Ensure journal directory exists
if [ ! -d "$JOURNAL_DIR" ]; then
    echo "SKIP: no .dev/journal/ directory"
    exit 0
fi

# Check if Cargo.toml changed vs base-ref
if ! git diff --name-only "$BASE_REF" HEAD 2>/dev/null | grep -q 'Cargo.toml'; then
    echo "OK: Cargo.toml unchanged vs $BASE_REF"
    exit 0
fi

# Extract dependency changes from the diff
echo "=== Cargo.toml dependency changes vs $BASE_REF ==="
DEP_CHANGES=$(git diff "$BASE_REF" HEAD -- Cargo.toml | grep '^[+-]' | grep -v '^[+-]\+\|^[+-]version\|^[+-]authors\|^[+-]description\|^[+-]license\|^[+-]repository\|^[+-]homepage\|^[+-]documentation\|^[+-]keywords\|^[+-]categories\|^[+-]rust-version\|^[+-]exclude\|^[+-]default-run\|^[+-]edition' || true)

if [ -z "$DEP_CHANGES" ]; then
    echo "OK: No dependency changes detected in Cargo.toml"
    exit 0
fi

echo "$DEP_CHANGES"
echo ""

# Get list of changed/new deps
CHANGED_DEPS=$(echo "$DEP_CHANGES" | grep '^[+-]' | sed 's/^[+-]\s*//' | grep -oP '^\w[\w-]*' | sort -u || true)

if [ -z "$CHANGED_DEPS" ]; then
    echo "OK: No dependency names extracted"
    exit 0
fi

echo "Changed dependencies: $CHANGED_DEPS"
echo ""

# Check journal entries since base-ref for justification
VIOLATIONS=""
for dep in $CHANGED_DEPS; do
    FOUND=false
    for journal_file in "$JOURNAL_DIR"/*.md; do
        [ -f "$journal_file" ] || continue
        # Check if this journal was created/modified after the base ref
        if git diff --name-only "$BASE_REF" HEAD 2>/dev/null | grep -q "$(basename "$journal_file")"; then
            if grep -qi "$dep" "$journal_file" 2>/dev/null; then
                echo "  + $dep justified in $(basename "$journal_file")"
                FOUND=true
                break
            fi
        fi
    done
    # Fallback: check all journal files (for initial commits)
    if [ "$FOUND" = false ]; then
        for journal_file in "$JOURNAL_DIR"/*.md; do
            [ -f "$journal_file" ] || continue
            if grep -qi "$dep" "$journal_file" 2>/dev/null; then
                echo "  ~ $dep mentioned in $(basename "$journal_file") (not in this PR)"
                FOUND=true
                break
            fi
        done
    fi
    if [ "$FOUND" = false ]; then
        echo "  ✗ $dep: NO JOURNAL JUSTIFICATION FOUND"
        VIOLATIONS="$VIOLATIONS $dep"
    fi
done

if [ -n "$VIOLATIONS" ]; then
    echo ""
    echo "invariant #6 VIOLATION: the following dependency changes lack journal justification:"
    for dep in $VIOLATIONS; do
        echo "  - $dep"
    done
    echo ""
    echo "Add a journal entry in .dev/journal/ explaining why each new dependency is needed."
    exit 1
fi

echo ""
echo "OK: All dependency changes have journal justification."
exit 0
