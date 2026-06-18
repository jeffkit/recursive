#!/usr/bin/env python3
"""
okf-goals.py — Add OKF-compliant YAML frontmatter to every goal file under
.dev/goals/ (and .dev/goals/deferred/).

Extracts structured metadata from the existing Markdown content:
  - type: Goal
  - title: from the first H1 heading
  - goal_number: integer NN (if heading matches "Goal NN — ...")
  - status: "completed" | "deferred" | "open" (inferred)
  - roadmap: roadmap reference string (if present)
  - tags: list derived from headings and keywords

Safe to re-run: skips files that already have `type:` in their frontmatter.
Skips DEPENDENCY.md and index files.
"""

import os
import re
import sys

PROJECT_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
GOALS_DIRS = [
    os.path.join(PROJECT_ROOT, ".dev", "goals"),
    os.path.join(PROJECT_ROOT, ".dev", "goals", "deferred"),
]
SKIP_FILES = {"DEPENDENCY.md", "index.md", "log.md"}

# Keywords → tags mapping
TAG_KEYWORDS = {
    "memory": ["memory"],
    "session": ["session"],
    "transcript": ["transcript"],
    "compaction": ["compaction"],
    "tui": ["tui"],
    "provider": ["provider"],
    "test": ["testing"],
    "skill": ["skill"],
    "tool": ["tool"],
    "shell": ["shell"],
    "checkpoint": ["checkpoint"],
    "sub.agent": ["sub-agent"],
    "multi.agent": ["multi-agent"],
    "sandbox": ["sandbox"],
    "cost": ["cost"],
    "benchmark": ["benchmark"],
    "refactor": ["refactor"],
}


def infer_tags(content: str, filename: str) -> list[str]:
    text = (content + " " + filename).lower()
    tags = set()
    for pattern, tag_list in TAG_KEYWORDS.items():
        if re.search(pattern, text):
            tags.update(tag_list)
    return sorted(tags)


def extract_roadmap(content: str) -> str | None:
    m = re.search(r"\*\*Roadmap\*\*\s*[:\—–-]\s*([^\n]+)", content)
    if m:
        val = m.group(1).strip().strip("*")
        # Clean up markdown bold if present
        val = re.sub(r"\*+", "", val).strip()
        return val
    return None


def extract_title_and_number(content: str) -> tuple[str, int | None]:
    m = re.search(r"^#\s+(.+)$", content, re.MULTILINE)
    if not m:
        return ("Untitled", None)
    heading = m.group(1).strip()
    # "Goal 38 — Persistent Memory"
    gm = re.match(r"Goal\s+(\d+)\s*[—–-]\s*(.+)", heading, re.IGNORECASE)
    if gm:
        return (gm.group(2).strip(), int(gm.group(1)))
    # "Goal: add a count_lines tool"
    gm2 = re.match(r"Goal:\s*(.+)", heading, re.IGNORECASE)
    if gm2:
        return (gm2.group(1).strip(), None)
    return (heading, None)


def yaml_str(s: str) -> str:
    """Return a YAML-safe single-line string (double-quoted, escaping special chars)."""
    s = s.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{s}"'


def build_frontmatter(content: str, filename: str, is_deferred: bool) -> str:
    title, goal_number = extract_title_and_number(content)
    roadmap = extract_roadmap(content)
    tags = infer_tags(content, filename)

    # Infer status
    if is_deferred:
        status = "deferred"
    elif re.search(r"\bWIP\b|\bin.progress\b", content, re.IGNORECASE):
        status = "in-progress"
    else:
        status = "open"

    lines = ["---", "type: Goal", f"title: {yaml_str(title)}"]
    if goal_number is not None:
        lines.append(f"goal_number: {goal_number}")
    lines.append(f"status: {status}")
    if roadmap:
        lines.append(f"roadmap: {yaml_str(roadmap)}")
    if tags:
        lines.append(f"tags: [{', '.join(tags)}]")
    lines.append("---")
    return "\n".join(lines) + "\n"


def has_type_in_frontmatter(content: str) -> bool:
    if not content.startswith("---\n"):
        return False
    end = content.find("\n---\n", 4)
    if end == -1:
        return False
    fm = content[4:end]
    return bool(re.search(r"^type\s*:", fm, re.MULTILINE))


def process_file(path: str, is_deferred: bool) -> str:
    filename = os.path.basename(path)
    if filename in SKIP_FILES:
        return f"  SKIP  (reserved) {filename}"

    with open(path, "r", encoding="utf-8") as f:
        content = f.read()

    if has_type_in_frontmatter(content):
        return f"  OK    (already OKF) {filename}"

    # If file already has frontmatter (no type), prepend type; else add fresh
    if content.startswith("---\n"):
        new_content = content.replace("---\n", "---\ntype: Goal\n", 1)
        action = "type injected"
    else:
        fm = build_frontmatter(content, filename, is_deferred)
        new_content = fm + "\n" + content
        action = "frontmatter added"

    with open(path, "w", encoding="utf-8") as f:
        f.write(new_content)
    return f"  DONE  ({action}) {filename}"


def main():
    print(f"Project root: {PROJECT_ROOT}\n")
    total = updated = 0
    for goals_dir in GOALS_DIRS:
        if not os.path.isdir(goals_dir):
            continue
        is_deferred = goals_dir.endswith("deferred")
        label = "deferred/" if is_deferred else ""
        files = sorted(
            f for f in os.listdir(goals_dir)
            if f.endswith(".md") and os.path.isfile(os.path.join(goals_dir, f))
        )
        print(f"── {label or 'goals/'} ({len(files)} files) ──")
        for fname in files:
            result = process_file(os.path.join(goals_dir, fname), is_deferred)
            print(result)
            total += 1
            if "DONE" in result:
                updated += 1
        print()

    print(f"Summary: {updated}/{total} files updated.")


if __name__ == "__main__":
    main()
