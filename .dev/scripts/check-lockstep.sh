#!/usr/bin/env bash
# check-lockstep.sh — enforce version lockstep across all publishable crates.
#
# Usage:
#   .dev/scripts/check-lockstep.sh          # all publishable crates share one version,
#                                           # internal path-dep reqs == that version
#   .dev/scripts/check-lockstep.sh v0.8.3   # additionally require version == tag
#
# Intended for release.yml wiring (out of scope of issue #44's repo changes):
#   .dev/scripts/check-lockstep.sh "${GITHUB_REF_NAME}"
set -euo pipefail

TAG="${1:-}"

META="$(cargo metadata --no-deps --format-version 1)" \
TAG="$TAG" python3 - <<'PYEOF'
import json, os

tag = os.environ["TAG"]
meta = json.loads(os.environ["META"])
expected_tag = tag[1:] if tag.startswith("v") else None

pkgs = [p for p in meta["packages"] if p.get("publish") is None]
if not pkgs:
    print("ERROR: no publishable crates found")
    raise SystemExit(1)

baseline = pkgs[0]["version"]
names = {p["name"] for p in pkgs}
errors = []

for p in pkgs:
    if expected_tag and p["version"] != expected_tag:
        errors.append(f"ERROR: {p['name']}={p['version']} (expected {expected_tag})")
    elif p["version"] != baseline:
        errors.append(f"ERROR: {p['name']}={p['version']} (expected {baseline})")
    for d in p.get("dependencies", []):
        if "path" not in d or d["name"] not in names:
            continue
        req = d["req"]
        is_dev = d.get("kind") == "dev"
        if req != f"={baseline}" and not (is_dev and req == "*"):
            errors.append(
                f"ERROR: {p['name']} -> {d['name']} req={req} (expected ={baseline})"
            )

if errors:
    print("\n".join(errors))
    raise SystemExit(1)
print(f"OK: {len(pkgs)} publishable crates lockstep at {baseline}")
PYEOF
