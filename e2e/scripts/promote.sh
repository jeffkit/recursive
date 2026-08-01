#!/bin/bash
# promote.sh <suite-id> — Promote recorded fixtures to regression tests.
#
# After running E2E tests in record mode (E2E_RECORD=1), this script:
# 1. Merges recorded fixture files into a single fixture JSON
# 2. Moves it to fixtures/<suite-id>.json (overwriting if exists)
# 3. Cleans up the recorded/ directory
#
# Usage:
#   ./promote.sh smoke
#   ./promote.sh memory-facts
#
# Prerequisites:
#   - E2E_RECORD=1 run completed successfully
#   - fixtures/recorded/ contains JSON files from the recording
#   - python3 available

set -euo pipefail

SUITE_ID="${1:?Usage: $0 <suite-id>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES_DIR="$(cd "$SCRIPT_DIR/../fixtures" && pwd)"
RECORDED_DIR="$FIXTURES_DIR/recorded"

if [ ! -d "$RECORDED_DIR" ]; then
  echo "error: No recorded/ directory found at $RECORDED_DIR"
  echo "       Run tests with E2E_RECORD=1 first."
  exit 1
fi

# aimock --record 把新 fixture 写到第一个 -f 路径下的 recorded/ 子目录
# （/fixtures/recorded/recorded/*.json），所以用 find 递归而非 ls 顶层 glob。
RECORDED_FILES=($(find "$RECORDED_DIR" -name '*.json' 2>/dev/null || true))

if [ ${#RECORDED_FILES[@]} -eq 0 ]; then
  echo "error: No recorded fixture files found in $RECORDED_DIR/"
  echo "       Run tests with E2E_RECORD=1 first."
  exit 1
fi

echo "Found ${#RECORDED_FILES[@]} recorded fixture file(s)"

# Merge all recorded files into a single fixture
TARGET="$FIXTURES_DIR/${SUITE_ID}.json"

python3 -c "
import json, glob, sys, os

recorded_dir = '$RECORDED_DIR'
all_fixtures = []

for f in sorted(glob.glob(os.path.join(recorded_dir, '**', '*.json'), recursive=True)):
    try:
        data = json.load(open(f))
        if isinstance(data, list):
            items = data
        elif isinstance(data, dict):
            items = data.get('fixtures', [data])
        else:
            items = []
        for fx in items:
            # Strip match.model: recording uses the real model name (e.g.
            # deepseek-chat) but replay uses mock-chat. Leaving model in the
            # fixture would make aimock reject every replay request as a
            # model mismatch. aimock matches on userMessage/turnIndex/
            # hasToolResult — model is not a meaningful discriminator here.
            if isinstance(fx, dict) and 'match' in fx and 'model' in fx['match']:
                del fx['match']['model']
            all_fixtures.append(fx)
    except json.JSONDecodeError as e:
        print(f'warning: skipping malformed file {f}: {e}', file=sys.stderr)

output = {'fixtures': all_fixtures}
with open('$TARGET', 'w') as out:
    json.dump(output, out, indent=2)

print(f'Promoted {len(all_fixtures)} fixture(s) to ${SUITE_ID}.json')
"

# Clean up recorded files (recursive — aimock nests them under recorded/recorded/)
find "$RECORDED_DIR" -name '*.json' -delete 2>/dev/null || true
# Also remove now-empty nested dirs aimock created
find "$RECORDED_DIR" -type d -empty -delete 2>/dev/null || true
echo "Recorded fixtures cleaned up."
echo ""
echo "Next steps:"
echo "  1. Review: cat $TARGET | python3 -m json.tool | head -50"
echo "  2. Replay (MCP path, no key): .dev/scripts/e2e-run.sh ${SUITE_ID}"
echo "     (from repo root; replays deterministically against the promoted fixture)"
echo "  3. Commit: git add $TARGET"
