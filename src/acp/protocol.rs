//! ACP v1 protocol types — pure data re-exports from `agent-client-protocol-schema`.
//!
//! This module is a thin re-export layer. All wire types come from the upstream
//! `agent-client-protocol-schema` crate (fixed version). No runtime dependencies,
//! no async, no I/O — just `serde`-backed structs and enums.
//!
//! Covers the full ACP v1 spec: initialize, session lifecycle, prompt turns,
//! tool calls, permissions, content blocks, capabilities, and protocol-level
//! notifications.
//!
//! See: <https://agentclientprotocol.com/protocol/v1/>

use agent_client_protocol_schema::v1;

// ── Requests ────────────────────────────────────────────────────────────

/// `authenticate` request from client to agent.
pub use v1::AuthenticateRequest;
/// `session/close` request from client to agent.
pub use v1::CloseSessionRequest;
/// `session/delete` request from client to agent.
pub use v1::DeleteSessionRequest;
/// `initialize` request from client to agent.
pub use v1::InitializeRequest;
/// `session/list` request from client to agent.
pub use v1::ListSessionsRequest;
/// `session/load` request from client to agent.
pub use v1::LoadSessionRequest;
/// `logout` request from client to agent.
pub use v1::LogoutRequest;
/// `session/new` request from client to agent.
pub use v1::NewSessionRequest;
/// `session/prompt` request from client to agent.
pub use v1::PromptRequest;
/// `session/resume` request from client to agent.
pub use v1::ResumeSessionRequest;
/// `session/setConfigOption` request from client to agent.
pub use v1::SetSessionConfigOptionRequest;
/// `session/setMode` request from client to agent.
pub use v1::SetSessionModeRequest;

// ── Responses ───────────────────────────────────────────────────────────

/// `authenticate` response from agent to client.
pub use v1::AuthenticateResponse;
/// `session/close` response from agent to client.
pub use v1::CloseSessionResponse;
/// `session/delete` response from agent to client.
pub use v1::DeleteSessionResponse;
/// `initialize` response from agent to client.
pub use v1::InitializeResponse;
/// `session/list` response from agent to client.
pub use v1::ListSessionsResponse;
/// `session/load` response from agent to client.
pub use v1::LoadSessionResponse;
/// `logout` response from agent to client.
pub use v1::LogoutResponse;
/// `session/new` response from agent to client.
pub use v1::NewSessionResponse;
/// `session/prompt` response from agent to client.
pub use v1::PromptResponse;
/// `session/resume` response from agent to client.
pub use v1::ResumeSessionResponse;
/// `session/setConfigOption` response from agent to client.
pub use v1::SetSessionConfigOptionResponse;
/// `session/setMode` response from agent to client.
pub use v1::SetSessionModeResponse;

// ── Notifications ───────────────────────────────────────────────────────

/// Notification sent by the agent.
pub use v1::AgentNotification;
/// `notifications/cancel` notification.
pub use v1::CancelNotification;
/// `notifications/cancelled` protocol-level notification.
pub use v1::CancelRequestNotification;
/// Notification sent by the client.
pub use v1::ClientNotification;
/// Extension notification.
pub use v1::ExtNotification;
/// Protocol-level notification enum (cancel, etc.).
pub use v1::ProtocolLevelNotification;
/// Session-scoped notification from agent to client.
pub use v1::SessionNotification;

// ── Enums (request/response/notification routing) ───────────────────────

/// All requests an agent can receive.
pub use v1::AgentRequest;
/// All responses an agent can send.
pub use v1::AgentResponse;
/// All requests a client can receive.
pub use v1::ClientRequest;
/// All responses a client can send.
pub use v1::ClientResponse;

// ── Tool calls ──────────────────────────────────────────────────────────

