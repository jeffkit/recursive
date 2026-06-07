//! Custom agent definitions loaded from `.recursive/agents/**/*.md` files.
//!
//! Each markdown file has optional YAML frontmatter delimited by `---` at the
//! start of the file.  The frontmatter declares:
//!
//! | Field            | Required | Description                              |
//! |------------------|----------|------------------------------------------|
//! | `name`           | yes      | Unique agent name (must match directory) |
//! | `system_prompt`  | yes      | System prompt for the agent              |
//! | `allowed_tools`  | no       | Array of tool names; empty = read-only   |
//!
//! After the closing `---`, the remaining markdown body is treated as
//! human-readable documentation and is ignored by the runtime.
//!
//! Example (`code-reviewer.md`):
//!
//! ```markdown
//! ---
//! name: code-reviewer
//! system_prompt: |
//!   You are an expert code reviewer.  Examine diffs and flag:
//!   - correctness issues
//!   - performance concerns
//!   - security vulnerabilities
//! allowed_tools:
//!   - Read
//!   - Glob
//!   - Grep
//! ---
//!
//! # Code Reviewer
//! Reviews pull requests for quality and security.
//! ```
//!
//! # Resolution in `AgentTool`
//!
//! When a `manifest` entry includes a `definition` field (instead of — or
//! in addition to — `system_prompt`), the definition is resolved from the
//! loaded registry and merged with any inline overrides.  This lets
//! callers write:
//!
//! ```json
//! {
//!   "manifest": {
//!     "reviewer": { "definition": "code-reviewer" }
//!   }
//! }
//! ```

use crate::error::{Error, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// AgentDefinition
// ---------------------------------------------------------------------------

/// A single agent definition parsed from a `.recursive/agents/*.md` file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinition {
    /// Unique name for this agent (must match the sub-directory name).
    pub name: String,

    /// The system prompt that defines the agent's role and behaviour.
    pub system_prompt: String,

    /// Optional tool allowlist.  Empty = default read-only set.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

/// Used internally for the top-level YAML parsing so `serde_yml` can
/// give us a flat `AgentDefinition` without requiring a nested key.
#[derive(Debug, Clone, Deserialize)]
struct AgentDefinitionRaw {
    name: String,
    system_prompt: String,
    #[serde(default)]
    allowed_tools: Vec<String>,
}

impl From<AgentDefinitionRaw> for AgentDefinition {
    fn from(raw: AgentDefinitionRaw) -> Self {
        Self {
            name: raw.name,
            system_prompt: raw.system_prompt,
            allowed_tools: raw.allowed_tools,
        }
    }
}

// ---------------------------------------------------------------------------
// AgentDefinitions — the registry
// ---------------------------------------------------------------------------

/// A read-only registry of named agent definitions loaded from disk.
///
/// Usage:
///
/// ```no_run
/// # use std::path::Path;
/// # use recursive::tools::agent_defs::AgentDefinitions;
/// # fn foo() -> Result<(), Box<dyn std::error::Error>> {
/// let defs = AgentDefinitions::load(Path::new("/workspace"))?;
/// if let Some(d) = defs.get("code-reviewer") {
///     println!("system_prompt = {}", d.system_prompt);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct AgentDefinitions {
    /// Name → definition mapping.
    definitions: HashMap<String, AgentDefinition>,
}

impl AgentDefinitions {
    /// Walk `.recursive/agents/**/*.md`, parse each file, and return a
    /// populated registry.  Returns an empty registry (not an error) when
    /// the directory does not exist — missing dir is a silent no-op.
    pub fn load(workspace_root: &Path) -> Result<Self> {
        let agents_dir = workspace_root.join(".recursive").join("agents");
        if !agents_dir.is_dir() {
            return Ok(Self::default());
        }

        let mut definitions = HashMap::new();

        for entry in walkdir::WalkDir::new(&agents_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            // Only process .md files
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }

            match Self::parse_file(path) {
                Ok(def) => {
                    if let Some(existing) = definitions.insert(def.name.clone(), def.clone()) {
                        // Duplicate name — keep whichever file sorts first
                        // (walkdir is deterministic), but warn.
                        tracing::warn!(
                            "Duplicate agent name '{}' — using {} (ignoring {})",
                            def.name,
                            path.display(),
                            format!("{}/{}", agents_dir.display(), existing.name)
                        );
                    }
                }
                Err(e) => {
                    // Skip files that fail to parse — this is permissive
                    // by design so one bad file doesn't break everything.
                    tracing::warn!("Skipping agent file {}: {e}", path.display());
                }
            }
        }

