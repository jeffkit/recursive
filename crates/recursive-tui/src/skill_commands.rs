//! Skill-backed slash commands for the TUI (Goal 169).
//!
//! A *skill* is a Markdown file with optional YAML front-matter that
//! describes a reusable prompt template.  Every `*.md` file found in the
//! standard skill directories is automatically registered as a `/name`
//! slash command.
//!
//! ## Search paths (priority order — first name wins on collision)
//!
//! 1. `<workspace>/.recursive/skills/` — project-level (committed to repo)
//! 2. `~/.recursive/skills/` — user-level (global)
//! 3. Built-in commands from `CommandRegistry::default_set()` (never shadowed
//!    by skills)
//!
//! ## Skill file format
//!
//! ```markdown
//! ---
//! name: refactor          # defaults to filename stem
//! description: Refactor the selected code for clarity
//! aliases: [rf]
//! argument_hint: "<file-or-description>"
//! allowed_tools: [Read, Edit, Bash]
//! ---
//!
//! Refactor the following with these goals:
//! - Single responsibility
//!
//! $ARGUMENTS
//! ```
//!
//! `$ARGUMENTS` (or `{{args}}`) is replaced with whatever the user types
//! after the command name.

use std::path::{Path, PathBuf};

// ──────────────────────────────────────────────────────────────────────────
// SkillCommand
// ──────────────────────────────────────────────────────────────────────────

/// A skill-backed slash command parsed from a `.md` file.
#[derive(Debug, Clone)]
pub struct SkillCommand {
    /// Command name (no leading `/`).
    pub name: String,
    /// Short description shown in `/help` and the completion popup.
    pub description: String,
    /// Alternative invocation names.
    pub aliases: Vec<String>,
    /// Argument hint shown after the command name in usage.
    pub argument_hint: String,
    /// Optional explicit tool allow-list (enforcement deferred to v2).
    pub allowed_tools: Option<Vec<String>>,
    /// The prompt template body (with `$ARGUMENTS` / `{{args}}` intact).
    pub prompt_template: String,
    /// Filesystem path the skill was loaded from.
    pub source_path: PathBuf,
}

