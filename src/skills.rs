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
    /// Executable scripts found in <skill_dir>/scripts/
    pub scripts: Vec<SkillScript>,
}

/// A reference document within a skill's `refs/` directory.
#[derive(Debug, Clone)]
pub struct SkillRef {
    /// Filename without extension, e.g. "api-spec"
    pub name: String,
    /// Absolute path to the ref file
    pub path: PathBuf,
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
                    let (name, description) = parse_skill_meta(&content, &dir_path);
                    let refs = discover_refs(&dir_path);
                    let scripts = discover_scripts(&dir_path);
                    skills.push(Skill {
                        name,
                        description,
                        path: skill_file,
                        refs,
                        scripts,
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
        let script_names: Vec<&str> = skill.scripts.iter().map(|s| s.name.as_str()).collect();

        let base = if ref_count > 0 {
            format!(
                "- {}: {} ({} refs)",
                skill.name, skill.description, ref_count
            )
        } else {
            format!("- {}: {}", skill.name, skill.description)
        };

        if script_names.is_empty() {
            lines.push(base);
        } else {
            lines.push(format!("{} [scripts: {}]", base, script_names.join(", ")));
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
                scripts: vec![],
            },
            Skill {
                name: "python-api".to_string(),
                description: "Python API patterns".to_string(),
                path: PathBuf::from("/tmp/skills/python-api/SKILL.md"),
                refs: vec![],
                scripts: vec![],
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
                scripts: vec![],
            },
            Skill {
                name: "no-refs".to_string(),
                description: "No references".to_string(),
                path: PathBuf::from("/tmp/skills/no-refs/SKILL.md"),
                refs: vec![],
                scripts: vec![],
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
                refs: vec![],
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
            },
            Skill {
                name: "no-scripts".to_string(),
                description: "No scripts".to_string(),
                path: PathBuf::from("/tmp/skills/no-scripts/SKILL.md"),
                refs: vec![],
                scripts: vec![],
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
}
