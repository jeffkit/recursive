//! Generic tool capability classification.
//!
//! This enum lives in the core tools layer; protocol adapters (ACP, MCP, ...)
//! map FROM this type, never the other way around. Serialised as snake_case
//! for the ACP wire format.

use serde::{Deserialize, Serialize};

/// Category of tool being invoked, serialised as `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Write,
    Execute,
    Search,
    Fetch,
    WebSearch,
    Other,
}

impl ToolKind {
    /// Map an internal tool-registry name to its tool kind.
    ///
    /// This is the single source of truth for name→kind mapping (used by the
    /// ACP bridge's `kind_map` and by `Tool::kind()` overrides must stay in
    /// sync with it).
    pub fn from_tool_name(name: &str) -> Self {
        match name {
            "Read" => ToolKind::Read,
            "Edit" => ToolKind::Edit,
            "Write" => ToolKind::Write,
            "Bash" => ToolKind::Execute,
            "Grep" => ToolKind::Search,
            "Glob" => ToolKind::Search,
            "WebFetch" => ToolKind::Fetch,
            "WebSearch" => ToolKind::WebSearch,
            _ => ToolKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_kind_serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&ToolKind::WebSearch).unwrap(),
            r#""web_search""#
        );
        assert_eq!(serde_json::to_string(&ToolKind::Read).unwrap(), r#""read""#);
        assert_eq!(
            serde_json::to_string(&ToolKind::Write).unwrap(),
            r#""write""#
        );
        assert_eq!(
            serde_json::to_string(&ToolKind::Other).unwrap(),
            r#""other""#
        );
    }

    #[test]
    fn from_tool_name_maps_correctly() {
        assert_eq!(ToolKind::from_tool_name("Read"), ToolKind::Read);
        assert_eq!(ToolKind::from_tool_name("Write"), ToolKind::Write);
        assert_eq!(ToolKind::from_tool_name("Edit"), ToolKind::Edit);
        assert_eq!(ToolKind::from_tool_name("Bash"), ToolKind::Execute);
        assert_eq!(ToolKind::from_tool_name("Grep"), ToolKind::Search);
        assert_eq!(ToolKind::from_tool_name("Glob"), ToolKind::Search);
        assert_eq!(ToolKind::from_tool_name("WebFetch"), ToolKind::Fetch);
        assert_eq!(ToolKind::from_tool_name("WebSearch"), ToolKind::WebSearch);
        assert_eq!(ToolKind::from_tool_name("UnknownTool"), ToolKind::Other);
    }
}
