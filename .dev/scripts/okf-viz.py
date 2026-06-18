#!/usr/bin/env python3
"""
okf-viz.py — Generate a self-contained interactive HTML visualization for any
OKF bundle directory.

Usage:
    python .dev/scripts/okf-viz.py --bundle <path/to/bundle> [--out viz.html] [--name "My Bundle"]

Output: a single HTML file (default: <bundle>/viz.html) that embeds:
  - A force-directed concept graph (Cytoscape.js)
  - Frontmatter metadata panel
  - Rendered Markdown body (marked.js)
  - Type filter + search + backlinks list

No server required; open the HTML file in any browser.
Dependencies: Python 3.9+ stdlib only (no pip install needed).
"""

import argparse
import json
import os
import re
import sys
from pathlib import Path

RESERVED = {"index.md", "log.md"}

# ── Frontmatter parser ────────────────────────────────────────────────────────

def parse_frontmatter(content: str) -> tuple[dict, str]:
    """Return (frontmatter_dict, body). Naive YAML key: value parser."""
    if not content.startswith("---\n"):
        return {}, content
    end = content.find("\n---\n", 4)
    if end == -1:
        return {}, content
    yaml_text = content[4:end]
    body = content[end + 5:]
    fm: dict = {}
    current_key = None
    current_val_lines: list[str] = []

    def flush():
        if current_key:
            fm[current_key] = "\n".join(current_val_lines).strip()

    for line in yaml_text.splitlines():
        m = re.match(r'^(\w[\w_-]*)\s*:\s*(.*)', line)
        if m:
            flush()
            current_key = m.group(1)
            current_val_lines = [m.group(2)]
        elif line.startswith("  ") and current_key:
            current_val_lines.append(line.strip())
        else:
            flush()
            current_key = None
            current_val_lines = []
    flush()

    # Parse tags list: "[a, b, c]" or "- a\n- b"
    if "tags" in fm:
        raw = fm["tags"]
        if raw.startswith("[") and raw.endswith("]"):
            fm["tags"] = [t.strip().strip('"\'') for t in raw[1:-1].split(",") if t.strip()]
        else:
            fm["tags"] = [t.lstrip("- ").strip() for t in raw.splitlines() if t.strip()]

    return fm, body


# ── Link extractor ────────────────────────────────────────────────────────────

def extract_links(body: str, concept_id: str, all_ids: set[str]) -> list[str]:
    """Return list of concept IDs that this concept links to."""
    targets = []
    for m in re.finditer(r'\[([^\]]+)\]\(([^)]+)\)', body):
        href = m.group(2)
        if href.startswith("http") or href.startswith("mailto"):
            continue
        # Normalise: strip leading / and .md suffix
        href = href.lstrip("/").removesuffix(".md")
        if href in all_ids and href != concept_id:
            targets.append(href)
    return list(dict.fromkeys(targets))  # deduplicate, preserve order


# ── Bundle loader ─────────────────────────────────────────────────────────────

def load_bundle(bundle_root: Path) -> list[dict]:
    concepts = []
    for md_path in sorted(bundle_root.rglob("*.md")):
        if md_path.name in RESERVED:
            continue
        rel = md_path.relative_to(bundle_root)
        concept_id = str(rel).removesuffix(".md").replace("\\", "/")
        with open(md_path, "r", encoding="utf-8") as f:
            content = f.read()
        fm, body = parse_frontmatter(content)
        concepts.append({
            "id": concept_id,
            "fm": fm,
            "body": body,
            "path": str(rel),
        })
    return concepts


# ── HTML template ─────────────────────────────────────────────────────────────