/// A tool call the agent is requested to make.
pub use v1::ToolCall;
/// Content produced by a tool call (content block / diff / terminal).
pub use v1::ToolCallContent;
/// Unique identifier for a tool call within a session.
pub use v1::ToolCallId;
/// A file location accessed or modified by a tool.
pub use v1::ToolCallLocation;
/// Execution status of a tool call.
pub use v1::ToolCallStatus;
/// An update to an existing tool call (status, content, etc.).
pub use v1::ToolCallUpdate;
/// Fields that can be updated in a tool call update.
pub use v1::ToolCallUpdateFields;
// `ToolKind` is defined locally in `src/acp/tool_kind.rs` (Sprint 3).
// The upstream `v1::ToolKind` is missing `Write` and `WebSearch` variants.

// ── Content blocks ──────────────────────────────────────────────────────

/// Audio content.
pub use v1::AudioContent;
/// Binary resource contents.
pub use v1::BlobResourceContents;
/// A block of content (text, image, audio, resource).
pub use v1::ContentBlock;
/// A chunk of streamed content.
pub use v1::ContentChunk;
/// An embedded resource with full content.
pub use v1::EmbeddedResource;
/// Embedded resource variants.
pub use v1::EmbeddedResourceResource;
/// Base64-encoded image content.
pub use v1::ImageContent;
/// A link to an external resource.
pub use v1::ResourceLink;
/// The role of a message (user / agent).
pub use v1::Role;
/// Plain text content.
pub use v1::TextContent;
/// Text resource contents.
pub use v1::TextResourceContents;

// ── Metadata / annotations ──────────────────────────────────────────────

/// Annotations attached to content blocks.
pub use v1::Annotations;
/// Cost information for a turn.
pub use v1::Cost;
/// Arbitrary metadata (opaque JSON object).
pub use v1::Meta;
/// A usage update during a session.
pub use v1::UsageUpdate;

// ── Capabilities ────────────────────────────────────────────────────────

/// Capabilities declared by the agent.
pub use v1::AgentCapabilities;
/// Boolean config option capabilities.
pub use v1::BooleanConfigOptionCapabilities;
/// Capabilities declared by the client.
pub use v1::ClientCapabilities;
/// Client-specific session capabilities.
pub use v1::ClientSessionCapabilities;
/// File-system capabilities.
pub use v1::FileSystemCapabilities;
/// MCP-related capabilities.
pub use v1::McpCapabilities;
/// Prompt-related capabilities.
pub use v1::PromptCapabilities;
/// Session additional directories capabilities.
pub use v1::SessionAdditionalDirectoriesCapabilities;
/// Capabilities for a specific session.
pub use v1::SessionCapabilities;
/// Session close capabilities.
pub use v1::SessionCloseCapabilities;
/// Session config options capabilities.
pub use v1::SessionConfigOptionsCapabilities;
/// Session delete capabilities.
pub use v1::SessionDeleteCapabilities;
/// Session list capabilities.
pub use v1::SessionListCapabilities;
/// Session resume capabilities.
pub use v1::SessionResumeCapabilities;

// ── Permissions ─────────────────────────────────────────────────────────

/// A permission option presented to the user.
pub use v1::PermissionOption;
/// A permission option ID.
pub use v1::PermissionOptionId;
/// The kind of permission option.
pub use v1::PermissionOptionKind;
/// The outcome the client selected for a permission request.
pub use v1::RequestPermissionOutcome;
/// A permission request to the client.
pub use v1::RequestPermissionRequest;
/// The client's response to a permission request.
pub use v1::RequestPermissionResponse;
/// The outcome selected by the user.
pub use v1::SelectedPermissionOutcome;

// ── Session types ───────────────────────────────────────────────────────

