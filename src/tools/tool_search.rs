//! ToolSearchTool — lets the model discover deferred tools on demand.
//!
//! When tool search is enabled, deferred tools are not sent with full schemas
//! in the initial API request. Instead their names appear in an
//! `<available-deferred-tools>` block. The model calls `ToolSearchTool` to
//! retrieve the full JSON schema for one or more deferred tools; the result
//! is a `<functions>` block identical in encoding to the tool list at the
//! top of the prompt. Once a tool's schema has been returned it is callable
//! exactly like any eagerly-loaded tool.
//!
//! The tool is always eager (not deferred itself) and is only registered when
//! at least one deferred tool exists in the registry.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;

use super::{Tool, ToolSideEffect};

pub const TOOL_SEARCH_TOOL_NAME: &str = "ToolSearchTool";

/// The deferred tool catalog shared between the registry and the tool.
/// Updated by `ToolRegistry::freeze_deferred_specs()` after all tools are
/// registered.
pub type DeferredCatalog = Arc<RwLock<Vec<ToolSpec>>>;

/// ToolSearchTool resolves keyword queries or `select:<name>` selectors
/// against the deferred tool catalog and returns full JSON schemas.
#[derive(Clone)]
pub struct ToolSearchTool {
    catalog: DeferredCatalog,
}

