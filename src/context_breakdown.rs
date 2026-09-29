//! Goal-328 context-breakdown bookkeeping (extracted from `run_core.rs`
//! during Issue #40 to keep run_core.rs inside the invariant #1 line cap).
//!
//! [`StaticBreakdownCache`] holds the per-bucket token counts for the
//! *static* prompt portions, sized once at `RunCore` construction from
//! [`PromptSegments`]. The `tools` and `mcp_dynamic` buckets are also
//! cached because they only change on a `/model` hot-swap or
//! tool-registry change — the same hook that re-creates the runtime.
//! `conversation` and `overhead` stay dynamic (recomputed every step).

use crate::llm::{estimate_tokens, ContextBreakdown, ToolSpec};
use crate::run_core::RunCore;
use crate::system_prompt::PromptSegments;
use crate::tools::ToolRegistry;

/// Cached token counts for the static breakdown buckets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StaticBreakdownCache {
    pub system_prompt: u32,
    pub rules: u32,
    pub skills: u32,
    pub subagents: u32,
    /// Eager tool specs that are NOT MCP / NOT deferred.
    pub tools: u32,
    /// MCP / deferred tool specs.
    pub mcp_dynamic: u32,
}

impl StaticBreakdownCache {
    /// Build a fresh cache from a `PromptSegments` + the registry's tool
    /// specs. `registry` is consulted to partition `eager` vs
    /// `deferred_or_mcp` (McpTool reports `is_deferred() == true`).
    pub(crate) fn build(
        segments: &PromptSegments,
        specs: &[ToolSpec],
        registry: &ToolRegistry,
    ) -> Self {
        let mut tools = 0u32;
        let mut mcp_dynamic = 0u32;
        for spec in specs {
            // Mirror the serde-shape the provider adapter would send: a
            // single ToolSpec serialises to a JSON object with
            // name/description/parameters. We tokenise that JSON text
            // so the local estimate is comparable to what the provider
            // actually sees after wrapping.
            let text = serde_json::to_string(spec).unwrap_or_default();
            let n = estimate_tokens(&text);
            if registry.is_deferred_spec(spec) {
                mcp_dynamic = mcp_dynamic.saturating_add(n);
            } else {
                tools = tools.saturating_add(n);
            }
        }
        Self {
            system_prompt: estimate_tokens(&segments.system_prompt),
            rules: estimate_tokens(&segments.rules),
            skills: estimate_tokens(&segments.skills),
            subagents: estimate_tokens(&segments.subagents),
            tools,
            mcp_dynamic,
        }
    }
}

impl<'a> RunCore<'a> {
    /// Goal-328: build a fresh [`ContextBreakdown`] from the cached static
    /// buckets + a re-tokenised `conversation` (the transcript body).
    /// `provider_total` is the `max(input_tokens, cache_hit + cache_miss)`
    /// reading from the just-completed LLM call; it backs the `overhead`
    /// bucket.
    pub(crate) fn compute_breakdown(&self, provider_total: u32) -> ContextBreakdown {
        // Conversation bucket: bytes/4 over the transcript body. We
        // intentionally re-tokenise every step (rather than caching) so
        // the bucket grows naturally with each new assistant / tool /
        // user message appended this run.
        let mut conversation_bytes: usize = 0;
        for msg in self.messages.iter() {
            conversation_bytes = conversation_bytes.saturating_add(msg.content.len());
            if let Some(rc) = &msg.reasoning_content {
                conversation_bytes = conversation_bytes.saturating_add(rc.len());
            }
        }
        // `estimate_tokens` uses (bytes as f64 / 4.0).ceil() as u32.
        let conversation = estimate_tokens_by_bytes(conversation_bytes);

        let local_sum = self
            .static_breakdown
            .system_prompt
            .saturating_add(self.static_breakdown.rules)
            .saturating_add(self.static_breakdown.skills)
            .saturating_add(self.static_breakdown.subagents)
            .saturating_add(self.static_breakdown.tools)
            .saturating_add(self.static_breakdown.mcp_dynamic)
            .saturating_add(conversation);
        let overhead = provider_total.saturating_sub(local_sum);

        ContextBreakdown {
            system_prompt: self.static_breakdown.system_prompt,
            rules: self.static_breakdown.rules,
            skills: self.static_breakdown.skills,
            subagents: self.static_breakdown.subagents,
            tools: self.static_breakdown.tools,
            mcp_dynamic: self.static_breakdown.mcp_dynamic,
            conversation,
            overhead,
        }
    }
}

/// Goal-328: token estimate from a pre-computed byte count.
///
/// Same arithmetic as [`estimate_tokens`] but takes a `usize` byte-count
/// directly so the conversation bucket can avoid re-iterating the transcript
/// just to read each message's `len()`. Byte-based (like the public helper):
/// over-counts CJK ~3× — intentional, keeps the breakdown buckets in the
/// same unit as `estimate_tokens`.
pub(crate) fn estimate_tokens_by_bytes(bytes: usize) -> u32 {
    ((bytes as f64) / 4.0).ceil() as u32
}