/// A boolean config option.
pub use v1::SessionConfigBoolean;
/// A config group ID.
pub use v1::SessionConfigGroupId;
/// A config ID.
pub use v1::SessionConfigId;
/// A config option kind.
pub use v1::SessionConfigKind;
/// A config option for a session.
pub use v1::SessionConfigOption;
/// Config option category.
pub use v1::SessionConfigOptionCategory;
/// A config option value.
pub use v1::SessionConfigOptionValue;
/// A select config option.
pub use v1::SessionConfigSelect;
/// A select config group.
pub use v1::SessionConfigSelectGroup;
/// A select config option item.
pub use v1::SessionConfigSelectOption;
/// Select options grouping.
pub use v1::SessionConfigSelectOptions;
/// A config value ID.
pub use v1::SessionConfigValueId;
/// A session identifier.
pub use v1::SessionId;
/// Information about a session.
pub use v1::SessionInfo;
/// A session mode.
pub use v1::SessionMode;
/// A session mode identifier.
pub use v1::SessionModeId;
/// Current mode state.
pub use v1::SessionModeState;

// ── Session update types ────────────────────────────────────────────────

/// An available commands update.
pub use v1::AvailableCommandsUpdate;
/// A config option update.
pub use v1::ConfigOptionUpdate;
/// A current mode update.
pub use v1::CurrentModeUpdate;
/// A session info update.
pub use v1::SessionInfoUpdate;
/// Types of updates that can be sent during session processing.
pub use v1::SessionUpdate;

// ── Plan types ──────────────────────────────────────────────────────────

/// A plan for execution.
pub use v1::Plan;
/// An entry in a plan.
pub use v1::PlanEntry;
/// The priority of a plan entry.
pub use v1::PlanEntryPriority;
/// The status of a plan entry.
pub use v1::PlanEntryStatus;

// ── Terminal types ──────────────────────────────────────────────────────

/// Terminal exit status.
pub use v1::TerminalExitStatus;
/// A terminal identifier.
pub use v1::TerminalId;

// ── MCP server config ───────────────────────────────────────────────────

/// An MCP server configuration.
pub use v1::McpServer;
/// HTTP-based MCP server config.
pub use v1::McpServerHttp;
/// SSE-based MCP server config.
pub use v1::McpServerSse;
/// Stdio-based MCP server config.
pub use v1::McpServerStdio;

// ── Auth ────────────────────────────────────────────────────────────────

/// Agent auth capabilities.
pub use v1::AgentAuthCapabilities;
/// An authentication method.
pub use v1::AuthMethod;
/// Agent-side auth method.
pub use v1::AuthMethodAgent;
/// An auth method ID.
pub use v1::AuthMethodId;
/// Logout capabilities.
pub use v1::LogoutCapabilities;

// ── General ─────────────────────────────────────────────────────────────

/// Agent method names.
pub use v1::AgentMethodNames;
/// Available command.
pub use v1::AvailableCommand;
/// Input type for an available command.
pub use v1::AvailableCommandInput;
/// Client method names.
pub use v1::ClientMethodNames;
/// A diff representing file modifications.
pub use v1::Diff;
/// An environment variable definition.
pub use v1::EnvVariable;
/// General method names.
pub use v1::GeneralMethodNames;
/// An HTTP header definition.
pub use v1::HttpHeader;
/// Information about an implementation (name, version).
pub use v1::Implementation;
/// A message identifier.
pub use v1::MessageId;
/// Reason the agent stopped a turn.
pub use v1::StopReason;
/// Unstructured command input.
pub use v1::UnstructuredCommandInput;

// ── Errors ──────────────────────────────────────────────────────────────

/// An ACP protocol error.
pub use v1::Error;
/// ACP error codes.
pub use v1::ErrorCode;

// ── Extension types ─────────────────────────────────────────────────────

/// Extension request.
pub use v1::ExtRequest;
/// Extension response.
pub use v1::ExtResponse;

// ── Version ─────────────────────────────────────────────────────────────

/// Protocol version identifier (v0, v1, v2).
pub use agent_client_protocol_schema::ProtocolVersion;

// ── JSON-RPC primitives ─────────────────────────────────────────────────

