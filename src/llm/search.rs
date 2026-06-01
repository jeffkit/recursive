//! Tool search engine: keyword-based discovery of deferred tools.
//!
//! Ported from `claude-code/src/tools/ToolSearchTool/ToolSearchTool.ts`
//! (`searchToolsWithKeywords`, lines 186-302). The pure-function
//! `resolve(query, specs) -> Vec<String>` design is preserved so the
//! algorithm is straightforward to test in isolation.
//!
//! Two phases:
//! 1. **Fast path**: exact name match (case-insensitive) returns
//!    immediately. Handles the `select:<tool_name>` style where the
//!    model passes a bare tool name.
//! 2. **Scored keyword search**: split the query into terms, score
//!    each candidate tool by name parts, `search_hint`, and
//!    description word boundaries, return the top N by score.
//!
//! The `+` prefix on a term marks it as required (must appear in
//! name or description); the remaining terms are ranked.

use crate::llm::ToolSpec;

/// Default number of results returned when the caller doesn't
/// specify a `max_results` value.
pub const DEFAULT_MAX_RESULTS: usize = 5;

/// Hard cap on the number of results, regardless of caller request.
pub const MAX_RESULTS_CAP: usize = 20;

/// A `(spec, search_hint)` pair, as produced by
/// `ToolRegistry::partition_by_eagerness`. The hint is local
/// metadata; the spec is the wire shape sent to the model.
pub type SpecWithHint = (ToolSpec, Option<String>);

/// Resolves a free-text query into a ranked list of deferred tool
/// names. Implementations are pure functions of their inputs — no
/// I/O, no async — so they are easy to unit-test.
pub trait ToolSearchEngine: Send + Sync + std::fmt::Debug {
    /// Return up to `max_results` tool names matching `query`.
    /// `max_results` of `None` means "use the default cap".
    fn resolve(&self, query: &str, candidates: &[SpecWithHint]) -> Vec<String>;
}

/// Score constants, mirroring the weights in fake-cc's
/// `searchToolsWithKeywords`:
///   - exact part match in name (regular tool): 10
///   - exact part match in name (MCP tool):    12
///   - partial part match in name (regular):   5
///   - partial part match in name (MCP):       6
///   - full name substring match (fallback):   3
///   - `search_hint` word-boundary match:      4
///   - description word-boundary match:        2
mod weights {
    pub const NAME_EXACT_REGULAR: i32 = 10;
    pub const NAME_EXACT_MCP: i32 = 12;
    pub const NAME_PARTIAL_REGULAR: i32 = 5;
    pub const NAME_PARTIAL_MCP: i32 = 6;
    pub const NAME_FALLBACK: i32 = 3;
    pub const SEARCH_HINT: i32 = 4;
    pub const DESCRIPTION: i32 = 2;
}

/// The default search engine: keyword + CamelCase + `searchHint`
/// weighted scoring, as described in fake-cc's
/// `src/tools/ToolSearchTool/ToolSearchTool.ts:186-302`.
#[derive(Debug, Default, Clone)]
pub struct KeywordSearchEngine;

impl KeywordSearchEngine {
    pub fn new() -> Self {
        Self
    }
}

impl ToolSearchEngine for KeywordSearchEngine {
    fn resolve(&self, query: &str, candidates: &[SpecWithHint]) -> Vec<String> {
        resolve(query, candidates, DEFAULT_MAX_RESULTS)
    }
}

