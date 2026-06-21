//! Skill system: file-based capability extension.
//!
//! Skills are markdown files in specific directories that can be loaded
//! on-demand to extend the agent's capabilities.

use std::fs;
use std::path::{Path, PathBuf};

/// Injection mode for a skill.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SkillMode {
    /// Inject into system prompt at session start.
    Always,
    /// Auto-load when trigger words appear in the user goal.
    Trigger,
    /// Agent must explicitly call load_skill (current behavior).
    #[default]
    Manual,
    /// Auto-inject when a tool result references a file path matching one of
    /// the skill's `globs` patterns (Goal 318). Each skill is injected at most
    /// once per agent run, tracked by [`SkillInjector`].
    Globs,
}

/// A discovered skill with metadata.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Human-readable name, e.g., "rust-traits".
    pub name: String,
    /// Brief description for the skill index.
    pub description: String,
    /// Absolute path to the SKILL.md file.
    pub path: PathBuf,
    /// Injection mode.
    pub mode: SkillMode,
    /// Trigger words (only relevant when mode == Trigger).
    pub triggers: Vec<String>,
    /// Hint string for trigger-mode skills (short description for injection).
    /// Auto-generated from description if absent for trigger mode.
    pub hint: String,
    /// Skills that should be auto-loaded before this one.
    pub depends_on: Vec<String>,
    /// Reference documents found in <skill_dir>/refs/
    pub refs: Vec<SkillRef>,
    /// Parameters declared in frontmatter.
    pub params: Vec<SkillParam>,
    /// Executable scripts found in <skill_dir>/scripts/
    pub scripts: Vec<SkillScript>,
    /// Named sections within the skill body (parsed from ## headings).
    pub sections: Vec<SkillSection>,
    /// Glob patterns for `mode: globs` (Goal 318).
    /// E.g. `["src/tools/**", "src/runtime.rs"]`.
    /// `None` when mode is not Globs (or globs list is empty/absent).
    pub globs: Option<Vec<String>>,
}

/// A named section within a skill's body, delimited by `## Section Name`.
#[derive(Debug, Clone)]
pub struct SkillSection {
    /// Section name (the text after `## `, trimmed).
    pub name: String,
    /// Content of the section (everything between this heading and the next
    /// heading of the same or higher level, trimmed).
    pub content: String,
}

/// A reference document within a skill's `refs/` directory.
#[derive(Debug, Clone)]
pub struct SkillRef {
    /// Filename without extension, e.g. "api-spec"
    pub name: String,
    /// Absolute path to the ref file
    pub path: PathBuf,
}

/// A parameter declared in a skill's frontmatter.
#[derive(Debug, Clone)]
pub struct SkillParam {
    /// Parameter name, e.g. "language"
    pub name: String,
    /// Brief description
    pub description: String,
    /// Default value (None if required)
    pub default: Option<String>,
}

/// An executable script within a skill's `scripts/` directory.
#[derive(Debug, Clone)]
pub struct SkillScript {
    /// Script name (filename without extension), e.g. "lint"
    pub name: String,
    /// Absolute path to the script file
    pub path: PathBuf,
    /// Brief description from the first comment line (if present)
    pub description: String,
}

/// Discover skills in the given search paths.
///
/// For each `<path>/<name>/SKILL.md`, parses optional YAML frontmatter.
/// If absent, uses the parent directory name as `name` and the first
/// non-empty line of body as `description`.
///
/// Also scans `<skill_dir>/refs/` for `.md` and `.txt` files and populates
/// `Skill::refs` with what's found. Also scans `<skill_dir>/scripts/` for
/// executable scripts and populates `Skill::scripts`.
pub fn discover_skills(search_paths: &[PathBuf]) -> Vec<Skill> {
    let mut skills = Vec::new();

    for base in search_paths {
        if !base.is_dir() {
            continue;
        }

        if let Ok(entries) = fs::read_dir(base) {
            for entry in entries.flatten() {
                let dir_path = entry.path();
                if !dir_path.is_dir() {
                    continue;
                }

                let skill_file = dir_path.join("SKILL.md");
                if !skill_file.is_file() {
                    continue;
                }

                if let Ok(content) = fs::read_to_string(&skill_file) {
                    let (name, description, mode, triggers, hint, depends_on, params, raw_globs) =
                        parse_skill_meta(&content, &dir_path);
                    let refs = discover_refs(&dir_path);
                    let scripts = discover_scripts(&dir_path);
                    let sections = parse_sections(&content);
                    let globs = if raw_globs.is_empty() {
                        None
                    } else {
                        Some(raw_globs)
                    };
                    skills.push(Skill {
                        name,
                        description,
                        path: skill_file,
                        mode,
                        triggers,
                        hint,
                        depends_on,
                        refs,
                        params,
                        scripts,
                        sections,
                        globs,
                    });
                }
            }
        }
    }

    skills
}

