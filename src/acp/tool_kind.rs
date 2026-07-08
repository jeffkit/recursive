//! Sprint-3: Custom `ToolKind` enum for ACP tool_call notifications.
//!
//! The upstream `agent-client-protocol-schema` v1.4 `ToolKind` is missing
//! `Write` and `WebSearch` variants required by the contract.  This module
//! defines a local `ToolKind` with those variants, serialised as snake_case.
//!
//! It also provides `AcpToolKind::from_acp_tool_name(&str)` for mapping
//! internal tool names to ACP tool kinds during registry→bridge wiring.

use serde::{Deserialize, Serialize};

/// Category of tool being invoked, serialised as `snake_case` for ACP wire format.
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
    /// Map an internal tool-registry name to its ACP tool kind.
    ///
    /// This is used to build the bridge's `kind_map` from the tool registry.
    /// Every override in `Tool::kind()` should have a corresponding entry here
    /// so the bridge can map names it sees in `AgentEvent::ToolCall` back to
    /// `ToolKind` without coupling to the `Tool` trait.
    pub fn from_acp_tool_name(name: &str) -> Self {
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
        let json = serde_json::to_string(&ToolKind::WebSearch).unwrap();
        assert_eq!(json, r#""web_search""#);

        let json = serde_json::to_string(&ToolKind::Read).unwrap();
        assert_eq!(json, r#""read""#);

        let json = serde_json::to_string(&ToolKind::Write).unwrap();
        assert_eq!(json, r#""write""#);

        let json = serde_json::to_string(&ToolKind::Other).unwrap();
        assert_eq!(json, r#""other""#);
    }

    #[test]
    fn from_acp_tool_name_maps_correctly() {
        assert_eq!(ToolKind::from_acp_tool_name("Read"), ToolKind::Read);
        assert_eq!(ToolKind::from_acp_tool_name("Write"), ToolKind::Write);
        assert_eq!(ToolKind::from_acp_tool_name("Edit"), ToolKind::Edit);
        assert_eq!(ToolKind::from_acp_tool_name("Bash"), ToolKind::Execute);
        assert_eq!(ToolKind::from_acp_tool_name("Grep"), ToolKind::Search);
        assert_eq!(ToolKind::from_acp_tool_name("Glob"), ToolKind::Search);
        assert_eq!(ToolKind::from_acp_tool_name("WebFetch"), ToolKind::Fetch);
        assert_eq!(
            ToolKind::from_acp_tool_name("WebSearch"),
            ToolKind::WebSearch
        );
        assert_eq!(ToolKind::from_acp_tool_name("UnknownTool"), ToolKind::Other);
    }
}