/// A JSON-RPC response envelope.
pub use v1::Response;

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helper ──────────────────────────────────────────────────────

    /// Serialise round-trip: value → JSON → value', assert equality.
    fn assert_roundtrip<
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
    >(
        val: &T,
    ) {
        let json = serde_json::to_string(val).expect("serialize");
        let val2: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(val, &val2, "roundtrip failed for JSON: {json}");
    }

    /// Round-trip from a JSON literal: JSON → value → JSON' → value', compare value == value'.
    fn assert_json_roundtrip<
        T: serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug + PartialEq,
    >(
        json: &str,
    ) {
        let val: T = serde_json::from_str(json).expect("deserialize from literal");
        let serialized = serde_json::to_string(&val).expect("serialize");
        let val2: T = serde_json::from_str(&serialized).expect("deserialize from serialized");
        assert_eq!(
            val, val2,
            "json roundtrip failed\n  input: {json}\n  serialized: {serialized}"
        );
    }

    // ── C0-04 enum variant tests ────────────────────────────────────

    #[test]
    fn tool_kind_variants_roundtrip() {
        for kind in [
            crate::acp::ToolKind::Read,
            crate::acp::ToolKind::Edit,
            crate::acp::ToolKind::Write,
            crate::acp::ToolKind::Execute,
            crate::acp::ToolKind::Search,
            crate::acp::ToolKind::Fetch,
            crate::acp::ToolKind::WebSearch,
            crate::acp::ToolKind::Other,
        ] {
            assert_roundtrip(&kind);
        }
    }

    #[test]
    fn tool_call_status_variants_roundtrip() {
        for status in [
            ToolCallStatus::Pending,
            ToolCallStatus::InProgress,
            ToolCallStatus::Completed,
            ToolCallStatus::Failed,
        ] {
            assert_roundtrip(&status);
        }
    }

    #[test]
    fn stop_reason_variants_roundtrip() {
        for reason in [
            StopReason::EndTurn,
            StopReason::Cancelled,
            StopReason::MaxTokens,
            StopReason::MaxTurnRequests,
            StopReason::Refusal,
        ] {
            assert_roundtrip(&reason);
        }
    }

    #[test]
    fn permission_option_kind_variants_roundtrip() {
        for kind in [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ] {
            assert_roundtrip(&kind);
        }
    }

    #[test]
    fn plan_entry_status_variants_roundtrip() {
        for status in [
            PlanEntryStatus::Pending,
            PlanEntryStatus::InProgress,
            PlanEntryStatus::Completed,
        ] {
            assert_roundtrip(&status);
        }
    }

    #[test]
    fn plan_entry_priority_variants_roundtrip() {
        for priority in [
            PlanEntryPriority::Low,
            PlanEntryPriority::Medium,
            PlanEntryPriority::High,
        ] {
            assert_roundtrip(&priority);
        }
    }

    #[test]
    fn error_code_variants_roundtrip() {
        for code in [
            ErrorCode::ParseError,
            ErrorCode::InvalidRequest,
            ErrorCode::MethodNotFound,
            ErrorCode::InvalidParams,
            ErrorCode::InternalError,
        ] {
            assert_roundtrip(&code);
        }
    }

    #[test]
    fn role_variants_roundtrip() {
        assert_roundtrip(&Role::User);
        assert_roundtrip(&Role::Assistant);
    }

    #[test]
    fn session_config_kind_variants_roundtrip() {
        // SessionConfigKind is internally tagged on "type".
        // Boolean variant: needs current_value (required).
        assert_json_roundtrip::<SessionConfigKind>(r#"{"type":"boolean","currentValue":true}"#);
        // Select variant: currentValue + options. SessionConfigSelectOptions is untagged;
        // Ungrouped variant is just a Vec<SessionConfigSelectOption>, so JSON is an array.
        assert_json_roundtrip::<SessionConfigKind>(
            r#"{"type":"select","currentValue":"a","options":[{"value":"a","name":"A"}]}"#,
        );
    }

    #[test]
    fn content_block_variants_roundtrip() {
        // Text
        assert_json_roundtrip::<ContentBlock>(r#"{"type":"text","text":"hello"}"#);
        // Image
        assert_json_roundtrip::<ContentBlock>(
            r#"{"type":"image","data":"AAAA","mimeType":"image/png"}"#,
        );
        // Audio
        assert_json_roundtrip::<ContentBlock>(
            r#"{"type":"audio","data":"AAAA","mimeType":"audio/wav"}"#,
        );
        // ResourceLink
        assert_json_roundtrip::<ContentBlock>(
            r#"{"type":"resource_link","uri":"file:///tmp/data.txt","name":"data.txt","mimeType":"text/plain"}"#,
        );
        // Resource (embedded)
        assert_json_roundtrip::<ContentBlock>(
            r#"{"type":"resource","resource":{"uri":"file:///tmp/x","mimeType":"text/plain","text":"content"}}"#,
        );
    }

    #[test]
    fn tool_call_content_variants_roundtrip() {
        // Content variant
        assert_json_roundtrip::<ToolCallContent>(
            r#"{"type":"content","content":{"type":"text","text":"output"}}"#,
        );
        // Diff variant (fields flattened by internal tagging on "type")
        assert_json_roundtrip::<ToolCallContent>(
            r#"{"type":"diff","path":"/tmp/file.txt","newText":"hello"}"#,
        );
        // Terminal variant (fields flattened; terminalId required)
        assert_json_roundtrip::<ToolCallContent>(r#"{"type":"terminal","terminalId":"t1"}"#);
    }

    #[test]
    fn session_update_variants_roundtrip() {
        // user_message_chunk
        assert_json_roundtrip::<SessionUpdate>(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}"#,
        );
        // agent_message_chunk
        assert_json_roundtrip::<SessionUpdate>(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#,
        );
        // tool_call (flattened)
        assert_json_roundtrip::<SessionUpdate>(
            r#"{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"test","kind":"bash","status":"in_progress"}"#,
        );
        // tool_call_update
        assert_json_roundtrip::<SessionUpdate>(
            r#"{"sessionUpdate":"tool_call_update","toolCallId":"tc1","status":"completed"}"#,
        );
    }

    #[test]
    fn mcp_server_variants_roundtrip() {
        // stdio (untagged fallback — no `type` field; McpServerStdio requires name/command/env)
        assert_json_roundtrip::<McpServer>(
            r#"{"name":"fs","command":"node","args":["server.js"],"env":[]}"#,
        );
        // http (tagged)
        assert_json_roundtrip::<McpServer>(
            r#"{"type":"http","name":"api","url":"http://localhost:8080","headers":[]}"#,
        );
        // sse (tagged)
        assert_json_roundtrip::<McpServer>(
            r#"{"type":"sse","name":"events","url":"http://localhost:8080/sse","headers":[]}"#,
        );
    }

    #[test]
    fn embedded_resource_resource_variants_roundtrip() {
        // Text resource
        assert_json_roundtrip::<EmbeddedResourceResource>(
            r#"{"uri":"file:///tmp/x","mimeType":"text/plain","text":"hello"}"#,
        );
        // Blob resource
        assert_json_roundtrip::<EmbeddedResourceResource>(
            r#"{"uri":"file:///tmp/x","mimeType":"application/octet-stream","blob":"AAAA"}"#,
        );
    }

    #[test]
    fn auth_method_variants_roundtrip() {
        // AuthMethod::Agent is the only stable variant (others are feature-gated). It is
        // untagged, so JSON shape is just the inner struct's fields.
        assert_json_roundtrip::<AuthMethod>(r#"{"id":"github","name":"GitHub"}"#);
    }

    #[test]
    fn available_command_input_variants_roundtrip() {
        // AvailableCommandInput is untagged; only the Unstructured variant is stable.
        assert_json_roundtrip::<AvailableCommandInput>(r#"{"hint":"query"}"#);
    }

    // ── C0-05 struct roundtrip tests ────────────────────────────────

    #[test]
    fn initialize_request_roundtrip() {
        assert_json_roundtrip::<InitializeRequest>(
            r#"{"protocolVersion":1,"clientCapabilities":{"terminal":true},"clientInfo":{"name":"zed","version":"1.0"}}"#,
        );
    }

    #[test]
    fn initialize_response_roundtrip() {
        assert_json_roundtrip::<InitializeResponse>(
            r#"{"protocolVersion":1,"agentInfo":{"name":"recursive","version":"0.7.0"},"agentCapabilities":{"promptCapabilities":{"text":true}}}"#,
        );
    }

    #[test]
    fn new_session_request_roundtrip() {
        assert_json_roundtrip::<NewSessionRequest>(r#"{"cwd":"/tmp","mcpServers":[]}"#);
    }

    #[test]
    fn new_session_response_roundtrip() {
        assert_json_roundtrip::<NewSessionResponse>(r#"{"sessionId":"s1","capabilities":{}}"#);
    }

    #[test]
    fn prompt_request_roundtrip() {
        assert_json_roundtrip::<PromptRequest>(
            r#"{"sessionId":"s1","prompt":[{"type":"text","text":"hello"}]}"#,
        );
    }

    #[test]
    fn prompt_response_roundtrip() {
        assert_json_roundtrip::<PromptResponse>(r#"{"stopReason":"end_turn"}"#);
    }

    #[test]
    fn cancel_notification_roundtrip() {
        assert_json_roundtrip::<CancelNotification>(
            r#"{"sessionId":"s1","reason":"user request"}"#,
        );
    }

    #[test]
    fn session_notification_roundtrip() {
        assert_json_roundtrip::<SessionNotification>(
            r#"{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}"#,
        );
    }

    #[test]
    fn tool_call_roundtrip() {
        assert_json_roundtrip::<ToolCall>(
            r#"{"toolCallId":"tc1","title":"test","kind":"bash","status":"in_progress"}"#,
        );
    }

    #[test]
    fn tool_call_update_roundtrip() {
        assert_json_roundtrip::<ToolCallUpdate>(r#"{"toolCallId":"tc1","status":"completed"}"#);
    }

    #[test]
    fn tool_call_location_roundtrip() {
        assert_json_roundtrip::<ToolCallLocation>(r#"{"path":"/tmp/file.txt","line":42}"#);
    }

    #[test]
    fn diff_roundtrip() {
        assert_json_roundtrip::<Diff>(r#"{"path":"/tmp/file.txt","newText":"hello"}"#);
    }

    #[test]
    fn annotations_roundtrip() {
        assert_json_roundtrip::<Annotations>(r#"{"audience":["user"],"priority":0.5}"#);
    }

    #[test]
    fn cost_roundtrip() {
        assert_json_roundtrip::<Cost>(r#"{"amount":0.05,"currency":"USD"}"#);
    }

    #[test]
    fn content_chunk_roundtrip() {
        assert_json_roundtrip::<ContentChunk>(
            r#"{"type":"text","content":{"type":"text","text":"hello"}}"#,
        );
    }

    #[test]
    fn agent_capabilities_roundtrip() {
        assert_json_roundtrip::<AgentCapabilities>(
            r#"{"promptCapabilities":{"text":true},"mcpCapabilities":{"http":true}}"#,
        );
    }

    #[test]
    fn client_capabilities_roundtrip() {
        assert_json_roundtrip::<ClientCapabilities>(r#"{"terminal":true}"#);
    }

    #[test]
    fn session_capabilities_roundtrip() {
        assert_json_roundtrip::<SessionCapabilities>(r#"{}"#);
    }

    #[test]
    fn request_permission_request_roundtrip() {
        // toolCall is required (a ToolCallUpdate), not toolCallId.
        assert_json_roundtrip::<RequestPermissionRequest>(
            r#"{"sessionId":"s1","toolCall":{"toolCallId":"tc1"},"options":[{"kind":"allow_once","optionId":"opt1","name":"Allow"}]}"#,
        );
    }

    #[test]
    fn request_permission_response_roundtrip() {
        assert_json_roundtrip::<RequestPermissionResponse>(
            r#"{"outcome":{"outcome":"selected","optionId":"opt1"}}"#,
        );
    }

    #[test]
    fn implementation_roundtrip() {
        assert_json_roundtrip::<Implementation>(r#"{"name":"recursive","version":"0.7.0"}"#);
    }

    #[test]
    fn load_session_request_roundtrip() {
        assert_json_roundtrip::<LoadSessionRequest>(
            r#"{"sessionId":"s1","cwd":"/tmp","mcpServers":[]}"#,
        );
    }

    #[test]
    fn load_session_response_roundtrip() {
        assert_json_roundtrip::<LoadSessionResponse>(r#"{"sessionId":"s1","messages":[]}"#);
    }

    #[test]
    fn close_session_request_roundtrip() {
        assert_json_roundtrip::<CloseSessionRequest>(r#"{"sessionId":"s1"}"#);
    }

    #[test]
    fn close_session_response_roundtrip() {
        assert_json_roundtrip::<CloseSessionResponse>(r#"{}"#);
    }

    #[test]
    fn session_id_roundtrip() {
        assert_json_roundtrip::<SessionId>(r#""session-abc-123""#);
    }

    #[test]
    fn tool_call_id_roundtrip() {
        assert_json_roundtrip::<ToolCallId>(r#""tc-xyz""#);
    }

    #[test]
    fn message_id_roundtrip() {
        assert_json_roundtrip::<MessageId>(r#""msg-456""#);
    }

    #[test]
    fn terminal_id_roundtrip() {
        assert_json_roundtrip::<TerminalId>(r#""term-1""#);
    }

    #[test]
    fn text_content_roundtrip() {
        assert_json_roundtrip::<TextContent>(r#"{"type":"text","text":"hello"}"#);
    }

    #[test]
    fn image_content_roundtrip() {
        assert_json_roundtrip::<ImageContent>(
            r#"{"type":"image","data":"AAAA","mimeType":"image/png"}"#,
        );
    }

    #[test]
    fn plan_roundtrip() {
        assert_json_roundtrip::<Plan>(r#"{"entries":[]}"#);
    }

    #[test]
    fn plan_entry_roundtrip() {
        assert_json_roundtrip::<PlanEntry>(
            r#"{"id":"p1","content":"Do X","status":"pending","priority":"medium"}"#,
        );
    }

    #[test]
    fn session_mode_roundtrip() {
        assert_json_roundtrip::<SessionMode>(r#"{"id":"default","name":"Default"}"#);
    }

    #[test]
    fn mcp_capabilities_roundtrip() {
        assert_json_roundtrip::<McpCapabilities>(r#"{"http":true,"sse":false}"#);
    }

    #[test]
    fn file_system_capabilities_roundtrip() {
        assert_json_roundtrip::<FileSystemCapabilities>(
            r#"{"readTextFile":true,"writeTextFile":false}"#,
        );
    }

    #[test]
    fn error_roundtrip() {
        assert_json_roundtrip::<Error>(r#"{"code":-32600,"message":"Invalid Request"}"#);
    }

    #[test]
    fn usage_update_roundtrip() {
        assert_json_roundtrip::<UsageUpdate>(r#"{"used":1500,"size":200000}"#);
    }

    #[test]
    fn auth_method_agent_roundtrip() {
        assert_json_roundtrip::<AuthMethodAgent>(r#"{"id":"github","name":"GitHub"}"#);
    }

    #[test]
    fn env_variable_roundtrip() {
        assert_json_roundtrip::<EnvVariable>(r#"{"name":"API_KEY","value":"secret123"}"#);
    }

    #[test]
    fn http_header_roundtrip() {
        assert_json_roundtrip::<HttpHeader>(r#"{"name":"Authorization","value":"Bearer token"}"#);
    }

    // ── C0-06 boundary / edge-case tests ────────────────────────────

    #[test]
    fn option_field_none() {
        // Implementation has optional `title` field
        assert_json_roundtrip::<Implementation>(r#"{"name":"test","version":"1.0"}"#);
        // With title
        assert_json_roundtrip::<Implementation>(
            r#"{"name":"test","version":"1.0","title":"Test App"}"#,
        );
        // Annotations with None audience
        assert_json_roundtrip::<Annotations>(r#"{}"#);
        // ToolCallLocation with None line
        assert_json_roundtrip::<ToolCallLocation>(r#"{"path":"/tmp/f"}"#);
        // Diff with None old_text
        assert_json_roundtrip::<Diff>(r#"{"path":"/tmp/f","newText":"new"}"#);
    }

    #[test]
    fn vec_field_empty() {
        // Plan with empty entries
        assert_json_roundtrip::<Plan>(r#"{"entries":[]}"#);
        // PromptRequest with empty prompt (pathological but valid)
        assert_json_roundtrip::<PromptRequest>(r#"{"sessionId":"s1","prompt":[]}"#);
        // LoadSessionResponse with empty messages
        assert_json_roundtrip::<LoadSessionResponse>(r#"{"sessionId":"s1","messages":[]}"#);
        // ToolCall with empty content & locations
        assert_json_roundtrip::<ToolCall>(
            r#"{"toolCallId":"tc1","title":"t","content":[],"locations":[]}"#,
        );
    }

    #[test]
    fn nested_enum_coverage() {
        // ContentBlock::Text (nested within ContentChunk, SessionUpdate, ToolCallContent, etc.)
        assert_json_roundtrip::<ContentChunk>(
            r#"{"type":"text","content":{"type":"text","text":"hello"}}"#,
        );
        // ContentBlock::Image
        assert_json_roundtrip::<ContentChunk>(
            r#"{"type":"image","content":{"type":"image","data":"AAAA","mimeType":"image/png"}}"#,
        );
        // ContentBlock::ResourceLink
        assert_json_roundtrip::<ContentChunk>(
            r#"{"type":"resource_link","content":{"type":"resource_link","uri":"file:///x","name":"x","mimeType":"text/plain"}}"#,
        );

        // SessionNotification wraps SessionUpdate
        assert_json_roundtrip::<SessionNotification>(
            r#"{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"Run","kind":"execute","status":"in_progress"}}"#,
        );
    }

    #[test]
    fn enum_non_default_variant_roundtrip() {
        // ToolKind non-default (default is Other)
        assert_roundtrip(&crate::acp::ToolKind::Read);
        assert_roundtrip(&crate::acp::ToolKind::Edit);
        // ToolCallStatus non-default (default is Pending)
        assert_roundtrip(&ToolCallStatus::Completed);
        assert_roundtrip(&ToolCallStatus::Failed);
        // StopReason non-default
        assert_roundtrip(&StopReason::Cancelled);
        assert_roundtrip(&StopReason::Refusal);
    }

    #[test]
    fn session_update_plan_variant_roundtrip() {
        // Plan update (internally tagged — fields flattened alongside sessionUpdate)
        assert_json_roundtrip::<SessionUpdate>(r#"{"sessionUpdate":"plan","entries":[]}"#);
    }

    #[test]
    fn session_update_usage_update_variant_roundtrip() {
        assert_json_roundtrip::<SessionUpdate>(
            r#"{"sessionUpdate":"usage_update","used":1000,"size":200000}"#,
        );
    }

    #[test]
    fn permission_option_all_kinds() {
        for (kind_str, kind) in [
            ("allow_once", PermissionOptionKind::AllowOnce),
            ("allow_always", PermissionOptionKind::AllowAlways),
            ("reject_once", PermissionOptionKind::RejectOnce),
            ("reject_always", PermissionOptionKind::RejectAlways),
        ] {
            let json = format!(r#"{{"kind":"{kind_str}","optionId":"o1","name":"L"}}"#);
            assert_json_roundtrip::<PermissionOption>(&json);
            assert_roundtrip(&kind);
        }
    }
}