/// Parse named sections from a skill's body content.
///
/// Sections are delimited by `## Section Name` headings (level-2 markdown).
/// The content of each section runs from after the heading line until the
/// next heading of the same or higher level, or end of body.
/// Frontmatter is stripped before parsing.
fn parse_sections(content: &str) -> Vec<SkillSection> {
    let body = extract_body(content);
    let mut sections = Vec::new();
    let mut lines = body.lines().peekable();

    while let Some(line) = lines.next() {
        if let Some(heading) = line.strip_prefix("## ") {
            let name = heading.trim().to_string();
            let mut section_lines = Vec::new();

            // Collect content until next ## heading or end
            while let Some(next) = lines.peek() {
                if next.starts_with("## ") {
                    break;
                }
                #[allow(
                    clippy::unwrap_used,
                    reason = "peeked Some just above in while-let condition"
                )]
                section_lines.push(lines.next().unwrap());
            }

            let content = section_lines.join("\n").trim().to_string();
            sections.push(SkillSection { name, content });
        }
    }

    sections
}

/// Scan `<skill_dir>/refs/` for `.md` and `.txt` files.
fn discover_refs(skill_dir: &Path) -> Vec<SkillRef> {
    let refs_dir = skill_dir.join("refs");
    if !refs_dir.is_dir() {
        return Vec::new();
    }

    let mut refs = Vec::new();
    if let Ok(entries) = fs::read_dir(&refs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if ext == "md" || ext == "txt" {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            refs.push(SkillRef {
                                name: stem.to_string(),
                                path,
                            });
                        }
                    }
                }
            }
        }
    }

    refs
}

/// Scan `<skill_dir>/scripts/` for executable files.
///
/// Accepts files with execute permission (Unix) or common script extensions:
/// `.sh`, `.py`, `.rb`, `.js`. Extracts a description from the first comment
/// line (shebang excluded).
fn discover_scripts(skill_dir: &Path) -> Vec<SkillScript> {
    let scripts_dir = skill_dir.join("scripts");
    if !scripts_dir.is_dir() {
        return Vec::new();
    }

    let mut scripts = Vec::new();
    if let Ok(entries) = fs::read_dir(&scripts_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            // Check if it's executable (Unix) or has a known script extension
            let is_exec = is_executable(&path);
            let has_script_ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| matches!(e, "sh" | "py" | "rb" | "js"))
                .unwrap_or(false);

            if !is_exec && !has_script_ext {
                continue;
            }

            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                let description = extract_script_description(&path);
                scripts.push(SkillScript {
                    name: stem.to_string(),
                    path,
                    description,
                });
            }
        }
    }

    scripts
}

/// Check if a file has execute permission (Unix-only).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

