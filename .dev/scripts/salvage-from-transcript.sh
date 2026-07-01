#!/usr/bin/env bash
# salvage-from-transcript.sh — recover code from a rolled-back run's transcript.
#
# When a Flowcast self-improve run rolls back, the worktree is discarded and
# the agent's code changes are gone from the filesystem — only the session
# transcript (transcript.jsonl) survives. Recovering used to mean hand-parsing
# JSONL and replaying apply_patch/write_file calls. This script automates it.
#
# It walks a transcript.jsonl, extracts every apply_patch / write_file tool
# call, and writes a recovery bundle to <outdir>/:
#   - files/<path>           reconstructed file contents from write_file calls
#   - patches/<n>.patch      each apply_patch payload (V4A format)
#   - all.patch              all apply_patch payloads concatenated
#   - manifest.txt           list of touched paths + per-call log
#
# The bundle is for review / re-application by an agent or `recursive
# apply_patch`. The script does NOT apply anything to your working tree.
#
# Usage:
#   salvage-from-transcript.sh <transcript.jsonl> [outdir]
#   salvage-from-transcript.sh <run-dir>         [outdir]   # picks transcript.jsonl (+ -fix-*.json / -resumed.json)
#
# Exit 0 if any recoverable tool call was found, non-zero otherwise.
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <transcript.jsonl | run-dir> [outdir]" >&2
  exit 2
fi

SRC="$1"
OUTDIR="${2:-./salvage-$(date +%Y%m%dT%H%M%S)}"