        Ok(Self { definitions })
    }

    /// Resolve a named definition.
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.definitions.get(name)
    }

    /// Number of loaded definitions.
    pub fn len(&self) -> usize {
        self.definitions.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }

    /// Iterate over all definitions.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &AgentDefinition)> {
        self.definitions.iter()
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Parse a single `.md` file, extracting YAML frontmatter.
    fn parse_file(path: &Path) -> Result<AgentDefinition> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::Config {
            message: format!("Cannot read agent file {}: {e}", path.display()),
        })?;

        let frontmatter = Self::extract_frontmatter(&raw, path)?;
        let def: AgentDefinitionRaw =
            serde_yml::from_str(&frontmatter).map_err(|e| Error::Config {
                message: format!("Invalid YAML frontmatter in {}: {e}", path.display()),
            })?;

        Ok(def.into())
    }

    /// Extract YAML frontmatter from a markdown string.
    ///
    /// Frontmatter must be delimited by `---` on its own line at the
    /// very start of the file, with a closing `---` on its own line.
    /// Returns the content between the delimiters.
    fn extract_frontmatter(raw: &str, path: &Path) -> Result<String> {
        let trimmed = raw.trim_start();
        if !trimmed.starts_with("---") {
            return Err(Error::Config {
                message: format!(
                    "Agent file {} has no YAML frontmatter (must start with ---)",
                    path.display()
                ),
            });
        }

        // Skip the opening `---`
        let after_open = &trimmed[3..];
        // Find the closing `---` — it must be on a line by itself
        let close_pos = after_open.find("\n---").or_else(|| {
            // Also handle the case where `---` is followed by EOF
            if after_open.starts_with("---") {
                Some(0)
            } else {
                None
            }
        });

        match close_pos {
            Some(pos) => {
                let content = &after_open[..pos].trim_end();
                Ok(content.to_string())
            }
            None => Err(Error::Config {
                message: format!(
                    "Agent file {} has unclosed YAML frontmatter (missing closing ---)",
                    path.display()
                ),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // extract_frontmatter
    // ------------------------------------------------------------------

    #[test]
    fn frontmatter_basic() {
        let raw = "---\nname: test\nsystem_prompt: 'hello'\n---\n\n# Doc\n";
        let front = AgentDefinitions::extract_frontmatter(raw, Path::new("test.md")).unwrap();
        assert_eq!(front.trim(), "name: test\nsystem_prompt: 'hello'");
    }

    #[test]
    fn frontmatter_multiline_prompt() {
        let raw = concat!(
            "---\n",
            "name: reviewer\n",
            "system_prompt: |\n",
            "  Line one.\n",
            "  Line two.\n",
            "allowed_tools:\n",
            "  - Read\n",
            "  - Grep\n",
            "---\n",
            "\n",
            "# Doc\n",
        );
        let front = AgentDefinitions::extract_frontmatter(raw, Path::new("test.md")).unwrap();
        let def: AgentDefinitionRaw = serde_yml::from_str(&front).unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.system_prompt, "Line one.\nLine two.\n");
        assert_eq!(def.allowed_tools, vec!["Read", "Grep"]);
    }

    #[test]
    fn frontmatter_no_closing() {
        let raw = "---\nname: test\nsystem_prompt: 'hi'\n";
        let err = AgentDefinitions::extract_frontmatter(raw, Path::new("test.md")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unclosed"), "got: {msg}");
    }

    #[test]
    fn frontmatter_missing_opening() {
        let raw = "name: test\nsystem_prompt: 'hi'\n---\n";
        let err = AgentDefinitions::extract_frontmatter(raw, Path::new("test.md")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no YAML frontmatter"), "got: {msg}");
    }

    // ------------------------------------------------------------------
    // parse_file
    // ------------------------------------------------------------------

    #[test]
    fn parse_full_file() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path().join("test.md");
        std::fs::write(
            &md,
            concat!(
                "---\n",
                "name: helper\n",
                "system_prompt: 'You are helpful.'\n",
                "allowed_tools:\n",
                "  - Read\n",
                "---\n",
                "\n",
                "# My Agent\n",
                "Extra docs go here.\n",
            ),
        )
        .unwrap();

        let def = AgentDefinitions::parse_file(&md).unwrap();
        assert_eq!(def.name, "helper");
        assert_eq!(def.system_prompt, "You are helpful.");
        assert_eq!(def.allowed_tools, vec!["Read"]);
    }

    #[test]
    fn parse_file_no_frontmatter_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path().join("test.md");
        std::fs::write(&md, "# Just docs\nNo frontmatter.\n").unwrap();

        let err = AgentDefinitions::parse_file(&md).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no YAML frontmatter"), "got: {msg}");
    }

    // ------------------------------------------------------------------
    // load (directory walking)
    // ------------------------------------------------------------------

    #[test]
    fn load_empty_when_no_agents_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // No .recursive/agents dir at all
        let defs = AgentDefinitions::load(tmp.path()).unwrap();
        assert!(defs.is_empty());
    }

    #[test]
    fn load_skips_non_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".recursive").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        // Write a .txt file (should be skipped)
        std::fs::write(agents_dir.join("notes.txt"), "junk").unwrap();

        // Write a valid .md file
        std::fs::write(
            agents_dir.join("helper.md"),
            "---\nname: helper\nsystem_prompt: 'You help.'\n---\n",
        )
        .unwrap();

        let defs = AgentDefinitions::load(tmp.path()).unwrap();
        assert_eq!(defs.len(), 1);
        assert!(defs.get("helper").is_some());
    }

    #[test]
    fn load_multiple_agents_with_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".recursive").join("agents");
        std::fs::create_dir_all(agents_dir.join("sub")).unwrap();

        std::fs::write(
            agents_dir.join("reviewer.md"),
            "---\nname: reviewer\nsystem_prompt: 'Review code.'\n---\n",
        )
        .unwrap();

        std::fs::write(
            agents_dir.join("sub").join("tester.md"),
            "---\nname: tester\nsystem_prompt: 'Run tests.'\nallowed_tools:\n  - Read\n---\n",
        )
        .unwrap();

        let defs = AgentDefinitions::load(tmp.path()).unwrap();
        assert_eq!(defs.len(), 2);

        let reviewer = defs.get("reviewer").unwrap();
        assert_eq!(reviewer.system_prompt, "Review code.");
        assert!(reviewer.allowed_tools.is_empty());

        let tester = defs.get("tester").unwrap();
        assert_eq!(tester.system_prompt, "Run tests.");
        assert_eq!(tester.allowed_tools, vec!["Read"]);
    }

    #[test]
    fn load_handles_duplicate_names() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".recursive").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        std::fs::write(
            agents_dir.join("a.md"),
            "---\nname: dup\nsystem_prompt: 'First.'\n---\n",
        )
        .unwrap();
        std::fs::write(
            agents_dir.join("b.md"),
            "---\nname: dup\nsystem_prompt: 'Second.'\n---\n",
        )
        .unwrap();

        let defs = AgentDefinitions::load(tmp.path()).unwrap();
        // Should still have 1 entry (first wins)
        // Actually due to WalkDir ordering, a.md or b.md could be first.
        assert_eq!(defs.len(), 1);
        // We don't assert which one wins since WalkDir order is platform-dependent
    }

    #[test]
    fn load_skips_bad_files_without_crashing() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".recursive").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        // Bad file (no frontmatter)
        std::fs::write(agents_dir.join("bad.md"), "just text\n").unwrap();

        // Good file
        std::fs::write(
            agents_dir.join("good.md"),
            "---\nname: good\nsystem_prompt: 'works'\n---\n",
        )
        .unwrap();

        let defs = AgentDefinitions::load(tmp.path()).unwrap();
        assert_eq!(defs.len(), 1);
        assert!(defs.get("good").is_some());
    }
}
