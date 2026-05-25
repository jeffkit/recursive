//! Skill system: file-based capability extension.
//!
//! Skills are markdown files in specific directories that can be loaded
//! on-demand to extend the agent's capabilities.

use std::fs;
use std::path::{Path, PathBuf};

/// A discovered skill with metadata.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Human-readable name, e.g., "rust-traits".
    pub name: String,
    /// Brief description for the skill index.
    pub description: String,
    /// Absolute path to the SKILL.md file.
    pub path: PathBuf,
    /// Reference documents found in <skill_dir>/refs/
    pub refs: Vec<SkillRef>,
}

/// A reference document within a skill's `refs/` directory.
#[derive(Debug, Clone)]
pub struct SkillRef {
    /// Filename without extension, e.g. "api-spec"
    pub name: String,
    /// Absolute path to the ref file
    pub path: PathBuf,
}

/// Discover skills in the given search paths.
///
/// For each `<path>/<name>/SKILL.md`, parses optional YAML frontmatter.
/// If absent, uses the parent directory name as `name` and the first
/// non-empty line of body as `description`.
///
/// Also scans `<skill_dir>/refs/` for `.md` and `.txt` files and populates
/// `Skill::refs` with what's found.
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
                    let (name, description) = parse_skill_meta(&content, &dir_path);
                    let refs = discover_refs(&dir_path);
                    skills.push(Skill {
                        name,
                        description,
                        path: skill_file,
                        refs,
                    });
                }
            }
        }
    }

    skills
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

/// Parse YAML frontmatter (if present) from skill content.
///
/// Returns (name, description). If frontmatter is absent, falls back to
/// using the parent directory name and first non-empty line.
fn parse_skill_meta(content: &str, dir_path: &Path) -> (String, String) {
    // Try to extract YAML frontmatter: --- ... ---
    if let Some(frontmatter) = content.strip_prefix("---") {
        if let Some(end) = frontmatter.find("---") {
            let yaml = &frontmatter[..end];
            let body = frontmatter[end + 3..].trim();

            // Parse naive key: value pairs
            let mut name = None;
            let mut description = None;

            for line in yaml.lines() {
                let line = line.trim();
                if let Some(stripped) = line.strip_prefix("name:") {
                    name = Some(stripped.trim().to_string());
                } else if let Some(stripped) = line.strip_prefix("description:") {
                    description = Some(stripped.trim().to_string());
                }
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

            return (final_name, final_description);
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

    (name, description)
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
        let ref_count = skill.refs.len();
        if ref_count > 0 {
            lines.push(format!(
                "- {}: {} ({} refs)",
                skill.name, skill.description, ref_count
            ));
        } else {
            lines.push(format!("- {}: {}", skill.name, skill.description));
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
                refs: vec![],
            },
            Skill {
                name: "python-api".to_string(),
                description: "Python API patterns".to_string(),
                path: PathBuf::from("/tmp/skills/python-api/SKILL.md"),
                refs: vec![],
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
            },
            Skill {
                name: "no-refs".to_string(),
                description: "No references".to_string(),
                path: PathBuf::from("/tmp/skills/no-refs/SKILL.md"),
                refs: vec![],
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
}
