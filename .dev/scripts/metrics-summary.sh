#!/usr/bin/env bash
# metrics-summary.sh — aggregate .dev/metrics/*.yaml into a dashboard summary.
#
# Usage:
#   .dev/scripts/metrics-summary.sh            # all metrics
#   .dev/scripts/metrics-summary.sh --batch 36 # filter by batch
#   .dev/scripts/metrics-summary.sh --last 10  # last N runs
#
# Requires: yq (or python3 with pyyaml)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
METRICS_DIR="$REPO_ROOT/.dev/metrics"

if [[ ! -d "$METRICS_DIR" ]]; then
  echo "No metrics directory found at $METRICS_DIR" >&2
  exit 1
fi

BATCH_FILTER=""
LAST_N=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --batch) BATCH_FILTER="$2"; shift 2 ;;
    --last)  LAST_N="$2"; shift 2 ;;
    *)       echo "usage: $0 [--batch N] [--last N]" >&2; exit 2 ;;
  esac
done

# Use Python for YAML parsing (more portable than yq)
python3 -c "
import os, sys, glob

try:
    import yaml
except ImportError:
    # Fallback: minimal YAML parser for our simple flat format
    class yaml:
        @staticmethod
        def safe_load(text):
            result = {}
            for line in text.split('\n'):
                line = line.strip()
                if not line or line.startswith('#'):
                    continue
                if ':' in line:
                    key, _, val = line.partition(':')
                    key = key.strip()
                    val = val.strip().strip('\"')
                    # Type inference
                    if val in ('true', 'True'): val = True
                    elif val in ('false', 'False'): val = False
                    elif val == 'null': val = None
                    else:
                        try: val = int(val)
                        except ValueError:
                            try: val = float(val)
                            except ValueError: pass
                    result[key] = val
            return result

metrics_dir = '$METRICS_DIR'
batch_filter = '$BATCH_FILTER' or None
last_n = int('$LAST_N') if '$LAST_N' else None

files = sorted(glob.glob(os.path.join(metrics_dir, 'run-*.yaml')))
if not files:
    print('No metrics files found.')
    sys.exit(0)

# Parse all metrics
runs = []
for f in files:
    with open(f) as fh:
        try:
            data = yaml.safe_load(fh.read())
            if data:
                runs.append(data)
        except Exception:
            pass

# Filters
if batch_filter:
    runs = [r for r in runs if str(r.get('batch', '')) == batch_filter]
if last_n:
    runs = runs[-last_n:]

if not runs:
    print('No matching runs found.')
    sys.exit(0)

# Aggregate
total = len(runs)
committed = sum(1 for r in runs if r.get('outcome') == 'committed')
rolled_back = sum(1 for r in runs if r.get('outcome') == 'rolled-back')
panics = sum(1 for r in runs if r.get('outcome') == 'panic')
no_changes = sum(1 for r in runs if r.get('outcome') == 'skip-commit')

total_cost = sum(float(r.get('cost_usd', 0) or 0) for r in runs)
total_steps = sum(int(r.get('steps_used', 0) or 0) for r in runs)
total_tools = sum(int(r.get('total_tool_calls', 0) or 0) for r in runs)
total_wall = sum(int(r.get('wall_time_seconds', 0) or 0) for r in runs)

# Per-provider breakdown
providers = {}
for r in runs:
    p = r.get('provider', 'unknown')
    if p not in providers:
        providers[p] = {'total': 0, 'committed': 0, 'cost': 0.0}
    providers[p]['total'] += 1
    if r.get('outcome') == 'committed':
        providers[p]['committed'] += 1
    providers[p]['cost'] += float(r.get('cost_usd', 0) or 0)

# Review stats
review_enabled = sum(1 for r in runs if r.get('self_review_enabled') in (1, '1', True))
review_approve = sum(1 for r in runs if r.get('review_verdict') == 'approve')
review_reject = sum(1 for r in runs if r.get('review_verdict') == 'request_changes')

# Output
print(f'''# Metrics Summary
| metric | value |
|--------|-------|
| total runs | {total} |
| committed | {committed} ({committed*100//total if total else 0}%) |
| rolled back | {rolled_back} |
| panics | {panics} |
| no changes | {no_changes} |
| total cost | \${total_cost:.4f} |
| avg cost/run | \${total_cost/total:.4f} |
| total steps | {total_steps} |
| avg steps/run | {total_steps//total if total else 0} |
| total wall time | {total_wall//60}m {total_wall%60}s |
| avg wall time | {total_wall//total if total else 0}s |

## Provider Breakdown
| provider | runs | success rate | cost |
|----------|------|-------------|------|''')

for p, d in sorted(providers.items()):
    rate = d['committed'] * 100 // d['total'] if d['total'] else 0
    print(f\"| {p} | {d['total']} | {rate}% | \${d['cost']:.4f} |\")

if review_enabled:
    print(f'''
## Self-Review Pipeline
| metric | value |
|--------|-------|
| runs with review | {review_enabled} |
| approved | {review_approve} |
| request_changes | {review_reject} |''')

# Last 5 runs detail
print()
print('## Recent Runs')
print('| run_id | goal | provider | outcome | cost | steps |')
print('|--------|------|----------|---------|------|-------|')
for r in runs[-5:]:
    print(f\"| {r.get('run_id','?')[:20]} | {r.get('goal_tag','?')[:20]} | {r.get('provider','?')} | {r.get('outcome','?')} | \${float(r.get('cost_usd',0) or 0):.4f} | {r.get('steps_used','?')} |\")
" 2>&1