/// Extract a description from the first comment line of a script.
///
/// Reads the first line. If it starts with `#!` (shebang), reads the second
/// line. If a line starts with `#` or `//`, returns it (with the comment
/// prefix stripped). Otherwise returns empty string.
fn extract_script_description(path: &Path) -> String {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let mut lines = content.lines();
    let first = lines.next();

    // Skip shebang line
    let target = match first {
        Some(l) if l.starts_with("#!") => lines.next(),
        other => other,
    };

    match target {
        Some(l) if l.starts_with("# ") || l.starts_with("#") => l
            .trim_start_matches("# ")
            .trim_start_matches('#')
            .trim()
            .to_string(),
        Some(l) if l.starts_with("// ") || l.starts_with("//") => l
            .trim_start_matches("// ")
            .trim_start_matches("//")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// Parse YAML frontmatter (if present) from skill content.
///
/// Returns (name, description, mode, triggers, hint, params). If frontmatter is
/// absent, falls back to using the parent directory name and first non-empty
/// Returns (name, description, mode, triggers, hint, depends_on, params). If frontmatter is
/// absent, falls back to using the parent directory name and first non-empty
/// line, with default mode (Manual), empty triggers, empty hint, and empty depends_on.
/// Returns `(name, description, mode, triggers, hint, depends_on, params, globs)`.
#[allow(clippy::type_complexity)]
pub fn parse_skill_meta(
    content: &str,
    dir_path: &Path,
) -> (
    String,
    String,
    SkillMode,
    Vec<String>,
    String,
    Vec<String>,
    Vec<SkillParam>,
    Vec<String>,
) {
    // Try to extract YAML frontmatter: --- ... ---
    if let Some(frontmatter) = content.strip_prefix("---") {
        if let Some(end) = frontmatter.find("---") {
            let yaml = &frontmatter[..end];
            let body = frontmatter[end + 3..].trim();

            // Parse naive key: value pairs
            let mut name = None;
            let mut description = None;
            let mut mode = SkillMode::Manual;
            let mut triggers = Vec::new();
            let mut hint = String::new();
            let mut params = Vec::new();
            let mut depends_on = Vec::new();
            let mut globs: Vec<String> = Vec::new();

            let lines: Vec<&str> = yaml.lines().collect();
            let mut i = 0;
            while i < lines.len() {
                let line = lines[i].trim();
                if let Some(stripped) = line.strip_prefix("name:") {
                    name = Some(stripped.trim().to_string());
                } else if let Some(stripped) = line.strip_prefix("description:") {
                    description = Some(stripped.trim().to_string());
                } else if let Some(stripped) = line.strip_prefix("hint:") {
                    hint = stripped.trim().to_string();
                } else if let Some(stripped) = line.strip_prefix("mode:") {
                    let raw = stripped.trim().to_lowercase();
                    mode = match raw.as_str() {
                        "always" => SkillMode::Always,
                        "trigger" => SkillMode::Trigger,
                        "globs" => SkillMode::Globs,
                        _ => SkillMode::Manual,
                    };
                } else if line == "globs:" {
                    // Parse YAML list: each entry is "  - pattern"
                    i += 1;
                    while i < lines.len() {
                        let entry = lines[i].trim();
                        if let Some(pat) = entry.strip_prefix("- ") {
                            let p = pat.trim().trim_matches('"').trim_matches('\'');
                            if !p.is_empty() {
                                globs.push(p.to_string());
                            }
                            i += 1;
                        } else if entry.is_empty() {
                            i += 1;
                        } else {
                            // End of list — don't advance, let outer loop re-read
                            break;
                        }
                    }
                    continue;
                } else if let Some(stripped) = line.strip_prefix("triggers:") {
                    // Parse comma-separated trigger words
                    triggers = stripped
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                } else if let Some(stripped) = line.strip_prefix("depends_on:") {
                    // Parse comma-separated dependency names
                    depends_on = stripped
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                } else if line == "params:" {
                    // Parse params list: each entry starts with "  - name: xxx"
                    i += 1;
                    while i < lines.len() {
                        let entry_line = lines[i];
                        // Check if this line starts a new param entry: "  - name: xxx"
                        if let Some(after_dash) = entry_line.trim().strip_prefix("- name:") {
                            let param_name = after_dash.trim().to_string();
                            let mut param_description = String::new();
                            let mut param_default = None;

                            // Look at subsequent lines for description and default
                            i += 1;
                            while i < lines.len() {
                                let sub_line = lines[i];
                                let trimmed = sub_line.trim();
                                // Stop if we hit a new top-level key (no indent) or a new list entry
                                if !sub_line.starts_with(' ') && !sub_line.starts_with('\t') {
                                    break;
                                }
                                if let Some(val) = trimmed.strip_prefix("description:") {
                                    param_description = val.trim().to_string();
                                } else if let Some(val) = trimmed.strip_prefix("default:") {
                                    param_default = Some(val.trim().to_string());
                                } else if trimmed.starts_with("- name:") {
                                    // Next param entry — back up so outer loop re-processes
                                    i -= 1;
                                    break;
                                } else if !trimmed.is_empty() && !trimmed.starts_with('-') {
                                    // Unknown indented line — skip
                                }
                                i += 1;
                            }

                            params.push(SkillParam {
                                name: param_name,
                                description: param_description,
                                default: param_default,
                            });
                        } else {
                            // Not a param entry — skip to next line
                            i += 1;
                        }
                    }
                    // After params block, break out of top-level loop
                    break;
                }
                i += 1;
            }

            // Use frontmatter values if present, otherwise fall back to defaults
            let final_name = name.unwrap_or_else(|| {
                dir_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unnamed")
                    .to_string()
            });

            let final_description = description.unwrap_or_else(|| {
                body.lines()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
                    .unwrap_or_default()
            });

            // Auto-generate hint for trigger-mode skills if not explicitly set
            if hint.is_empty() && mode == SkillMode::Trigger {
                hint = format!("{}: {}", final_name, final_description);
            }

            let final_globs = if globs.is_empty() { None } else { Some(globs) };
            // Normalise: Globs mode with no patterns → Manual
            let final_mode = if mode == SkillMode::Globs && final_globs.is_none() {
                SkillMode::Manual
            } else {
                mode
            };
            return (
                final_name,
                final_description,
                final_mode,
                triggers,
                hint,
                depends_on,
                params,
                final_globs.unwrap_or_default(),
            );
        }
    }

    // No frontmatter - use directory name and first non-empty line
    let name = dir_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed")
        .to_string();

    let description = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .unwrap_or_default();

    (
        name,
        description,
        SkillMode::Manual,
        Vec::new(),
        String::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

/// Select skills whose mode is `Always`, plus any `Trigger` skills whose
/// triggers match the given goal text.
///
/// Returns `Vec<(name, body_or_hint)>` of matching skills.
/// For `Always` mode, the full body is returned.
/// For `Trigger` mode, the hint is returned (short description).
pub fn skills_for_injection(skills: &[Skill], goal: &str) -> Vec<(String, String)> {
    let mut result: Vec<(String, String)> = Vec::new();

    for skill in skills {
        match skill.mode {
            SkillMode::Always => {
                // Read the body from the SKILL.md file
                if let Ok(content) = fs::read_to_string(&skill.path) {
                    let body = extract_body(&content);
                    result.push((skill.name.clone(), body.to_string()));
                }
            }
            SkillMode::Trigger => {
                // Check if any trigger matches the goal (case-insensitive)
                let goal_lower = goal.to_lowercase();
                let matched = skill
                    .triggers
                    .iter()
                    .any(|t| goal_lower.contains(&t.to_lowercase()));
                if matched {
                    // Inject the hint (short description) instead of full body
                    result.push((skill.name.clone(), skill.hint.clone()));
                }
            }
            SkillMode::Manual | SkillMode::Globs => {
                // Manual: never auto-injected; agent must call load_skill.
                // Globs: injected by SkillInjector after matching tool results, not here.
            }
        }
    }

    result
}

/// Extract the body of a SKILL.md file, stripping YAML frontmatter if present.
pub fn extract_skill_body(content: &str) -> &str {
    extract_body(content)
}

fn extract_body(content: &str) -> &str {
    if let Some(frontmatter) = content.strip_prefix("---") {
        if let Some(end) = frontmatter.find("---") {
            return frontmatter[end + 3..].trim();
        }
    }
    content.trim()
}

/// Render a compact "available skills" block for the system prompt.
///
/// Returns empty string if no skills found.
pub fn skill_index(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "".to_string(),
        "Available skills (use `load_skill` to activate):".to_string(),
    ];

    for skill in skills {
        let mut suffix_parts = Vec::new();

        // Mode tag
        let mode_tag = match skill.mode {
            SkillMode::Always => "[always]",
            SkillMode::Trigger => "[trigger]",
            SkillMode::Globs => "[globs]",
            SkillMode::Manual => "",
        };

        // Ref count
        let ref_count = skill.refs.len();
        if ref_count > 0 {
            suffix_parts.push(format!("{ref_count} refs"));
        }

        // Params
        if !skill.params.is_empty() {
            let param_strs: Vec<String> = skill
                .params
                .iter()
                .map(|p| {
                    if let Some(ref default) = p.default {
                        format!("{}={}", p.name, default)
                    } else {
                        p.name.clone()
                    }
                })
                .collect();
            suffix_parts.push(format!("params: {}", param_strs.join(", ")));
        }

        // Sections
        if !skill.sections.is_empty() {
            let section_names: Vec<&str> = skill.sections.iter().map(|s| s.name.as_str()).collect();
            suffix_parts.push(format!("sections: {}", section_names.join(", ")));
        }

        // Depends on
        if !skill.depends_on.is_empty() {
            suffix_parts.push(format!("depends_on: {}", skill.depends_on.join(", ")));
        }

        // Scripts
        let script_names: Vec<&str> = skill.scripts.iter().map(|s| s.name.as_str()).collect();

        let prefix = if mode_tag.is_empty() {
            format!("- {}: {}", skill.name, skill.description)
        } else {
            format!("- {} {}: {}", mode_tag, skill.name, skill.description)
        };

        if suffix_parts.is_empty() {
            lines.push(prefix);
        } else {
            lines.push(format!("{} ({})", prefix, suffix_parts.join(", ")));
        }

        // Append scripts suffix if present (after the main line)
        if !script_names.is_empty() {
            #[allow(
                clippy::unwrap_used,
                reason = "lines is non-empty: prior push guarantees at least one element"
            )]
            let last = lines.last_mut().unwrap();
            *last = format!("{} [scripts: {}]", last, script_names.join(", "));
        }
    }

    lines.push("".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn discover_skills_parses_frontmatter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create skill with YAML frontmatter
        let rust_dir = base.join("rust-traits");
        fs::create_dir(&rust_dir).unwrap();
        let mut file = fs::File::create(rust_dir.join("SKILL.md")).unwrap();
        writeln!(
            file,
            "---\
             \nname: rust-traits\
             \ndescription: Explain Rust trait design patterns\
             \n---\
             \n\nWhen asked about Rust traits, walk the codebase..."
        )
        .unwrap();

        // Create skill without frontmatter (name from dir, desc from body)
        let python_dir = base.join("python-api");
        fs::create_dir(&python_dir).unwrap();
        let mut file = fs::File::create(python_dir.join("SKILL.md")).unwrap();
        writeln!(file, "First line of description\n\nMore content...").unwrap();

        let skills = discover_skills(&[base.to_path_buf()]);
        assert_eq!(skills.len(), 2);

        let rust = skills.iter().find(|s| s.name == "rust-traits").unwrap();
        assert_eq!(rust.description, "Explain Rust trait design patterns");

        let python = skills.iter().find(|s| s.name == "python-api").unwrap();
        assert_eq!(python.description, "First line of description");
    }

    #[test]
    fn skill_index_empty_for_no_skills() {
        let result = skill_index(&[]);
        assert_eq!(result, "");
    }

    #[test]
    fn skill_index_renders_correctly() {
        let skills = vec![
            Skill {
                name: "rust-traits".to_string(),
                description: "Explain Rust trait design".to_string(),
                path: PathBuf::from("/tmp/skills/rust-traits/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
            Skill {
                name: "python-api".to_string(),
                description: "Python API patterns".to_string(),
                path: PathBuf::from("/tmp/skills/python-api/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
        ];

        let result = skill_index(&skills);
        assert!(result.contains("Available skills"));
        assert!(result.contains("- rust-traits: Explain Rust trait design"));
        assert!(result.contains("- python-api: Python API patterns"));
    }

    #[test]
    fn discover_skills_populates_refs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a skill with refs
        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A skill with refs\n---\n\nBody",
        )
        .unwrap();

        // Create refs directory with some files
        let refs_dir = skill_dir.join("refs");
        fs::create_dir(&refs_dir).unwrap();
        fs::write(refs_dir.join("api-spec.md"), "# API Spec\n\nDetails here.").unwrap();
        fs::write(refs_dir.join("examples.txt"), "Example 1\nExample 2").unwrap();
        // Non-matching extension should be ignored
        fs::write(refs_dir.join("notes.json"), "{}").unwrap();

        let skills = discover_skills(&[base.to_path_buf()]);
        assert_eq!(skills.len(), 1);

        let skill = &skills[0];
        assert_eq!(skill.name, "my-skill");
        assert_eq!(
            skill.refs.len(),
            2,
            "should find 2 ref files (md + txt), ignoring json"
        );

        let api_spec = skill.refs.iter().find(|r| r.name == "api-spec").unwrap();
        assert!(api_spec.path.ends_with("api-spec.md"));

        let examples = skill.refs.iter().find(|r| r.name == "examples").unwrap();
        assert!(examples.path.ends_with("examples.txt"));
    }

    #[test]
    fn discover_skills_handles_no_refs_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a skill without refs directory
        let skill_dir = base.join("simple-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: simple-skill\ndescription: No refs\n---\n\nBody",
        )
        .unwrap();

        let skills = discover_skills(&[base.to_path_buf()]);
        assert_eq!(skills.len(), 1);
        assert!(
            skills[0].refs.is_empty(),
            "skill with no refs/ dir should have empty refs"
        );
    }

    #[test]
    fn skill_index_shows_ref_count() {
        let skills = vec![
            Skill {
                name: "with-refs".to_string(),
                description: "Has references".to_string(),
                path: PathBuf::from("/tmp/skills/with-refs/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![
                    SkillRef {
                        name: "api-spec".to_string(),
                        path: PathBuf::from("/tmp/skills/with-refs/refs/api-spec.md"),
                    },
                    SkillRef {
                        name: "examples".to_string(),
                        path: PathBuf::from("/tmp/skills/with-refs/refs/examples.txt"),
                    },
                ],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
            Skill {
                name: "no-refs".to_string(),
                description: "No references".to_string(),
                path: PathBuf::from("/tmp/skills/no-refs/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
        ];

        let result = skill_index(&skills);
        assert!(result.contains("- with-refs: Has references (2 refs)"));
        assert!(result.contains("- no-refs: No references"));
        assert!(
            !result.contains("(0 refs)"),
            "should not show count for 0 refs"
        );
    }

    #[test]
    fn discover_skills_parses_params() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("code-review");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
             name: code-review\n\
             description: Review code for quality issues\n\
             params:\n\
             \x20\x20- name: language\n\
             \x20\x20  description: Target language\n\
             \x20\x20  default: rust\n\
             \x20\x20- name: strict\n\
             \x20\x20  description: Enable strict mode\n\
             ---\n\
             \n\
             Review {{language}} code {{#if strict}}strictly{{/if}}.",
        )
        .unwrap();

        let skills = discover_skills(&[base.to_path_buf()]);
        assert_eq!(skills.len(), 1);

        let skill = &skills[0];
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.params.len(), 2);

        let lang = skill.params.iter().find(|p| p.name == "language").unwrap();
        assert_eq!(lang.description, "Target language");
        assert_eq!(lang.default.as_deref(), Some("rust"));

        let strict = skill.params.iter().find(|p| p.name == "strict").unwrap();
        assert_eq!(strict.description, "Enable strict mode");
        assert_eq!(strict.default, None);
    }

    #[test]
    fn discover_skills_handles_no_params() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("simple");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: simple\ndescription: No params\n---\n\nBody",
        )
        .unwrap();

        let skills = discover_skills(&[base.to_path_buf()]);
        assert_eq!(skills.len(), 1);
        assert!(
            skills[0].params.is_empty(),
            "skill without params should have empty params"
        );
    }

    #[test]
    fn skill_index_shows_params() {
        let skills = vec![
            Skill {
                name: "code-review".to_string(),
                description: "Review code".to_string(),
                path: PathBuf::from("/tmp/skills/code-review/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![
                    SkillParam {
                        name: "language".to_string(),
                        description: "Target language".to_string(),
                        default: Some("rust".to_string()),
                    },
                    SkillParam {
                        name: "strict".to_string(),
                        description: "Enable strict mode".to_string(),
                        default: None,
                    },
                ],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
            Skill {
                name: "simple".to_string(),
                description: "No params".to_string(),
                path: PathBuf::from("/tmp/skills/simple/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
        ];

        let result = skill_index(&skills);
        assert!(
            result.contains("params: language=rust, strict"),
            "should show params with defaults: {result}"
        );
        assert!(
            result.contains("- simple: No params"),
            "should show skill without params normally"
        );
    }

    #[test]
    fn discover_skills_populates_scripts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a skill with scripts
        let skill_dir = base.join("my-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A skill with scripts\n---\n\nBody",
        )
        .unwrap();

        // Create scripts directory
        let scripts_dir = skill_dir.join("scripts");
        fs::create_dir(&scripts_dir).unwrap();
        fs::write(
            scripts_dir.join("lint.sh"),
            "#!/bin/sh\n# Run the linter\necho 'linting...'\n",
        )
        .unwrap();
        fs::write(
            scripts_dir.join("format.py"),
            "#!/usr/bin/env python3\n# Format the code\nprint('formatting')\n",
        )
        .unwrap();
        // Non-script extension should be ignored
        fs::write(scripts_dir.join("notes.txt"), "not a script").unwrap();

        let skills = discover_skills(&[base.to_path_buf()]);
        assert_eq!(skills.len(), 1);

        let skill = &skills[0];
        assert_eq!(skill.name, "my-skill");
        assert_eq!(
            skill.scripts.len(),
            2,
            "should find 2 scripts (sh + py), ignoring txt"
        );

        let lint = skill.scripts.iter().find(|s| s.name == "lint").unwrap();
        assert!(lint.path.ends_with("lint.sh"));
        assert_eq!(lint.description, "Run the linter");

        let format = skill.scripts.iter().find(|s| s.name == "format").unwrap();
        assert!(format.path.ends_with("format.py"));
        assert_eq!(format.description, "Format the code");
    }

    #[test]
    fn discover_skills_handles_no_scripts_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        // Create a skill without scripts directory
        let skill_dir = base.join("simple-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: simple-skill\ndescription: No scripts\n---\n\nBody",
        )
        .unwrap();

        let skills = discover_skills(&[base.to_path_buf()]);
        assert_eq!(skills.len(), 1);
        assert!(
            skills[0].scripts.is_empty(),
            "skill with no scripts/ dir should have empty scripts"
        );
    }

    #[test]
    fn skill_index_shows_script_names() {
        let skills = vec![
            Skill {
                name: "with-scripts".to_string(),
                description: "Has scripts".to_string(),
                path: PathBuf::from("/tmp/skills/with-scripts/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![
                    SkillScript {
                        name: "lint".to_string(),
                        path: PathBuf::from("/tmp/skills/with-scripts/scripts/lint.sh"),
                        description: "Run the linter".to_string(),
                    },
                    SkillScript {
                        name: "format".to_string(),
                        path: PathBuf::from("/tmp/skills/with-scripts/scripts/format.py"),
                        description: "Format the code".to_string(),
                    },
                ],
                sections: vec![],
                globs: None,
            },
            Skill {
                name: "no-scripts".to_string(),
                description: "No scripts".to_string(),
                path: PathBuf::from("/tmp/skills/no-scripts/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
        ];

        let result = skill_index(&skills);
        assert!(result.contains("- with-scripts: Has scripts [scripts: lint, format]"));
        assert!(result.contains("- no-scripts: No scripts"));
    }

    #[test]
    fn extract_script_description_shebang_then_comment() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.sh");
        fs::write(&path, "#!/bin/sh\n# Run the linter\necho hi\n").unwrap();
        assert_eq!(extract_script_description(&path), "Run the linter");
    }

    #[test]
    fn extract_script_description_no_shebang() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.py");
        fs::write(&path, "# Format the code\nprint('hi')\n").unwrap();
        assert_eq!(extract_script_description(&path), "Format the code");
    }

    #[test]
    fn extract_script_description_js_comment() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.js");
        fs::write(&path, "// Run the build\nconsole.log('hi')\n").unwrap();
        assert_eq!(extract_script_description(&path), "Run the build");
    }

    #[test]
    fn extract_script_description_empty_when_no_comment() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.sh");
        fs::write(&path, "#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(extract_script_description(&path), "");
    }

    // --- Mode & trigger tests ---

    #[test]
    fn parse_skill_meta_parses_mode_always() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("test-skill");
        fs::create_dir(&dir).unwrap();
        let content = "---\nname: test-skill\ndescription: A test\nmode: always\n---\n\nBody text";
        let (name, desc, mode, triggers, hint, _depends_on, params, _globs) =
            parse_skill_meta(content, &dir);
        assert_eq!(name, "test-skill");
        assert_eq!(desc, "A test");
        assert_eq!(mode, SkillMode::Always);
        assert!(triggers.is_empty());
        assert!(hint.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn parse_skill_meta_parses_mode_trigger_with_triggers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("test-skill");
        fs::create_dir(&dir).unwrap();
        let content =
            "---\nname: test-skill\ndescription: A test\nmode: trigger\ntriggers: rust, trait\n---\n\nBody text";
        let (name, desc, mode, triggers, hint, _depends_on, params, _globs) =
            parse_skill_meta(content, &dir);
        assert_eq!(name, "test-skill");
        assert_eq!(desc, "A test");
        assert_eq!(mode, SkillMode::Trigger);
        assert_eq!(triggers, vec!["rust", "trait"]);
        // Hint should be auto-generated for trigger mode
        assert_eq!(hint, "test-skill: A test");
        assert!(params.is_empty());
    }

    #[test]
    fn parse_skill_meta_parses_explicit_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("test-skill");
        fs::create_dir(&dir).unwrap();
        let content =
            "---\nname: test-skill\ndescription: A test\nmode: trigger\ntriggers: rust\nhint: Rust-related helper\n---\n\nBody text";
        let (name, desc, mode, triggers, hint, _depends_on, params, _globs) =
            parse_skill_meta(content, &dir);
        assert_eq!(name, "test-skill");
        assert_eq!(desc, "A test");
        assert_eq!(mode, SkillMode::Trigger);
        assert_eq!(triggers, vec!["rust"]);
        assert_eq!(hint, "Rust-related helper");
        assert!(params.is_empty());
    }

    #[test]
    fn parse_skill_meta_defaults_to_manual() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("test-skill");
        fs::create_dir(&dir).unwrap();
        let content = "---\nname: test-skill\ndescription: A test\n---\n\nBody text";
        let (_, _, mode, triggers, hint, _, _, _) = parse_skill_meta(content, &dir);
        assert_eq!(mode, SkillMode::Manual);
        assert!(triggers.is_empty());
        assert!(hint.is_empty());
    }

    #[test]
    fn parse_skill_meta_no_frontmatter_defaults_manual() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("test-skill");
        fs::create_dir(&dir).unwrap();
        let content = "Body text";
        let (_, _, mode, triggers, hint, _, _, _) = parse_skill_meta(content, &dir);
        assert_eq!(mode, SkillMode::Manual);
        assert!(triggers.is_empty());
        assert!(hint.is_empty());
    }

    #[test]
    fn skills_for_injection_always_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("always-skill");
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: always-skill\ndescription: Always injected\nmode: always\n---\n\nAlways body content",
        )
        .unwrap();

        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        let result = skills_for_injection(&skills, "anything");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "always-skill");
        assert_eq!(result[0].1, "Always body content");
    }

    #[test]
    fn skills_for_injection_trigger_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("rust-skill");
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: rust-skill\ndescription: Rust helper\nmode: trigger\ntriggers: rust, trait\n---\n\nRust body content",
        )
        .unwrap();

        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        let result = skills_for_injection(&skills, "I need help with Rust traits");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "rust-skill");
        // Trigger mode should return hint, not full body
        assert_eq!(result[0].1, "rust-skill: Rust helper");
    }

    #[test]
    fn skills_for_injection_trigger_no_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("rust-skill");
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: rust-skill\ndescription: Rust helper\nmode: trigger\ntriggers: rust, trait\n---\n\nRust body content",
        )
        .unwrap();

        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        let result = skills_for_injection(&skills, "I need help with Python");
        assert!(result.is_empty());
    }

    #[test]
    fn skills_for_injection_manual_never_injected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("manual-skill");
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: manual-skill\ndescription: Manual only\n---\n\nManual body",
        )
        .unwrap();

        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        let result = skills_for_injection(&skills, "anything");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_skill_meta_parses_globs_mode() {
        let content =
            "---\nname: arch-sync\ndescription: sync docs\nmode: globs\nglobs:\n  - src/tools/**\n  - src/runtime.rs\n---\n\nBody";
        let dir = std::path::PathBuf::from("/tmp/skills/arch-sync");
        let (name, _, mode, _, _, _, _, globs) = parse_skill_meta(content, &dir);
        assert_eq!(name, "arch-sync");
        assert_eq!(mode, SkillMode::Globs);
        assert_eq!(globs, vec!["src/tools/**", "src/runtime.rs"]);
    }

    #[test]
    fn skills_for_injection_globs_not_injected_at_session_start() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("arch-sync");
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: arch-sync\ndescription: sync docs\nmode: globs\nglobs:\n  - src/tools/**\n---\n\nBody",
        )
        .unwrap();

        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        // Globs-mode skills must NOT be injected at session start
        let result = skills_for_injection(&skills, "anything src/tools/fs.rs");
        assert!(
            result.is_empty(),
            "globs skill should not inject at session start"
        );
    }

    #[test]
    fn skill_index_shows_mode_tags() {
        let skills = vec![
            Skill {
                name: "always-skill".to_string(),
                description: "Always injected".to_string(),
                path: PathBuf::from("/tmp/skills/always-skill/SKILL.md"),
                mode: SkillMode::Always,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
            Skill {
                name: "trigger-skill".to_string(),
                description: "Trigger based".to_string(),
                path: PathBuf::from("/tmp/skills/trigger-skill/SKILL.md"),
                mode: SkillMode::Trigger,
                triggers: vec!["rust".to_string()],
                hint: "trigger-skill: Trigger based".to_string(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
            Skill {
                name: "manual-skill".to_string(),
                description: "Manual only".to_string(),
                path: PathBuf::from("/tmp/skills/manual-skill/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
        ];

        let result = skill_index(&skills);
        assert!(result.contains("[always] always-skill"));
        assert!(result.contains("[trigger] trigger-skill"));
        assert!(result.contains("- manual-skill: Manual only"));
    }

    // --- Section parsing tests ---

    #[test]
    fn parse_sections_extracts_named_sections() {
        let content = "---\nname: test\ndescription: Test\n---\n\n## Overview\n\nGeneral info here.\n\n## Details\n\nMore specific content.\n\n## Examples\n\nExample 1\nExample 2";
        let sections = parse_sections(content);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].name, "Overview");
        assert_eq!(sections[0].content, "General info here.");
        assert_eq!(sections[1].name, "Details");
        assert_eq!(sections[1].content, "More specific content.");
        assert_eq!(sections[2].name, "Examples");
        assert_eq!(sections[2].content, "Example 1\nExample 2");
    }

    #[test]
    fn parse_sections_returns_empty_for_no_headings() {
        let content = "---\nname: test\ndescription: Test\n---\n\nJust a body with no sections.";
        let sections = parse_sections(content);
        assert!(sections.is_empty());
    }

    #[test]
    fn parse_sections_handles_no_frontmatter() {
        let content = "## Intro\n\nHello world\n\n## Conclusion\n\nBye";
        let sections = parse_sections(content);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].name, "Intro");
        assert_eq!(sections[0].content, "Hello world");
        assert_eq!(sections[1].name, "Conclusion");
        assert_eq!(sections[1].content, "Bye");
    }

    #[test]
    fn skill_index_shows_sections() {
        let skills = vec![
            Skill {
                name: "with-sections".to_string(),
                description: "Has sections".to_string(),
                path: PathBuf::from("/tmp/skills/with-sections/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![
                    SkillSection {
                        name: "Overview".to_string(),
                        content: "General info".to_string(),
                    },
                    SkillSection {
                        name: "Details".to_string(),
                        content: "Specific info".to_string(),
                    },
                ],
                globs: None,
            },
            Skill {
                name: "no-sections".to_string(),
                description: "No sections".to_string(),
                path: PathBuf::from("/tmp/skills/no-sections/SKILL.md"),
                mode: SkillMode::Manual,
                triggers: vec![],
                hint: String::new(),
                depends_on: vec![],
                refs: vec![],
                params: vec![],
                scripts: vec![],
                sections: vec![],
                globs: None,
            },
        ];

        let result = skill_index(&skills);
        assert!(result.contains("sections: Overview, Details"));
        assert!(result.contains("- no-sections: No sections"));
    }

    #[test]
    fn discover_skills_populates_sections() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path();

        let skill_dir = base.join("sectioned-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: sectioned-skill\ndescription: A skill with sections\n---\n\n## Setup\n\nInstallation steps.\n\n## Usage\n\nHow to use it.\n\n## API\n\nReference docs.",
        )
        .unwrap();

        let skills = discover_skills(&[base.to_path_buf()]);
        assert_eq!(skills.len(), 1);
        let skill = &skills[0];
        assert_eq!(skill.sections.len(), 3);
        assert_eq!(skill.sections[0].name, "Setup");
        assert_eq!(skill.sections[1].name, "Usage");
        assert_eq!(skill.sections[2].name, "API");
    }
}
