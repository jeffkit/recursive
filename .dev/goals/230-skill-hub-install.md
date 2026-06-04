# Goal 230: Skill Hub — find_skills + install_skill

## Motivation

The agent currently has a `load_skill` tool that loads skills **already installed
locally**. There is no way for the agent to discover new skills from a remote
registry or install them. This goal adds two tools:

- **`find_skills`** — fuzzy-search through locally installed skills by keyword
  (name + description), returning ranked matches. Replaces the need to call
  `load_skill` blind when the agent doesn't know the exact skill name.

- **`install_skill`** — search the [skillhub.cn](https://skillhub.cn) registry,
  present a TUI modal so the user can browse the zip contents and confirm, then
  extract the package to `~/.recursive/skills/<slug>/`. TUI-only (returns an
  error in headless mode).

## Skill Hub API (skillhub.cn)

All public skills are anonymously accessible.

```
Search:   GET https://api.skillhub.cn/api/v1/search?q={query}&limit={n}
Download: GET https://api.skillhub.cn/api/v1/download?slug={slug}
CDN fallback: https://skillhub-1388575217.cos.ap-guangzhou.myqcloud.com/skills/{slug}.zip
```

Search response (abbreviated):
```json
{
  "results": [{
    "slug": "pdf",
    "name": "Pdf",
    "description": "Comprehensive PDF manipulation toolkit ...",
    "description_zh": "全面的PDF处理工具包 ...",
    "downloads": 46837,
    "stars": 83,
    "version": "0.1.0",
    "category": "productivity"
  }]
}
```

The download endpoint returns a zip file containing a directory tree like:
```
pdf/
  SKILL.md
  refs/
    api-spec.md
  scripts/
    run.sh
```

## New Dependency

Add to `Cargo.toml` under `[dependencies]`:
```toml
zip = { version = "2", optional = true }
```

And add `"skill-hub"` feature:
```toml
skill-hub = ["dep:zip"]
```

Include `skill-hub` in the `default` feature set.

## Requirements

### 1. `find_skills` tool (`src/tools/find_skills.rs`)

Struct: `FindSkills { skills: Arc<Vec<Skill>> }`

Tool spec:
```json
{
  "name": "find_skills",
  "description": "Search locally installed skills by keyword (matches name and description). Returns a ranked list of matching skills with their names, descriptions, and injection modes.",
  "parameters": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "Keyword(s) to search for in skill names and descriptions"
      },
      "limit": {
        "type": "integer",
        "description": "Max results to return (default 10)"
      }
    },
    "required": ["query"]
  }
}
```

Matching algorithm:
1. Lowercase the query and all skill names/descriptions
2. Score each skill: +3 if query appears in name, +1 if in description
3. Return up to `limit` skills sorted by score descending
4. If no matches, return a message saying "no matching skills found"

Output format (plain text, one skill per line):
```
- pdf: Comprehensive PDF manipulation toolkit ... [mode=manual]
- pdf-extract: Extract text from PDF files for LLM processing [mode=trigger]
```

`side_effect_class` → `ToolSideEffect::ReadOnly`

### 2. `install_skill` tool (`src/tools/install_skill.rs`)

Struct:
```rust
pub struct InstallSkill {
    /// Channel to send SkillInstallRequest to the TUI event loop
    pub tui_tx: Option<tokio::sync::mpsc::UnboundedSender<TuiSkillInstallEvent>>,
    /// Receiver end for install confirmation
    // (held here, sent to TUI at startup)
}
```

Because the tool needs to communicate with the TUI, use a
`tokio::sync::oneshot` channel pattern like `PendingPermission`:

1. The tool sends a `TuiSkillInstallEvent::Request { results, responder }` to
   the TUI via `mpsc::UnboundedSender`.
2. The TUI displays the `SkillInstall` modal, waits for user input, then sends
   `true/false` via the `oneshot::Sender<bool>` responder.
3. The tool awaits the oneshot receiver.

**Headless guard**: if `tui_tx.is_none()`, return:
```
Error: install_skill is only available in TUI mode. Use find_skills to browse
locally installed skills, or install manually with the skillhub CLI:
  skillhub install <slug>
```

**Tool flow**:
1. Call `GET https://api.skillhub.cn/api/v1/search?q={query}&limit=8`
2. Parse results into `Vec<SkillSearchResult>`
3. Send `TuiSkillInstallEvent::Request` to TUI with the results
4. TUI shows `Modal::SkillInstall` (see below)
5. Await user's choice: `(slug, confirmed)` or `None` (cancel)
6. If cancelled or `confirmed == false`, return `"Installation cancelled."`
7. Download zip from primary URL, fall back to CDN URL on failure
8. Extract zip to `~/.recursive/skills/<slug>/`
9. Return `"Installed skill '<slug>' to ~/.recursive/skills/<slug>/"`

Tool spec:
```json
{
  "name": "install_skill",
  "description": "Search skillhub.cn for skills matching a query and install the chosen skill. Only available in TUI mode — the user must review the skill files and confirm installation.",
  "parameters": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "Keywords to search for on skillhub.cn"
      }
    },
    "required": ["query"]
  }
}
```

`side_effect_class` → `ToolSideEffect::Write` (installs files)

### 3. TUI: `TuiSkillInstallEvent` and `Modal::SkillInstall`

#### 3a. New event type (add to `src/tui/events.rs` or a new submodule)

```rust
/// A search result entry from skillhub.cn.
#[derive(Debug, Clone)]
pub struct SkillSearchResult {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub downloads: u64,
    pub stars: u32,
    pub version: String,
}

/// A file entry inside a skill's zip archive.
#[derive(Debug, Clone)]
pub struct SkillZipFile {
    pub path: String,  // e.g. "pdf/SKILL.md", "pdf/refs/api-spec.md"
    pub content: String,  // UTF-8 text content (binary files shown as "<binary>")
}

/// Event sent from install_skill tool to TUI.
pub enum TuiSkillInstallEvent {
    Request {
        query: String,
        results: Vec<SkillSearchResult>,
        /// Send back (slug, confirm=true) or None to cancel
        responder: tokio::sync::oneshot::Sender<Option<(String, Vec<SkillZipFile>)>>,
    },
    /// Sent after user selects a skill: TUI needs to fetch file listing
    /// The tool pre-fetches the zip contents before sending Request,
    /// so this variant is NOT needed — files are bundled in Request.
}
```

**Note**: To keep things simple, `install_skill` fetches the zip *before*
sending the TUI event. It stores all files in memory as `Vec<SkillZipFile>`
and includes them in the `Request`. The TUI modal only needs to display;
the tool does the I/O.

#### 3b. `Modal::SkillInstall` variant

Add to `src/tui/ui/modal.rs`:

```rust
/// Sub-page within the SkillInstall modal.
#[derive(Clone, Debug, PartialEq)]
pub enum SkillInstallPage {
    /// Showing search results list.
    Results { selected: usize },
    /// Showing file tree for the selected skill.
    Files { selected: usize },
    /// Viewing the content of a specific file.
    Preview { file_idx: usize, scroll: u16 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SkillInstallState {
    pub query: String,
    pub results: Vec<SkillSearchResult>,
    pub selected_slug: Option<String>,
    pub files: Vec<SkillZipFile>,
    pub page: SkillInstallPage,
}

// In the Modal enum:
Modal::SkillInstall(SkillInstallState)
```

#### 3c. Rendering (`render_skill_install`)

**Results page** (SkillInstallPage::Results):
```
 Install Skill — "pdf"                         [1/3]
 
 ▶ Pdf            ⭐ 83   ↓ 46.8k   v0.1.0
   Pdf Extract    ⭐ 31   ↓ 32.1k   v1.0.0
   PDF Extractor  ⭐ 12   ↓ 8.9k    v0.2.1

 Description:
   Comprehensive PDF manipulation toolkit...

 [↑↓] navigate  [Enter] select & browse files  [Esc] cancel
```

**Files page** (SkillInstallPage::Files):
```
 Pdf — Files                              [q: cancel]
 
   pdf/
   ├─ ▶ SKILL.md              (1.2kb)
   ├─   refs/
   │    ├─   api-spec.md      (3.1kb)
   │    └─   examples.md      (1.8kb)
   └─   scripts/
        └─   run.sh           (0.5kb)

 [↑↓] navigate  [v/Enter] preview  [y] confirm install  [Esc] back
```

**Preview page** (SkillInstallPage::Preview):
```
 SKILL.md                              [PgUp/PgDn scroll]
 
 ---
 name: pdf
 description: Comprehensive PDF manipulation toolkit
 ---
 
 Use this skill when working with PDF files...
 ...

 [Esc] back to files
```

#### 3d. Key handling in `src/tui/app/event_loop.rs`

When a `Modal::SkillInstall(_)` is active, key events go to the skill
install handler instead of the default modal handler:

- `Results` page: `↑/↓` → change `selected`, `Enter` → go to `Files`,
  `Esc` → send `None` to responder, pop modal
- `Files` page: `↑/↓` → change `selected`, `v`/`Enter` → go to `Preview`,
  `y` → send `Some((slug, files))` to responder, pop modal, return to chat,
  `Esc` → go back to `Results`
- `Preview` page: `PgUp/PgDn/↑/↓` → scroll, `Esc` → go back to `Files`

The `responder` is a `oneshot::Sender` stored in the modal state. After
sending to it, move the sender out (take it via `Option`) so it can't be
double-sent.

### 4. `TuiSkillInstallEvent` channel wiring

In `src/tui/mod.rs` (or wherever the TUI runtime is initialised):
- Create `mpsc::unbounded_channel::<TuiSkillInstallEvent>()`
- Pass the `Sender` to `InstallSkill::new(Some(tx))`
- In the TUI event loop, add a branch to `tokio::select!` that receives
  from the `Receiver` and calls `app.push_modal(Modal::SkillInstall(...))`

### 5. Register tools in `src/tools/mod.rs`

- Register `FindSkills` unconditionally (local only, safe everywhere)
- Register `InstallSkill` only when a TUI sender is available (the CLI
  builder passes `None` in headless mode; the TUI builder passes `Some(tx)`)
- Add both to the offline tool catalog in `src/tui/app/mod.rs`

### 6. New dependency justification

`zip = "2"` — needed to extract downloaded skill zip archives. No other
crate in `Cargo.toml` provides zip extraction. The `zip` crate is the
standard choice in the Rust ecosystem and has no C dependencies.

## Files to create / modify

| File | Action |
|------|--------|
| `src/tools/find_skills.rs` | Create |
| `src/tools/install_skill.rs` | Create |
| `src/tui/ui/modal.rs` | Modify — add `SkillInstall` variant + render |
| `src/tui/app/event_loop.rs` | Modify — key routing for skill install modal |
| `src/tui/app/mod.rs` | Modify — offline tool catalog, channel wiring |
| `src/tui/mod.rs` | Modify — create mpsc channel, pass to tool |
| `src/tools/mod.rs` | Modify — register new tools |
| `Cargo.toml` | Modify — add `zip` dependency + `skill-hub` feature |

## Test requirements

- `find_skills`: unit tests covering (a) exact name match, (b) partial
  description match, (c) no-match returns empty message, (d) score ordering
- `install_skill`: unit test verifying headless guard returns expected error
  message when `tui_tx` is `None`
- TUI modal state: unit test for `SkillInstallPage` navigation transitions

## Acceptance criteria

1. `cargo test --workspace` passes
2. `cargo clippy --all-targets --all-features -- -D warnings` passes
3. `cargo fmt --all` produces no diff
4. `find_skills(query="pdf")` on a workspace with a local pdf skill returns
   it ranked first
5. `install_skill(query="pdf")` in headless mode returns the expected error
6. The TUI binary compiles and the new modal variant is reachable