/// Internal entry point exposed for tests so they can pass an
/// explicit `max_results` without going through the trait.
pub(crate) fn resolve(query: &str, candidates: &[SpecWithHint], max_results: usize) -> Vec<String> {
    let max_results = max_results.clamp(1, MAX_RESULTS_CAP);
    let query_lower = query.to_lowercase();
    let query_trim = query_lower.trim();

    if query_trim.is_empty() {
        return Vec::new();
    }

    // Fast path: exact (case-insensitive) name match.
    if let Some((spec, _)) = candidates
        .iter()
        .find(|(s, _)| s.name.to_lowercase() == query_trim)
    {
        return vec![spec.name.clone()];
    }

    // MCP prefix fast path: "mcp__server" matches every tool
    // whose name starts with that prefix.
    if query_trim.starts_with("mcp__") && query_trim.len() > 5 {
        let mut hits: Vec<String> = candidates
            .iter()
            .filter(|(s, _)| s.name.to_lowercase().starts_with(query_trim))
            .map(|(s, _)| s.name.clone())
            .collect();
        if !hits.is_empty() {
            hits.truncate(max_results);
            return hits;
        }
    }

    // Parse query terms: split on whitespace, treat leading `+` as
    // "required". The remaining terms are optional.
    let raw_terms: Vec<&str> = query_trim.split_whitespace().collect();
    let mut required: Vec<String> = Vec::new();
    let mut optional: Vec<String> = Vec::new();
    for term in raw_terms {
        if let Some(stripped) = term.strip_prefix('+') {
            if !stripped.is_empty() {
                required.push(stripped.to_string());
            }
        } else {
            optional.push(term.to_string());
        }
    }
    let scoring_terms: Vec<String> = if !required.is_empty() {
        required.iter().chain(optional.iter()).cloned().collect()
    } else {
        optional.clone()
    };

    if scoring_terms.is_empty() {
        return Vec::new();
    }

    // Pre-filter: a tool only enters scoring if it satisfies ALL
    // required terms. Without required terms, every candidate is
    // considered.
    let filtered: Vec<&SpecWithHint> = if required.is_empty() {
        candidates.iter().collect()
    } else {
        candidates
            .iter()
            .filter(|(s, hint)| {
                let parsed = parse_tool_name(&s.name);
                let hint_lower = hint.as_deref().unwrap_or("").to_lowercase();
                let desc = s.description.to_lowercase();
                required.iter().all(|term| {
                    parsed.parts.iter().any(|p| p == term)
                        || parsed.parts.iter().any(|p| p.contains(term.as_str()))
                        || word_boundary_match(term, &desc)
                        || (!hint_lower.is_empty() && word_boundary_match(term, &hint_lower))
                })
            })
            .collect()
    };

    let mut scored: Vec<(String, i32)> = filtered
        .into_iter()
        .map(|(spec, hint)| {
            let parsed = parse_tool_name(&spec.name);
            let desc_lower = spec.description.to_lowercase();
            let hint_lower = hint.as_deref().unwrap_or("").to_lowercase();
            let mut score: i32 = 0;
            for term in &scoring_terms {
                // Name part match (highest weight)
                if parsed.parts.iter().any(|p| p == term) {
                    score += if parsed.is_mcp {
                        weights::NAME_EXACT_MCP
                    } else {
                        weights::NAME_EXACT_REGULAR
                    };
                } else if parsed.parts.iter().any(|p| p.contains(term.as_str())) {
                    score += if parsed.is_mcp {
                        weights::NAME_PARTIAL_MCP
                    } else {
                        weights::NAME_PARTIAL_REGULAR
                    };
                }
                // Full name substring fallback (only when nothing
                // else has scored yet for this term)
                if score == 0 && parsed.full.contains(term.as_str()) {
                    score += weights::NAME_FALLBACK;
                }
                // search_hint match (curated capability phrase)
                if !hint_lower.is_empty() && word_boundary_match(term, &hint_lower) {
                    score += weights::SEARCH_HINT;
                }
                // Description match
                if word_boundary_match(term, &desc_lower) {
                    score += weights::DESCRIPTION;
                }
            }
            (spec.name.clone(), score)
        })
        .filter(|(_, score)| *score > 0)
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(max_results);
    scored.into_iter().map(|(name, _)| name).collect()
}

/// Split a tool name into searchable parts. Handles both MCP names
/// (`mcp__server__action`) and CamelCase (`ReadFile`).
struct ParsedName {
    parts: Vec<String>,
    full: String,
    is_mcp: bool,
}