impl ToolSearchTool {
    pub fn new(catalog: DeferredCatalog) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_SEARCH_TOOL_NAME.to_string(),
            description: "Fetches full schema definitions for deferred tools so they can be \
                called. Until fetched, only the name is known — there is no parameter schema, \
                so the tool cannot be invoked. This tool takes a query, matches it against the \
                deferred tool list, and returns the matched tools' complete JSONSchema \
                definitions inside a <functions> block. Once a tool's schema appears in that \
                result, it is callable exactly like any tool defined at the top of the prompt.\n\n\
                Result format: each matched tool appears as one \
                <function>{\"description\": \"...\", \"name\": \"...\", \"parameters\": {...}}\
                </function> line inside the <functions> block.\n\n\
                Query forms:\n\
                - \"select:Read,Edit,Grep\" — fetch these exact tools by name\n\
                - \"notebook jupyter\" — keyword search, up to max_results best matches\n\
                - \"+slack send\" — require \"slack\" in the name, rank by remaining terms"
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Query to find deferred tools. Use \"select:<tool_name>\" \
                            for direct selection, or keywords to search."
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 5)",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let max_results = arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(5)
            .clamp(1, 20);

        if query.is_empty() {
            return Err(Error::BadToolArgs {
                name: TOOL_SEARCH_TOOL_NAME.to_string(),
                message: "\"query\" must be a non-empty string".to_string(),
            });
        }

        let catalog = self.catalog.read().map_err(|_| Error::BadToolArgs {
            name: TOOL_SEARCH_TOOL_NAME.to_string(),
            message: "deferred catalog lock poisoned".to_string(),
        })?;

        let matched = resolve(query, &catalog, max_results);

        if matched.is_empty() {
            return Ok(format!(
                "No matching deferred tools found for query \"{query}\". \
                Available: {}",
                catalog
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        // Return the resolved names as a JSON array. The Anthropic provider's
        // message serializer (`serialize_messages_anthropic`) detects this format
        // and converts it to `tool_reference` content blocks, which the API uses
        // to expand full tool schemas into the model's context window.
        let names: Vec<&str> = matched.iter().map(|s| s.name.as_str()).collect();
        Ok(serde_json::to_string(&names).unwrap_or_default())
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    fn is_deferred(&self) -> bool {
        false // ToolSearchTool itself is always eager
    }
}

// ── Search logic ──────────────────────────────────────────────────────────────

/// Resolve a query against the catalog, returning matching `ToolSpec`s.
///
/// Supports:
/// - `select:A,B,C` — exact name lookup, preserves order, skips unknowns
/// - `exact name` — single exact match
/// - `keyword terms` — scored substring search across name + description
pub fn resolve<'a>(query: &str, catalog: &'a [ToolSpec], max_results: usize) -> Vec<&'a ToolSpec> {
    let q = query.trim().to_lowercase();

    // select: prefix — direct name lookup
    if let Some(names_str) = q.strip_prefix("select:") {
        let mut result = Vec::new();
        for name in names_str.split(',').map(|s| s.trim()) {
            if let Some(spec) = catalog.iter().find(|s| s.name.to_lowercase() == name) {
                result.push(spec);
            }
        }
        result.truncate(max_results);
        return result;
    }

    // Exact name match
    if let Some(spec) = catalog.iter().find(|s| s.name.to_lowercase() == q) {
        return vec![spec];
    }

    // Keyword scoring: score each tool by how many query terms appear in
    // name + description (case-insensitive substring).
    let terms: Vec<&str> = q.split_whitespace().collect();
    let mut scored: Vec<(usize, &ToolSpec)> = catalog
        .iter()
        .filter_map(|spec| {
            let haystack = format!(
                "{} {}",
                spec.name.to_lowercase(),
                spec.description.to_lowercase()
            );
            let score = terms.iter().filter(|&&t| haystack.contains(t)).count();
            if score > 0 {
                Some((score, spec))
            } else {
                None
            }
        })
        .collect();

    // Sort by score descending, then name ascending for stability.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(&b.1.name)));
    scored.truncate(max_results);
    scored.into_iter().map(|(_, s)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_spec(name: &str, desc: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    #[test]
    fn select_prefix_returns_exact_matches() {
        let catalog = vec![
            make_spec("Read", "Read a file"),
            make_spec("WebFetch", "Fetch a URL"),
            make_spec("Write", "Write a file"),
        ];
        let result = resolve("select:WebFetch,Read", &catalog, 10);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "WebFetch");
        assert_eq!(result[1].name, "Read");
    }

    #[test]
    fn select_skips_unknown_names() {
        let catalog = vec![make_spec("Read", "Read a file")];
        let result = resolve("select:Read,Unknown", &catalog, 10);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "Read");
    }

    #[test]
    fn exact_name_match_case_insensitive() {
        let catalog = vec![make_spec("WebFetch", "Fetch a URL")];
        let result = resolve("webfetch", &catalog, 10);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "WebFetch");
    }

    #[test]
    fn keyword_search_scores_by_term_matches() {
        let catalog = vec![
            make_spec("Read", "Read a UTF-8 file from disk"),
            make_spec("WebFetch", "Fetch content from a URL via HTTP"),
            make_spec("Write", "Write content to a file"),
        ];
        let result = resolve("file", &catalog, 10);
        // Both Read and Write mention "file"; WebFetch does not
        assert!(result.iter().any(|s| s.name == "Read"));
        assert!(result.iter().any(|s| s.name == "Write"));
        assert!(!result.iter().any(|s| s.name == "WebFetch"));
    }

    #[test]
    fn max_results_is_respected() {
        let catalog = (0..10)
            .map(|i| make_spec(&format!("Tool{i}"), "do something useful"))
            .collect::<Vec<_>>();
        let result = resolve("useful", &catalog, 3);
        assert_eq!(result.len(), 3);
    }

    #[tokio::test]
    async fn execute_returns_json_array_of_names() {
        let catalog: DeferredCatalog =
            Arc::new(RwLock::new(vec![make_spec("WebFetch", "Fetch a URL")]));
        let tool = ToolSearchTool::new(catalog);
        let out = tool
            .execute(json!({"query": "select:WebFetch"}))
            .await
            .unwrap();
        // Output is a JSON array of resolved names — serialized as
        // tool_reference blocks by serialize_messages_anthropic.
        let names: Vec<String> = serde_json::from_str(&out).expect("valid JSON array");
        assert!(
            names.contains(&"WebFetch".to_string()),
            "missing WebFetch: {out}"
        );
    }

    #[tokio::test]
    async fn execute_no_match_returns_helpful_message() {
        let catalog: DeferredCatalog =
            Arc::new(RwLock::new(vec![make_spec("Read", "Read a file")]));
        let tool = ToolSearchTool::new(catalog);
        let out = tool
            .execute(json!({"query": "select:Unknown"}))
            .await
            .unwrap();
        assert!(
            out.contains("No matching"),
            "expected no-match message: {out}"
        );
    }
}
