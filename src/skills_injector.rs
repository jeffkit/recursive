//! Goal-318: Glob-based skill injection.
//!
//! [`SkillInjector`] is created once per agent run and tracks which
//! `mode: globs` skills have already been injected (at most once per run).
//! After every tool-call batch, [`SkillInjector::check`] scans the raw result
//! strings for file path references; if any path matches a skill's glob
//! patterns, that skill's body is returned for injection as a system message.

use std::collections::HashSet;
use std::fs;

use crate::skills::{Skill, SkillMode};

/// Minimal glob matcher supporting `**` and `*` wildcards.
///
/// Rules:
/// - `**` matches any number of path segments (including zero).
/// - `*`  matches any sequence of non-`/` characters within a segment.
/// - All other characters are matched literally.
/// - Matching is case-sensitive.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    match_glob(pattern, path)
}

fn match_glob(pat: &str, s: &str) -> bool {
    if pat.is_empty() {
        return s.is_empty();
    }

    // `**` at the start (optionally followed by `/`)
    if let Some(rest) = pat.strip_prefix("**/") {
        // Try matching `rest` against every suffix of `s` starting at a
        // segment boundary (i.e. after a `/` or at the very beginning).
        if match_glob(rest, s) {
            return true;
        }
        // Advance through `s` looking for the next `/`
        let mut idx = 0;
        for ch in s.chars() {
            idx += ch.len_utf8();
            if ch == '/' && match_glob(rest, &s[idx..]) {
                return true;
            }
        }
        return false;
    }

    // Lone `**` at the end of pattern matches everything
    if pat == "**" {
        return true;
    }

    // `*` within a segment — matches any run of non-`/` chars
    if let Some(rest) = pat.strip_prefix('*') {
        // Try zero or more non-`/` chars
        let s_bytes = s.as_bytes();
        for i in 0..=s.len() {
            if i > 0 && s_bytes[i - 1] == b'/' {
                break; // `*` cannot cross a `/`
            }
            if match_glob(rest, &s[i..]) {
                return true;
            }
        }
        return false;
    }

    // Literal character match
    let mut pat_chars = pat.chars();
    let mut s_chars = s.chars();
    match (pat_chars.next(), s_chars.next()) {
        (Some(pc), Some(sc)) if pc == sc => match_glob(pat_chars.as_str(), s_chars.as_str()),
        (None, None) => true,
        _ => false,
    }
}

/// Tracks which `Globs`-mode skills have already fired in this agent run.
pub struct SkillInjector {
    globs_skills: Vec<Skill>,
    already_injected: HashSet<String>,
}

impl SkillInjector {
    /// Create a new injector from the full skill list.
    /// Only skills with `mode == Globs` are retained.
    pub fn new(skills: &[Skill]) -> Self {
        let globs_skills = skills
            .iter()
            .filter(|s| s.mode == SkillMode::Globs && s.globs.is_some())
            .cloned()
            .collect();
        Self {
            globs_skills,
            already_injected: HashSet::new(),
        }
    }

    /// Scan raw tool-result strings for path references and return the body
    /// of any newly-matching `Globs`-mode skills.
    ///
    /// Each skill is returned at most once per run (tracked by name).
    /// Returns `Vec<(skill_name, skill_body)>`.
    pub fn check(&mut self, tool_results: &[String]) -> Vec<(String, String)> {
        let paths = extract_paths(tool_results);
        let mut injected = Vec::new();

        for skill in &self.globs_skills {
            if self.already_injected.contains(&skill.name) {
                continue;
            }
            let patterns = match &skill.globs {
                Some(g) if !g.is_empty() => g,
                _ => continue,
            };
            let matches = paths
                .iter()
                .any(|p| patterns.iter().any(|pat| glob_matches(pat, p)));
            if matches {
                if let Ok(content) = fs::read_to_string(&skill.path) {
                    let body = crate::skills::extract_skill_body(&content);
                    injected.push((skill.name.clone(), body.to_string()));
                    self.already_injected.insert(skill.name.clone());
                }
            }
        }

        injected
    }
}

/// Extract candidate file paths from a list of raw tool-result strings.
///
/// A "path" is any token that:
/// - Does not contain whitespace.
/// - Contains at least one `/`.
/// - Does not start with `http://` or `https://`.
fn extract_paths(results: &[String]) -> Vec<&str> {
    let mut paths = Vec::new();
    for result in results {
        for token in result.split_whitespace() {
            // Strip trailing punctuation that might appear in tool output
            let token = token.trim_end_matches([',', '.', ';', ':', '"', '\'', ')']);
            let token = token.trim_start_matches(['"', '\'', '(']);
            if token.contains('/')
                && !token.starts_with("http://")
                && !token.starts_with("https://")
            {
                paths.push(token);
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_exact() {
        assert!(glob_matches("src/runtime.rs", "src/runtime.rs"));
        assert!(!glob_matches("src/runtime.rs", "src/agent.rs"));
    }

    #[test]
    fn glob_matches_double_star() {
        assert!(glob_matches("src/tools/**", "src/tools/fs.rs"));
        assert!(glob_matches("src/tools/**", "src/tools/sub/dir/file.rs"));
        assert!(!glob_matches("src/tools/**", "src/agent.rs"));
    }

    #[test]
    fn glob_matches_single_star() {
        assert!(glob_matches("src/*.rs", "src/agent.rs"));
        assert!(!glob_matches("src/*.rs", "src/tools/fs.rs"));
    }

    #[test]
    fn glob_matches_star_at_end() {
        assert!(glob_matches("src/tools/**", "src/tools/"));
    }

    #[test]
    fn glob_no_match_different_prefix() {
        assert!(!glob_matches("src/tools/**", "tests/integration.rs"));
        assert!(!glob_matches("src/llm/**", "src/tools/fs.rs"));
    }

    #[test]
    fn extract_paths_finds_file_paths() {
        let results = vec![
            "Updated file src/tools/fs.rs successfully.".to_string(),
            "No changes to src/agent.rs".to_string(),
        ];
        let paths = extract_paths(&results);
        assert!(paths.contains(&"src/tools/fs.rs"));
        assert!(paths.contains(&"src/agent.rs"));
    }

    #[test]
    fn extract_paths_ignores_urls() {
        let results = vec!["See https://example.com/docs for more.".to_string()];
        let paths = extract_paths(&results);
        assert!(paths.is_empty());
    }
}
