#!/usr/bin/env python3
"""
okf-skills.py — Add OKF-compliant `type: Skill` frontmatter to all SKILL.md
files within the Recursive project.

Safe to re-run: skips files that already have `type:` in their frontmatter.
"""

import os
import re
import sys

PROJECT_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

SKILL_FILES = [
    ".recursive/skills/rust-patch-discipline/SKILL.md",
    ".claude/skills/recursive-loop/SKILL.md",
    ".claude/skills/gitnexus/gitnexus-cli/SKILL.md",
    ".claude/skills/gitnexus/gitnexus-debugging/SKILL.md",
    ".claude/skills/gitnexus/gitnexus-exploring/SKILL.md",
    ".claude/skills/gitnexus/gitnexus-guide/SKILL.md",
    ".claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md",
    ".claude/skills/gitnexus/gitnexus-refactoring/SKILL.md",
]

DESCRIPTIONS = {
    "recursive-loop": "Loop orchestrator for the Recursive self-improvement workflow. Reads the roadmap, picks goals, launches self-improve.sh, and handles results.",
}


def has_frontmatter(content: str) -> bool:
    return content.startswith("---\n")


def frontmatter_has_type(content: str) -> bool:
    if not has_frontmatter(content):
        return False
    end = content.find("\n---\n", 4)
    if end == -1:
        return False
    fm = content[4:end]
    return bool(re.search(r"^type\s*:", fm, re.MULTILINE))


def add_type_to_frontmatter(content: str) -> str:
    """Insert `type: Skill` as the first key after the opening `---`."""
    return content.replace("---\n", "---\ntype: Skill\n", 1)


def add_full_frontmatter(path: str, content: str) -> str:
    """Wrap a file that has no frontmatter with a minimal OKF-compliant block."""
    skill_name = os.path.basename(os.path.dirname(path))
    # Extract title from first H1
    m = re.search(r"^#\s+(.+)$", content, re.MULTILINE)
    title = m.group(1).strip() if m else skill_name
    # Use overridden description or fall back to first non-heading line
    description = DESCRIPTIONS.get(skill_name)
    if not description:
        for line in content.splitlines():
            stripped = line.strip()
            if stripped and not stripped.startswith("#"):
                description = stripped[:120]
                break
        description = description or title
    # Escape any double-quotes in description
    description = description.replace('"', '\\"')
    fm = (
        f"---\n"
        f"type: Skill\n"
        f"name: {skill_name}\n"
        f'description: "{description}"\n'
        f"---\n\n"
    )
    return fm + content


def process(rel_path: str) -> str:
    abs_path = os.path.join(PROJECT_ROOT, rel_path)
    if not os.path.isfile(abs_path):
        return f"  SKIP  (not found) {rel_path}"
    with open(abs_path, "r", encoding="utf-8") as f:
        content = f.read()

    if frontmatter_has_type(content):
        return f"  OK    (already OKF) {rel_path}"

    if has_frontmatter(content):
        new_content = add_type_to_frontmatter(content)
        action = "type added"
    else:
        new_content = add_full_frontmatter(abs_path, content)
        action = "frontmatter added"

    with open(abs_path, "w", encoding="utf-8") as f:
        f.write(new_content)
    return f"  DONE  ({action}) {rel_path}"


def main():
    print(f"Project root: {PROJECT_ROOT}\n")
    results = [process(p) for p in SKILL_FILES]
    for r in results:
        print(r)
    updated = sum(1 for r in results if "DONE" in r)
    print(f"\n{updated}/{len(SKILL_FILES)} files updated.")


if __name__ == "__main__":
    main()