HTML_TEMPLATE = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>__BUNDLE_NAME__ — OKF Knowledge Graph</title>
<script src="https://cdnjs.cloudflare.com/ajax/libs/cytoscape/3.29.2/cytoscape.min.js"></script>
<script src="https://cdnjs.cloudflare.com/ajax/libs/marked/12.0.0/marked.min.js"></script>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
         background: #0f1117; color: #e2e8f0; height: 100vh; display: flex;
         flex-direction: column; overflow: hidden; }
  header { background: #1a1d27; border-bottom: 1px solid #2d3149;
           padding: 10px 16px; display: flex; align-items: center; gap: 12px; flex-shrink: 0; }
  header h1 { font-size: 15px; font-weight: 600; color: #a5b4fc; }
  .badge { background: #2d3149; color: #818cf8; border-radius: 12px;
           padding: 2px 8px; font-size: 11px; }
  .controls { display: flex; gap: 8px; margin-left: auto; align-items: center; }
  input[type=text] { background: #2d3149; border: 1px solid #3d4266;
                     color: #e2e8f0; padding: 4px 10px; border-radius: 6px;
                     font-size: 12px; width: 160px; }
  select { background: #2d3149; border: 1px solid #3d4266; color: #e2e8f0;
           padding: 4px 8px; border-radius: 6px; font-size: 12px; }
  .main { display: flex; flex: 1; overflow: hidden; }
  #cy { flex: 1; background: #0f1117; }
  .panel { width: 340px; background: #1a1d27; border-left: 1px solid #2d3149;
           overflow-y: auto; padding: 16px; flex-shrink: 0; }
  .panel h2 { font-size: 14px; color: #a5b4fc; margin-bottom: 8px; }
  .panel .meta { margin-bottom: 12px; }
  .panel .meta dt { font-size: 11px; color: #64748b; text-transform: uppercase;
                    letter-spacing: .05em; margin-top: 6px; }
  .panel .meta dd { font-size: 12px; color: #cbd5e1; margin-left: 0; }
  .tag { display: inline-block; background: #1e293b; color: #7dd3fc;
         border-radius: 4px; padding: 1px 6px; font-size: 11px; margin: 2px 2px 0 0; }
  .body-content { font-size: 12px; line-height: 1.6; color: #cbd5e1; margin-top: 12px; }
  .body-content h1,.body-content h2 { color: #a5b4fc; font-size: 13px; margin: 10px 0 4px; }
  .body-content h3 { color: #818cf8; font-size: 12px; margin: 8px 0 3px; }
  .body-content code { background: #1e293b; padding: 1px 4px; border-radius: 3px;
                       font-size: 11px; }
  .body-content pre { background: #1e293b; padding: 8px; border-radius: 6px;
                      overflow-x: auto; margin: 6px 0; }
  .body-content pre code { background: none; padding: 0; }
  .body-content table { border-collapse: collapse; width: 100%; font-size: 11px; }
  .body-content th, .body-content td { border: 1px solid #2d3149;
                                        padding: 3px 6px; text-align: left; }
  .body-content th { background: #2d3149; color: #a5b4fc; }
  .body-content a { color: #818cf8; cursor: pointer; }
  .backlinks { margin-top: 14px; }
  .backlinks h3 { font-size: 12px; color: #64748b; margin-bottom: 6px; }
  .backlink-item { font-size: 11px; color: #818cf8; cursor: pointer;
                   padding: 2px 0; text-decoration: underline; }
  .placeholder { color: #4a5568; font-size: 13px; text-align: center;
                 padding: 40px 20px; }
</style>
</head>
<body>
<header>
  <h1>__BUNDLE_NAME__</h1>
  <span class="badge" id="node-count">— concepts</span>
  <div class="controls">
    <input type="text" id="search" placeholder="Search concepts…">
    <select id="type-filter"><option value="">All types</option></select>
    <select id="layout-select">
      <option value="cose">Force</option>
      <option value="concentric">Concentric</option>
      <option value="breadthfirst">Tree</option>
      <option value="circle">Circle</option>
      <option value="grid">Grid</option>
    </select>
  </div>
</header>
<div class="main">
  <div id="cy"></div>
  <div class="panel" id="panel">
    <div class="placeholder">Click a node to explore</div>
  </div>
</div>
<script>
const BUNDLE = __BUNDLE_JSON__;

// ── colour palette per type ───────────────────────────────────────────────────
const TYPE_COLORS = [
  "#818cf8","#34d399","#f472b6","#fbbf24","#60a5fa","#a78bfa",
  "#fb923c","#4ade80","#38bdf8","#e879f9"
];
const typeColorMap = {};
let colorIdx = 0;
function colorFor(type) {
  if (!typeColorMap[type]) typeColorMap[type] = TYPE_COLORS[colorIdx++ % TYPE_COLORS.length];
  return typeColorMap[type];
}

// ── build backlink index ──────────────────────────────────────────────────────
const backlinks = {};
BUNDLE.edges.forEach(e => {
  if (!backlinks[e.target]) backlinks[e.target] = [];
  backlinks[e.target].push(e.source);
});

// ── populate type filter ──────────────────────────────────────────────────────
const types = [...new Set(BUNDLE.nodes.map(n => n.fm.type || "unknown"))].sort();
const sel = document.getElementById("type-filter");
types.forEach(t => {
  const opt = document.createElement("option");
  opt.value = t; opt.textContent = t;
  sel.appendChild(opt);
});

// ── init Cytoscape ────────────────────────────────────────────────────────────
const cy = cytoscape({
  container: document.getElementById("cy"),
  elements: [
    ...BUNDLE.nodes.map(n => ({ data: {
      id: n.id,
      label: n.fm.title || n.id.split("/").pop(),
      type: n.fm.type || "unknown",
      color: colorFor(n.fm.type || "unknown"),
    }})),
    ...BUNDLE.edges.map(e => ({ data: { source: e.source, target: e.target }})),
  ],
  style: [
    { selector: "node", style: {
      "background-color": "data(color)", label: "data(label)",
      "font-size": 10, color: "#e2e8f0", "text-valign": "bottom",
      "text-margin-y": 4, width: 22, height: 22,
      "text-max-width": 100, "text-wrap": "ellipsis",
    }},
    { selector: "node:selected", style: {
      "border-width": 3, "border-color": "#fff", width: 28, height: 28,
    }},
    { selector: "edge", style: {
      width: 1, "line-color": "#2d3149", "target-arrow-color": "#3d4266",
      "target-arrow-shape": "triangle", "curve-style": "bezier",
    }},
  ],
  layout: { name: "cose", animate: false, randomize: true,
            nodeRepulsion: 8000, gravity: 0.5 },
});

document.getElementById("node-count").textContent =
  `${BUNDLE.nodes.length} concepts, ${BUNDLE.edges.length} links`;

// ── layout switcher ───────────────────────────────────────────────────────────
document.getElementById("layout-select").addEventListener("change", e => {
  cy.layout({ name: e.target.value, animate: true }).run();
});

// ── type filter ───────────────────────────────────────────────────────────────
document.getElementById("type-filter").addEventListener("change", e => {
  const val = e.target.value;
  cy.nodes().forEach(n => {
    n.style("display", (!val || n.data("type") === val) ? "element" : "none");
  });
});

// ── search ────────────────────────────────────────────────────────────────────
document.getElementById("search").addEventListener("input", e => {
  const q = e.target.value.toLowerCase();
  cy.nodes().forEach(n => {
    const match = !q || n.data("label").toLowerCase().includes(q) ||
                  n.data("id").toLowerCase().includes(q);
    n.style("opacity", match ? 1 : 0.15);
  });
});

// ── node click → detail panel ─────────────────────────────────────────────────
const nodeMap = Object.fromEntries(BUNDLE.nodes.map(n => [n.id, n]));

cy.on("tap", "node", e => showPanel(e.target.data("id")));

function showPanel(conceptId) {
  const node = nodeMap[conceptId];
  if (!node) return;
  const fm = node.fm;
  const tags = Array.isArray(fm.tags) ? fm.tags : (fm.tags ? [fm.tags] : []);
  const bodyHtml = marked.parse(node.body || "");
  const blinks = (backlinks[conceptId] || []).map(id =>
    `<div class="backlink-item" onclick="showPanel('${id}');selectNode('${id}')">${id}</div>`
  ).join("");

  const metaRows = Object.entries(fm)
    .filter(([k]) => !["title","description","type","tags"].includes(k))
    .map(([k,v]) => `<dt>${k}</dt><dd>${Array.isArray(v) ? v.join(", ") : v}</dd>`)
    .join("");

  document.getElementById("panel").innerHTML = `
    <h2>${fm.title || conceptId}</h2>
    <div style="font-size:11px;color:#64748b;margin-bottom:8px">${conceptId}</div>
    ${fm.type ? `<span class="tag" style="background:#1e293b;color:${colorFor(fm.type)}">${fm.type}</span>` : ""}
    ${tags.map(t => `<span class="tag">${t}</span>`).join("")}
    <div class="meta">
      ${fm.description ? `<dt>description</dt><dd>${fm.description}</dd>` : ""}
      ${metaRows}
    </div>
    <div class="body-content">${bodyHtml}</div>
    ${blinks ? `<div class="backlinks"><h3>Cited by</h3>${blinks}</div>` : ""}
  `;

  // Rewire internal links in the rendered body
  document.querySelectorAll(".body-content a").forEach(a => {
    const href = a.getAttribute("href") || "";
    if (!href.startsWith("http")) {
      const target = href.replace(/^\//, "").replace(/\.md$/, "");
      if (nodeMap[target]) {
        a.addEventListener("click", ev => {
          ev.preventDefault();
          showPanel(target);
          selectNode(target);
        });
      }
    }
  });
}

function selectNode(id) {
  cy.nodes().unselect();
  const n = cy.getElementById(id);
  if (n) { n.select(); cy.animate({ fit: { eles: n, padding: 80 }}); }
}
</script>
</body>
</html>
"""


# ── main ──────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="Generate OKF bundle visualization")
    parser.add_argument("--bundle", required=True, help="Path to OKF bundle root")
    parser.add_argument("--out", help="Output HTML path (default: <bundle>/viz.html)")
    parser.add_argument("--name", help="Display name in the header")
    args = parser.parse_args()

    bundle_root = Path(args.bundle).resolve()
    if not bundle_root.is_dir():
        print(f"ERROR: bundle directory not found: {bundle_root}", file=sys.stderr)
        sys.exit(1)

    bundle_name = args.name or bundle_root.name
    out_path = Path(args.out) if args.out else bundle_root / "viz.html"

    print(f"Loading bundle: {bundle_root}")
    concepts = load_bundle(bundle_root)
    print(f"  {len(concepts)} concepts found")

    all_ids = {c["id"] for c in concepts}

    # Build edges
    edges = []
    for c in concepts:
        for target in extract_links(c["body"], c["id"], all_ids):
            edges.append({"source": c["id"], "target": target})

    print(f"  {len(edges)} cross-links extracted")

    bundle_data = {
        "nodes": [{"id": c["id"], "fm": c["fm"], "body": c["body"]} for c in concepts],
        "edges": edges,
    }

    html = (
        HTML_TEMPLATE
        .replace("__BUNDLE_NAME__", bundle_name)
        .replace("__BUNDLE_JSON__", json.dumps(bundle_data, ensure_ascii=False))
    )

    out_path.parent.mkdir(parents=True, exist_ok=True)
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(html)

    print(f"  Written: {out_path}")
    print("Open in any browser — no server needed.")


if __name__ == "__main__":
    main()