# Resolve source: a directory → take its transcript.jsonl + fix/resumed files.
TRANSCRIPTS=()
if [[ -d "$SRC" ]]; then
  for t in "$SRC"/transcript.json "$SRC"/transcript*.json; do
    [[ -f "$t" ]] && TRANSCRIPTS+=("$t")
  done
  if [[ ${#TRANSCRIPTS[@]} -eq 0 ]]; then
    # recursive session transcripts are jsonl
    for t in "$SRC"/transcript.jsonl "$SRC"/*.jsonl; do
      [[ -f "$t" ]] && TRANSCRIPTS+=("$t")
    done
  fi
  if [[ ${#TRANSCRIPTS[@]} -eq 0 ]]; then
    echo "error: no transcript.json{l} in dir $SRC" >&2
    exit 2
  fi
else
  [[ -f "$SRC" ]] || { echo "error: not a file or dir: $SRC" >&2; exit 2; }
  TRANSCRIPTS=("$SRC")
fi

mkdir -p "$OUTDIR/files" "$OUTDIR/patches"
MANIFEST="$OUTDIR/manifest.txt"
: > "$MANIFEST"

python3 - "${TRANSCRIPTS[@]}" "$OUTDIR" "$MANIFEST" <<'PY'
import json, os, re, sys

transcripts = sys.argv[1:-2]
outdir = sys.argv[-2]
manifest = sys.argv[-1]

def iter_records(path):
    with open(path, errors='replace') as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                # Some flowcast transcripts wrap multiple JSON objects per line
                # or use a top-level array; try best-effort object scan.
                for m in re.finditer(r'\{.*?\}(?=\{|\Z)', line, flags=re.S):
                    try:
                        yield json.loads(m.group(0))
                    except json.JSONDecodeError:
                        continue

def tool_calls_of(rec):
    """Yield (name, args_obj_or_str, call_id) from an assistant record,
    tolerant of OpenAI-style and flat variants."""
    # OpenAI style: tool_calls: [{id, type, function: {name, arguments(jsonstr)}}]
    for tc in rec.get('tool_calls') or []:
        if not isinstance(tc, dict):
            continue
        fn = tc.get('function') or {}
        name = fn.get('name') or tc.get('name')
        args = fn.get('arguments') or tc.get('arguments')
        cid = tc.get('id')
        if name:
            yield name, args, cid
    # Flat style: {role:assistant, tool_call: {name, arguments}}
    tc = rec.get('tool_call')
    if isinstance(tc, dict) and tc.get('name'):
        yield tc.get('name'), tc.get('arguments'), tc.get('id')
    # Anthropic style: content list with type=tool_use
    content = rec.get('content')
    if isinstance(content, list):
        for block in content:
            if isinstance(block, dict) and block.get('type') == 'tool_use':
                yield block.get('name'), block.get('input'), block.get('id')

def parse_args(args):
    if args is None:
        return {}
    if isinstance(args, dict):
        return args
    if isinstance(args, str):
        s = args.strip()
        if not s:
            return {}
        try:
            return json.loads(s)
        except json.JSONDecodeError:
            return {'_raw': s}
    return {'_raw': args}

def safe_path(p):
    p = str(p).strip()
    p = p.lstrip('/')
    # block escapes
    if '..' in p.split('/'):
        return None
    return p

write_count = patch_count = 0
patch_idx = 0
all_patches = open(os.path.join(outdir, 'all.patch'), 'w', errors='replace')

for tp in transcripts:
    for rec in iter_records(tp):
        if rec.get('role') != 'assistant':
            continue
        for name, args, cid in tool_calls_of(rec):
            if name == 'write_file':
                a = parse_args(args)
                p = a.get('path') or a.get('file') or a.get('filename')
                contents = a.get('contents') or a.get('content')
                if p is None or contents is None:
                    continue
                rp = safe_path(p)
                if rp is None:
                    continue
                dest = os.path.join(outdir, 'files', rp)
                os.makedirs(os.path.dirname(dest), exist_ok=True)
                with open(dest, 'w', errors='replace') as fh:
                    fh.write(contents if isinstance(contents, str) else json.dumps(contents, ensure_ascii=False, indent=2))
                write_count += 1
                with open(manifest, 'a') as m:
                    m.write(f"write_file\t{rp}\t(call {cid or '-'} from {os.path.basename(tp)})\n")
            elif name in ('apply_patch', 'ApplyPatch', 'edit', 'str_replace'):
                a = parse_args(args)
                # apply_patch puts the patch in 'patch'; some variants use 'input'/'content'
                patch = a.get('patch') or a.get('input') or a.get('content') or a.get('_raw')
                if patch is None:
                    continue
                patch_idx += 1
                pf = os.path.join(outdir, 'patches', f'{patch_idx:04d}.patch')
                with open(pf, 'w', errors='replace') as fh:
                    fh.write(patch if isinstance(patch, str) else json.dumps(patch, ensure_ascii=False, indent=2))
                all_patches.write(patch if isinstance(patch, str) else json.dumps(patch, ensure_ascii=False, indent=2))
                all_patches.write('\n\n')
                patch_count += 1
                # crude path extraction for the manifest
                paths = sorted(set(re.findall(r'(?:^|\n)(?:\*\*\* (?:Add|Update|Delete) File: |--- a/|\+\+\+ b/)(\S+)', patch if isinstance(patch, str) else '')))
                with open(manifest, 'a') as mm:
                    mm.write(f"apply_patch\tcall={cid or '-'}\tsrc={os.path.basename(tp)}\tpaths={','.join(paths) or '?'}\n")

all_patches.close()

with open(manifest, 'a') as m:
    m.write(f"\n# summary: {write_count} write_file, {patch_count} apply_patch, from {len(transcripts)} transcript(s)\n")

print(f"salvage bundle: {outdir}")
print(f"  write_file calls: {write_count}  -> {outdir}/files/")
print(f"  apply_patch calls: {patch_count} -> {outdir}/patches/ + {outdir}/all.patch")
print(f"  manifest: {manifest}")
sys.exit(0 if (write_count or patch_count) else 1)
PY

rc=$?
echo
echo "=== manifest ==="
cat "$MANIFEST"
exit $rc
