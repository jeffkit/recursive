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

RECORDED_FILES=($(ls "$RECORDED_DIR"/*.json 2>/dev/null || true))

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

for f in sorted(glob.glob(os.path.join(recorded_dir, '*.json'))):
    try:
        data = json.load(open(f))
        if isinstance(data, list):
            all_fixtures.extend(data)
        elif isinstance(data, dict):
            if 'fixtures' in data:
                all_fixtures.extend(data['fixtures'])
            else:
                all_fixtures.append(data)
    except json.JSONDecodeError as e:
        print(f'warning: skipping malformed file {f}: {e}', file=sys.stderr)

output = {'fixtures': all_fixtures}
with open('$TARGET', 'w') as out:
    json.dump(output, out, indent=2)

print(f'Promoted {len(all_fixtures)} fixture(s) to ${SUITE_ID}.json')
"

# Clean up recorded files
rm -f "$RECORDED_DIR"/*.json
echo "Recorded fixtures cleaned up."
echo ""
echo "Next steps:"
echo "  1. Review: cat $TARGET | python3 -m json.tool | head -50"
echo "  2. Test replay: argusai -c e2e/e2e.yaml run -s ${SUITE_ID}"
echo "  3. Commit: git add $TARGET"