fn parse_tool_name(name: &str) -> ParsedName {
    if let Some(stripped) = name.strip_prefix("mcp__") {
        let lowered = stripped.to_lowercase();
        let parts: Vec<String> = lowered
            .split("__")
            .flat_map(|p| p.split('_'))
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        let full = lowered.replace("__", " ").replace('_', " ");
        ParsedName {
            parts,
            full,
            is_mcp: true,
        }
    } else {
        // Split the original CamelCase boundary then on underscores.
        let mut parts: Vec<String> = Vec::new();
        let mut buf = String::new();
        for ch in name.chars() {
            if ch == '_' {
                if !buf.is_empty() {
                    parts.push(buf.to_lowercase());
                    buf.clear();
                }
            } else if ch.is_uppercase() {
                if !buf.is_empty() {
                    parts.push(buf.to_lowercase());
                    buf.clear();
                }
                buf.push(ch.to_ascii_lowercase());
            } else {
                buf.push(ch.to_ascii_lowercase());
            }
        }
        if !buf.is_empty() {
            parts.push(buf);
        }
        let full = parts.join(" ");
        ParsedName {
            parts,
            full,
            is_mcp: false,
        }
    }
}

/// Word-boundary regex match. Avoids substring false positives
/// like "slack" matching "slacks" or "jack".
fn word_boundary_match(term: &str, haystack: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(term) {
        let abs = start + pos;
        let before_ok = abs == 0 || !haystack.as_bytes()[abs - 1].is_ascii_alphanumeric();
        let end = abs + term.len();
        let after_ok = end == haystack.len() || !haystack.as_bytes()[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn specs() -> Vec<SpecWithHint> {
        vec![
            (
                spec("ReadFile", "Read a UTF-8 text file under the workspace."),
                Some("open file contents".to_string()),
            ),
            (
                spec(
                    "WriteFile",
                    "Write content to a file. Overwrites if the file exists.",
                ),
                Some("save content to disk".to_string()),
            ),
            (
                spec(
                    "RunShell",
                    "Execute a shell command and return stdout/stderr.",
                ),
                Some("bash command execution".to_string()),
            ),
            (
                spec(
                    "SearchFiles",
                    "Find files whose path matches a glob pattern.",
                ),
                Some("glob filename lookup".to_string()),
            ),
            (
                spec("WebFetch", "Fetch a URL and return its content as text."),
                Some("download webpage html".to_string()),
            ),
            (
                spec(
                    "mcp__slack__send_message",
                    "Send a message to a Slack channel.",
                ),
                None,
            ),
            (
                spec(
                    "mcp__github__create_pr",
                    "Create a pull request on a GitHub repository.",
                ),
                None,
            ),
        ]
    }

    #[test]
    fn exact_name_match_returns_single_result() {
        let names = resolve("ReadFile", &specs(), 5);
        assert_eq!(names, vec!["ReadFile".to_string()]);
    }

    #[test]
    fn exact_name_match_is_case_insensitive() {
        let names = resolve("readfile", &specs(), 5);
        assert_eq!(names, vec!["ReadFile".to_string()]);
    }

    #[test]
    fn mcp_prefix_returns_matching_tools() {
        let names = resolve("mcp__slack", &specs(), 5);
        assert_eq!(names, vec!["mcp__slack__send_message".to_string()]);
    }

    #[test]
    fn keyword_search_uses_search_hint_weight() {
        // "open" matches ReadFile's hint ("open file contents")
        let names = resolve("open", &specs(), 5);
        assert!(
            names.first().map(|s| s.as_str()) == Some("ReadFile"),
            "expected ReadFile first, got {:?}",
            names
        );
    }

    #[test]
    fn keyword_search_splits_camel_case() {
        // "shell" matches RunShell by name part
        let names = resolve("shell", &specs(), 5);
        assert!(
            names.contains(&"RunShell".to_string()),
            "expected RunShell in {:?}",
            names
        );
    }

    #[test]
    fn required_term_filters_candidates() {
        // +slack should require "slack" in name/description
        let names = resolve("+slack send", &specs(), 5);
        assert!(names.contains(&"mcp__slack__send_message".to_string()));
    }

    #[test]
    fn required_term_excludes_non_matches() {
        // +jupyter should match nothing (no jupyter in our specs)
        let names = resolve("+jupyter notebook", &specs(), 5);
        assert!(names.is_empty());
    }

    #[test]
    fn empty_query_returns_empty() {
        let names = resolve("", &specs(), 5);
        assert!(names.is_empty());
    }

    #[test]
    fn whitespace_only_query_returns_empty() {
        let names = resolve("   ", &specs(), 5);
        assert!(names.is_empty());
    }

    #[test]
    fn max_results_caps_output() {
        // Use a query that matches multiple MCP tools, then cap
        // the output to 1. (A bare "mcp__" prefix is intentionally
        // not a special case — see the fast path's `len() > 5`
        // guard — so we use a keyword that hits both.)
        let names = resolve("slack github", &specs(), 1);
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn max_results_capped_at_internal_limit() {
        let names = resolve("mcp__", &specs(), 1000);
        assert!(names.len() <= MAX_RESULTS_CAP);
    }

    #[test]
    fn word_boundary_prevents_substring_false_positive() {
        // "shell" should NOT match a hypothetical "shellscript" via
        // description substring (the spec below has "shellscript"
        // in description; word-boundary match should not hit).
        let s = vec![(
            spec("shellscript", "Run a shellscript under the workspace."),
            None,
        )];
        let names = resolve("shell", &s, 5);
        // The name part "shell" matches "shellscript" via
        // "contains", which is intentional (CamelCase parts use
        // contains). The word-boundary check is for description
        // and search_hint.
        assert_eq!(names, vec!["shellscript".to_string()]);
        // ... but description word-boundary should NOT match
        // "shellscript" for the term "shell" because there is no
        // word boundary.
        let s2 = vec![(
            spec("Unrelated", "Run a shellscript under the workspace."),
            None,
        )];
        let names2 = resolve("shell", &s2, 5);
        // "Unrelated" doesn't contain "shell" in any part, so
        // description word-boundary should be the only signal —
        // and that should not match.
        assert!(names2.is_empty(), "expected empty, got {:?}", names2);
    }

    #[test]
    fn parse_tool_name_handles_camel_case() {
        let p = parse_tool_name("ReadFile");
        assert_eq!(p.parts, vec!["read", "file"]);
        assert_eq!(p.full, "read file");
        assert!(!p.is_mcp);
    }

    #[test]
    fn parse_tool_name_handles_mcp() {
        let p = parse_tool_name("mcp__slack__send_message");
        assert_eq!(p.parts, vec!["slack", "send", "message"]);
        assert_eq!(p.full, "slack send message");
        assert!(p.is_mcp);
    }

    #[test]
    fn parse_tool_name_handles_snake_case() {
        let p = parse_tool_name("my_tool_name");
        assert_eq!(p.parts, vec!["my", "tool", "name"]);
        assert!(!p.is_mcp);
    }

    #[test]
    fn ordering_ranks_search_hint_above_description() {
        // Two tools, one with a search_hint that matches, one
        // without. The one with the hint should rank higher.
        // Use the exact word "open" in both — word-boundary match
        // does not collapse "opens" → "open".
        let s = vec![
            (
                spec(
                    "NoHintTool",
                    "Something that can open a file in the editor.",
                ),
                None,
            ),
            (
                spec("HintTool", "Does something else."),
                Some("open a file".to_string()),
            ),
        ];
        let names = resolve("open", &s, 5);
        assert_eq!(
            names.first().map(String::as_str),
            Some("HintTool"),
            "searchHint match should rank above description match: {:?}",
            names
        );
    }

    #[test]
    fn no_match_returns_empty() {
        let names = resolve("zzzzz_no_such_thing", &specs(), 5);
        assert!(names.is_empty());
    }
}
