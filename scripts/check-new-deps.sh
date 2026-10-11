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
#   Exits 2 if the base ref cannot be resolved (the gate fails closed, #184).

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

# The base ref must resolve, and the gate must FAIL when it does not. A shallow
# CI clone (actions/checkout defaults to fetch-depth: 1) has no HEAD~1; the old
# code swallowed that git error with `2>/dev/null` and reported "unchanged", so
# the gate never actually ran (#184).
if ! git -C "$WORKSPACE" rev-parse --verify --quiet "${BASE_REF}^{commit}" >/dev/null; then
    echo "FAIL: base ref '$BASE_REF' does not resolve — cannot check invariant #6." >&2
    echo "      CI must check out with fetch-depth >= 2 (see issue #184)." >&2
    exit 2
fi

# All git calls use -C "$WORKSPACE" so the verdict does not depend on the
# caller's cwd (the invariant test runs the script from a fixture repo). The
# newline padding lets the checks below match a whole path (never a prefix or
# suffix of one) without piping into `grep -q`.
CHANGED_FILES=$'\n'"$(git -C "$WORKSPACE" diff --name-only "$BASE_REF" HEAD)"$'\n'

# Check if Cargo.toml changed vs base-ref
if [[ "$CHANGED_FILES" != *$'\nCargo.toml'$'\n'* ]]; then
    echo "OK: Cargo.toml unchanged vs $BASE_REF"
    exit 0
fi

# List "<name>\t<spec>" for every key of a dependency table in the Cargo.toml
# read from stdin. Section-aware on purpose: `[package] version = "0.8.3"` must
# never be mistaken for a dependency change. The old implementation regex-ed the
# raw diff, and its `^[+-]\+` alternation matched every `+`/`-` line, so
# DEP_CHANGES was always empty (#184).
#
# Inline tables may span lines:
#
#     rand = { version = "0.8", default-features = false,
#              features = ["small_rng"] }
#
# so a key only counts as a dependency when it starts at brace depth 0, and the
# whole (possibly multi-line) spec is what gets compared — otherwise the
# continuation line `features = [...]` reads as a dependency named "features"
# (#184 review) and an edit inside the table goes unnoticed.
#
# Comments are stripped only outside double-quoted strings: a `#` inside a spec
# (`features = ["a#b"] }`) must not swallow the closing brace, otherwise the
# depth never returns to 0 and the dependency is dropped from the comparison
# entirely — a fail-open hole in a gate that is supposed to fail closed.
list_dependency_entries() {
    awk '
        function emit(   s) {
            if (pending == "") return
            s = spec
            gsub(/[[:space:]]+/, " ", s)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", s)
            printf "%s\t%s\n", pending, s
            pending = ""
            spec = ""
        }
        # Drop a trailing `# …` comment, keeping `#` that sits inside a string.
        function strip_comment(s,   i, c, in_str, out) {
            in_str = 0
            out = ""
            for (i = 1; i <= length(s); i++) {
                c = substr(s, i, 1)
                if (c == "\\" && in_str) {
                    out = out c substr(s, i + 1, 1)
                    i++
                    continue
                }
                if (c == "\"") {
                    in_str = !in_str
                    out = out c
                    continue
                }
                if (c == "#" && !in_str) break
                out = out c
            }
            return out
        }
        /^[[:space:]]*\[/ {
            emit()
            depth = 0
            in_table = 0
            header = strip_comment($0)
            gsub(/[][[:space:]]/, "", header)
            if (header ~ /(^|\.)(dependencies|dev-dependencies|build-dependencies)$/) {
                in_deps = 1
            } else if (header ~ /(^|\.)(dependencies|dev-dependencies|build-dependencies)\./) {
                # Table form: `[dependencies.serde]` followed by version/features
                # keys. The dependency is the part after the table keyword, and
                # its *whole body* is its spec — otherwise an edit inside the
                # table (a version bump, an added feature) would leave the
                # name/spec pair unchanged and the gate would silently pass it.
                in_deps = 0
                in_table = 1
                name = header
                sub(/^.*(dependencies|dev-dependencies|build-dependencies)\./, "", name)
                pending = name
                spec = "<table>"
            } else {
                in_deps = 0
            }
            next
        }
        !in_deps && !in_table { next }
        in_table {
            spec = spec " " strip_comment($0)
            next
        }
        {
            line = strip_comment($0)
            opens = gsub(/\{/, "{", line)
            closes = gsub(/\}/, "}", line)
            if (depth == 0 && line ~ /^[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=/) {
                emit()
                eq = index(line, "=")
                pending = substr(line, 1, eq - 1)
                gsub(/[[:space:]]/, "", pending)
                spec = substr(line, eq + 1)
            } else if (pending != "") {
                spec = spec " " line
            }
            depth += opens - closes
            if (depth <= 0) {
                depth = 0
                emit()
            }
        }
        # A dependency table at EOF (or an unterminated inline table) must still
        # be reported: table-form entries are buffered until the next header.
        END { emit() }
    '
}

# Extract dependency changes by comparing the two revisions' dependency tables,
# not the diff text: a dependency needs justification when it is new, or when
# its spec changed (the name/spec pair exists only on the HEAD side). Removing a
# dependency never needs one, and a `[package]` metadata edit is invisible here.
echo "=== Cargo.toml dependency changes vs $BASE_REF ==="
BASE_DEPS=$(git -C "$WORKSPACE" show "$BASE_REF:Cargo.toml" 2>/dev/null \
    | list_dependency_entries | sort || true)
HEAD_DEPS=$(list_dependency_entries < "$CARGO_TOML" | sort)

CHANGED_DEPS=$(comm -13 <(printf '%s\n' "$BASE_DEPS") <(printf '%s\n' "$HEAD_DEPS") \
    | cut -f1 | grep -v '^$' | sort -u || true)

if [ -z "$CHANGED_DEPS" ]; then
    echo "OK: No dependency changes detected in Cargo.toml"
    exit 0
fi

echo "Changed dependencies:"
echo "$CHANGED_DEPS" | sed 's/^/  /'
echo ""

# Check journal entries since base-ref for justification
VIOLATIONS=""
for dep in $CHANGED_DEPS; do
    FOUND=false
    for journal_file in "$JOURNAL_DIR"/*.md; do
        [ -f "$journal_file" ] || continue
        # Check if this journal was created/modified after the base ref
        journal_rel=".dev/journal/$(basename "$journal_file")"
        if [[ "$CHANGED_FILES" == *$'\n'"$journal_rel"$'\n'* ]]; then
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