impl SkillCommand {
    /// Expand the prompt template, substituting `args` for `$ARGUMENTS` /
    /// `{{args}}`.
    pub fn expand(&self, args: &str) -> String {
        self.prompt_template
            .replace("$ARGUMENTS", args)
            .replace("{{args}}", args)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// SkillCommandLoader
// ──────────────────────────────────────────────────────────────────────────

/// Loads [`SkillCommand`]s from the standard search paths.
pub struct SkillCommandLoader;

impl SkillCommandLoader {
    /// Load all skill files from the standard search paths.
    ///
    /// Project-level skills (`.recursive/skills/`) take priority over
    /// user-level skills (`~/.recursive/skills/`).  Name collisions are
    /// resolved by first-seen wins (project > user).
    pub fn load(workspace: &Path) -> Vec<SkillCommand> {
        let mut commands: Vec<SkillCommand> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 1. Project-level.
        let project_dir = workspace.join(".recursive").join("skills");
        for skill in Self::load_dir(&project_dir) {
            if seen.insert(skill.name.clone()) {
                commands.push(skill);
            }
        }

        // 2. User-level.
        if let Some(home) = dirs::home_dir() {
            let user_dir = home.join(".recursive").join("skills");
            for skill in Self::load_dir(&user_dir) {
                if seen.insert(skill.name.clone()) {
                    commands.push(skill);
                }
            }
        }

        commands
    }

    /// Load all `*.md` skill files from a single directory.
    ///
    /// Files that fail to read or parse are skipped with a `tracing::warn!`
    /// so users debugging "why isn't my skill showing up?" can find the
    /// reason in the log instead of having to bisect the directory.
    pub fn load_dir(dir: &Path) -> Vec<SkillCommand> {
        let entries = match std::fs::read_dir(dir) {
            Ok(it) => it,
            Err(err) => {
                if dir.exists() {
                    // Permissions or some other real failure — surface it.
                    tracing::warn!(
                        target: "recursive::tui::skill_commands",
                        dir = %dir.display(),
                        error = %err,
                        "skill_commands: failed to read directory"
                    );
                }
                return Vec::new();
            }
        };

        let mut skills: Vec<SkillCommand> = entries
            .flatten()
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|x| x.eq_ignore_ascii_case("md"))
                    .unwrap_or(false)
            })
            .filter_map(|e| Self::parse_file(&e.path()))
            .collect();

        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Parse a single skill file. Returns `None` on IO / parse errors and
    /// emits a `tracing::warn!` describing why the file was skipped.
    pub fn parse_file(path: &Path) -> Option<SkillCommand> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    target: "recursive::tui::skill_commands",
                    path = %path.display(),
                    error = %err,
                    "skill_commands: failed to read .md file; skipping"
                );
                return None;
            }
        };
        let parsed = Self::parse_content(path, &raw);
        if parsed.is_none() {
            tracing::warn!(
                target: "recursive::tui::skill_commands",
                path = %path.display(),
                "skill_commands: front-matter / filename produced an empty command name; skipping"
            );
        }
        parsed
    }

    /// Parse skill content (separated from IO for easy unit-testing).
    pub fn parse_content(path: &Path, content: &str) -> Option<SkillCommand> {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        // Split front-matter from body.
        let (front, body) = split_frontmatter(content);

        // Defaults from filename stem.
        let mut name = stem.clone();
        let mut description = String::new();
        let mut aliases: Vec<String> = Vec::new();
        let mut argument_hint = String::new();
        let mut allowed_tools: Option<Vec<String>> = None;

        // Parse YAML front-matter if present.
        if let Some(fm) = front {
            // We parse a minimal subset of YAML manually to avoid pulling in
            // a YAML dependency (project policy: no new deps without
            // justification).  The format is simple enough for line-by-line
            // parsing: `key: value` with optional list values on one line.
            for line in fm.lines() {
                let line = line.trim();
                if let Some((k, v)) = line.split_once(':') {
                    let k = k.trim();
                    let v = v.trim();
                    match k {
                        "name" => name = v.to_string(),
                        "description" => description = v.to_string(),
                        "argument_hint" => argument_hint = v.trim_matches('"').to_string(),
                        "aliases" => {
                            // Parse `[a, b, c]` inline list.
                            aliases = parse_inline_list(v);
                        }
                        "allowed_tools" => {
                            let tools = parse_inline_list(v);
                            if !tools.is_empty() {
                                allowed_tools = Some(tools);
                            }
                        }
                        _ => {} // ignore unknown fields
                    }
                }
            }
        }

        // Fall back to first non-blank line of body for description.
        if description.is_empty() {
            description = body
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .trim_start_matches('#')
                .trim()
                .to_string();
        }

        // Sanitize name: lowercase, replace spaces/underscores with hyphens.
        name = name
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        // Strip leading/trailing hyphens.
        let name = name.trim_matches('-').to_string();
        if name.is_empty() {
            return None;
        }

        Some(SkillCommand {
            name,
            description,
            aliases,
            argument_hint,
            allowed_tools,
            prompt_template: body.trim().to_string(),
            source_path: path.to_path_buf(),
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────

/// Split `---\n<front-matter>\n---\n<body>` into `(Some(front), body)`.
/// Returns `(None, full_content)` when no front-matter delimiter is found.
fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return (None, content);
    }
    // Skip the opening `---`.
    let rest = &content["---".len()..];
    // Find the closing `---`.
    if let Some(pos) = rest.find("\n---") {
        let fm = &rest[..pos];
        let body = &rest[pos + "\n---".len()..];
        (Some(fm.trim()), body)
    } else {
        (None, content)
    }
}

/// Parse an inline YAML list value like `[a, b, c]` or `a` into a `Vec<String>`.
fn parse_inline_list(s: &str) -> Vec<String> {
    let s = s.trim();
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        inner
            .split(',')
            .map(|t| t.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|t| !t.is_empty())
            .collect()
    } else if s.is_empty() {
        Vec::new()
    } else {
        vec![s.trim_matches('"').trim_matches('\'').to_string()]
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_path(name: &str) -> PathBuf {
        PathBuf::from(format!("/fake/{name}.md"))
    }

    // ── parse_content: happy path ─────────────────────────────────────────

    #[test]
    fn parse_skill_with_full_frontmatter() {
        let content = r#"---
name: refactor
description: Refactor code for clarity
aliases: [rf, refac]
argument_hint: "<file>"
allowed_tools: [Read, Edit]
---

Refactor this:

$ARGUMENTS
"#;
        let skill = SkillCommandLoader::parse_content(&fake_path("refactor"), content).unwrap();
        assert_eq!(skill.name, "refactor");
        assert_eq!(skill.description, "Refactor code for clarity");
        assert_eq!(skill.aliases, vec!["rf", "refac"]);
        assert_eq!(skill.argument_hint, "<file>");
        assert_eq!(skill.allowed_tools.as_deref().unwrap(), &["Read", "Edit"]);
        assert!(skill.prompt_template.contains("$ARGUMENTS"));
    }

    #[test]
    fn parse_skill_without_frontmatter_uses_filename_stem() {
        let content = "Explain the code at $ARGUMENTS\n";
        let skill = SkillCommandLoader::parse_content(&fake_path("explain"), content).unwrap();
        assert_eq!(skill.name, "explain");
        // Description falls back to first non-blank body line.
        assert!(skill.description.contains("Explain"));
        assert!(skill.prompt_template.contains("$ARGUMENTS"));
    }

    #[test]
    fn parse_skill_description_fallback_to_first_body_line() {
        let content = "---\nname: hello\n---\n\nFirst line description\n\nMore content\n";
        let skill = SkillCommandLoader::parse_content(&fake_path("hello"), content).unwrap();
        assert_eq!(skill.description, "First line description");
    }

    // ── expand ────────────────────────────────────────────────────────────

    #[test]
    fn expand_substitutes_dollar_arguments() {
        let skill = SkillCommand {
            name: "test".into(),
            description: "test".into(),
            aliases: vec![],
            argument_hint: "".into(),
            allowed_tools: None,
            prompt_template: "Fix $ARGUMENTS for me".into(),
            source_path: fake_path("test"),
        };
        assert_eq!(skill.expand("src/lib.rs"), "Fix src/lib.rs for me");
    }

    #[test]
    fn expand_substitutes_mustache_args() {
        let skill = SkillCommand {
            name: "test".into(),
            description: "test".into(),
            aliases: vec![],
            argument_hint: "".into(),
            allowed_tools: None,
            prompt_template: "Review {{args}}".into(),
            source_path: fake_path("test"),
        };
        assert_eq!(skill.expand("my-file.rs"), "Review my-file.rs");
    }

    #[test]
    fn expand_empty_args_leaves_placeholder_in_place() {
        let skill = SkillCommand {
            name: "test".into(),
            description: "test".into(),
            aliases: vec![],
            argument_hint: "".into(),
            allowed_tools: None,
            prompt_template: "Do the thing with $ARGUMENTS".into(),
            source_path: fake_path("test"),
        };
        assert_eq!(skill.expand(""), "Do the thing with ");
    }

    // ── alias resolution ──────────────────────────────────────────────────

    #[test]
    fn aliases_parsed_from_frontmatter() {
        let content = "---\nname: review\naliases: [rev, r]\n---\nReview $ARGUMENTS\n";
        let skill = SkillCommandLoader::parse_content(&fake_path("review"), content).unwrap();
        assert_eq!(skill.aliases, vec!["rev", "r"]);
    }

    #[test]
    fn single_alias_without_brackets() {
        let content = "---\nname: check\naliases: chk\n---\nCheck $ARGUMENTS\n";
        let skill = SkillCommandLoader::parse_content(&fake_path("check"), content).unwrap();
        assert_eq!(skill.aliases, vec!["chk"]);
    }

    // ── name sanitization ─────────────────────────────────────────────────

    #[test]
    fn name_with_spaces_becomes_hyphenated() {
        let content = "---\nname: my skill\n---\nDo stuff\n";
        let skill = SkillCommandLoader::parse_content(&fake_path("my-skill"), content).unwrap();
        assert_eq!(skill.name, "my-skill");
    }

    // ── load_dir ──────────────────────────────────────────────────────────

    #[test]
    fn load_dir_returns_empty_for_nonexistent_directory() {
        let skills = SkillCommandLoader::load_dir(Path::new("/nonexistent/path/xyz"));
        assert!(skills.is_empty());
    }

    #[test]
    fn load_dir_sorts_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zzz.md"), "Do ZZZ with $ARGUMENTS").unwrap();
        std::fs::write(dir.path().join("aaa.md"), "Do AAA with $ARGUMENTS").unwrap();
        std::fs::write(dir.path().join("mmm.md"), "Do MMM with $ARGUMENTS").unwrap();
        let skills = SkillCommandLoader::load_dir(dir.path());
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    // ── split_frontmatter ─────────────────────────────────────────────────

    #[test]
    fn split_frontmatter_parses_standard_delimiters() {
        let content = "---\nkey: value\n---\nbody text\n";
        let (fm, body) = split_frontmatter(content);
        assert_eq!(fm, Some("key: value"));
        assert!(body.contains("body text"));
    }

    #[test]
    fn split_frontmatter_returns_none_when_no_delimiter() {
        let content = "just body\nno front matter";
        let (fm, body) = split_frontmatter(content);
        assert!(fm.is_none());
        assert!(body.contains("just body"));
    }
}
