# Manual edit: okf-knowledge-engineering

**Date**: 2026-06-18
**Goal**: Adopt Google Open Knowledge Format (OKF) for Recursive's knowledge engineering — standardize skills, goals, and add interactive visualization tooling.

**Files touched**:
- `.recursive/skills/rust-patch-discipline/SKILL.md` — added `type: Skill`
- `.claude/skills/recursive-loop/SKILL.md` — added full OKF frontmatter (was missing entirely)
- `.claude/skills/gitnexus/gitnexus-{cli,debugging,exploring,guide,impact-analysis,refactoring}/SKILL.md` — added `type: Skill`
- `.dev/scripts/okf-skills.py` — new: batch-adds OKF `type` field to all project SKILL.md files (idempotent)
- `.dev/scripts/okf-goals.py` — new: batch-adds OKF frontmatter to `.dev/goals/`; **not applied** — goals reverted to original (goals are internal self-improve metadata, not agent-facing knowledge)
- `.dev/scripts/okf-viz.py` — new: generates self-contained interactive HTML viz for any OKF bundle
- `docs/architecture/` — new OKF bundle (23 concept docs, 23 cross-links): agent-loop, invariants, memory layers, all tools, providers, skills, sessions
- `.recursive/memory/project.md` — new: Layer 0 project memory, entry point to architecture bundle
- `.dev/viz/architecture-viz.html` — generated: architecture knowledge graph (23 nodes, 23 edges)

**Tests added**: none (no product `src/` code changed)

**Direction 3 correction**: Initial implementation mistakenly built a general viz tool. Corrected to build the actual Architecture Knowledge Bundle as `.dev/architecture/` — an OKF bundle of 23 cross-linked concept files covering agent loop, memory layers, tools, providers, skills, sessions, and invariants. Also created `.recursive/memory/project.md` as the Layer 0 entry point for the bundle.

**Notes**:
- OKF (Open Knowledge Format, v0.1) is Google's open spec: Markdown + YAML frontmatter bundle, only mandatory field is `type`.
- Recursive's existing Skill format was already ~90% OKF-compliant; adding `type: Skill` made it fully conformant.
- Goals had no frontmatter before; the script extracts title, goal_number (from "Goal NN — Title" headings), roadmap references, and infers status and tags from content.
- `okf-goals.py` and `okf-skills.py` are idempotent (safe to re-run).
- `okf-viz.py` generates a zero-dependency self-contained HTML using Cytoscape.js (CDN) + marked.js (CDN). No pip install required.
- Cross-links between goals are 0 for now because goal files don't use `[Name](/goals/other.md)` links. Future improvement: add cross-references in goals that share roadmap phases or dependencies.
- `.dev/viz/` is not committed to git (add to .gitignore if desired); re-generate anytime with the viz script.
