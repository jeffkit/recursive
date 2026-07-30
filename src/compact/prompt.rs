//! Free-text compaction prompt template + post-processing.
//!
//! This module provides the 9-section prompt template used as the free-text
//! fallback when structured compaction is unavailable, and the
//! [`format_compact_summary`] function that strips the `<analysis>` drafting
//! scratchpad and renders the `<summary>` block into a readable form.

/// Preamble telling the model to produce text only (no tool calls during
/// compaction). The compaction call uses an empty tool list already, but
/// this reinforces it for models that hallucinate tool calls.
pub const FREE_TEXT_COMPACT_PROMPT: &str = "\
Your task is to create a detailed summary of the conversation so far for a \
coding agent. This summary will be placed back into the agent's context, so \
it must be comprehensive enough that the agent can continue working without \
loss of important context. Do not attempt to call any tools — produce text only.

Before providing your final summary, wrap your analysis in <analysis> tags. \
This analysis is scratchpad thinking and will be discarded — only the content \
between <summary> tags will be preserved.

Your summary should include the following sections:
1. Primary Request and Intent
2. Key Technical Concepts
3. Files and Code Sections (file paths modified, full code snippets where applicable)
4. Errors and fixes
5. Problem Solving
6. All user messages (non-tool-result)
7. Pending Tasks
8. Current Work (what was being worked on immediately before this summary)
9. Optional Next Step (directly in line with the most recent request; include verbatim quotes)

Wrap the final summary in <summary>…</summary>.";

/// Strip the `<analysis>…</analysis>` drafting scratchpad from the model's
/// response and convert `<summary>…</summary>` into readable section headers.
/// Returns the cleaned summary.
///
/// Processing steps:
/// 1. Remove the first `<analysis>…</analysis>` block (non-greedy).
/// 2. If `<summary>…</summary>` is present, replace the whole match with
///    `Summary:\n<content trimmed>`.
/// 3. Collapse 3+ consecutive newlines to 2.
/// 4. Trim whitespace.
/// 5. If neither tag is present, return `raw.trim()` unchanged (graceful
///    degradation for models that ignore the template).
pub fn format_compact_summary(raw: &str) -> String {
    let mut result = raw.to_string();

    // Step 1: Remove the first <analysis>...</analysis> block (non-greedy).
    if let Some(start) = result.find("<analysis>") {
        if let Some(end) = result[start..].find("</analysis>") {
            let end_absolute = start + end + "</analysis>".len();
            result.replace_range(start..end_absolute, "");
        }
    }

    // Step 2: Extract <summary>...</summary> content if present.
    if let Some(start) = result.find("<summary>") {
        if let Some(end) = result[start..].find("</summary>") {
            let content_start = start + "<summary>".len();
            let content_end = start + end;
            let summary_content = result[content_start..content_end].trim().to_string();
            let end_absolute = start + end + "</summary>".len();
            result.replace_range(start..end_absolute, &format!("Summary:\n{summary_content}"));
        }
    } else {
        // No tags present — graceful degradation.
        return result.trim().to_string();
    }

    // Step 3: Collapse 3+ newlines to 2.
    // Use a simple loop-based approach since regex isn't available as a dep.
    let mut collapsed = String::with_capacity(result.len());
    let mut newline_count = 0_usize;
    for ch in result.chars() {
        if ch == '\n' {
            newline_count += 1;
            if newline_count <= 2 {
                collapsed.push(ch);
            }
        } else {
            newline_count = 0;
            collapsed.push(ch);
        }
    }

    // Step 4: Trim.
    collapsed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_all_nine_sections() {
        let prompt = FREE_TEXT_COMPACT_PROMPT;
        let sections = [
            "Primary Request and Intent",
            "Key Technical Concepts",
            "Files and Code Sections",
            "Errors and fixes",
            "Problem Solving",
            "All user messages",
            "Pending Tasks",
            "Current Work",
            "Optional Next Step",
        ];
        for section in &sections {
            assert!(
                prompt.contains(section),
                "prompt must contain section title: '{section}'"
            );
        }
    }

    #[test]
    fn format_strips_analysis_block() {
        let input = "<analysis>this is scratchpad thinking</analysis>\n\
                     <summary>this is the real summary</summary>";
        let output = format_compact_summary(input);
        assert!(
            !output.contains("scratchpad thinking"),
            "analysis content must be stripped"
        );
        assert!(
            output.contains("this is the real summary"),
            "summary content must be preserved"
        );
    }

    #[test]
    fn format_converts_summary_tag_to_header() {
        let input = "<summary>Key decision: added tool X.</summary>";
        let output = format_compact_summary(input);
        assert!(
            output.starts_with("Summary:"),
            "output must start with 'Summary:' header, got: {output:?}"
        );
        assert!(
            output.contains("Key decision: added tool X."),
            "summary content must be present"
        );
    }

    #[test]
    fn format_preserves_content_when_no_tags() {
        let input = "  plain text without any tags  ";
        let output = format_compact_summary(input);
        assert_eq!(output, "plain text without any tags");
    }

    #[test]
    fn format_collapses_excess_newlines() {
        // Use a summary with lots of newlines before/after.
        let input = "<summary>line1\n\n\n\nline2</summary>";
        let output = format_compact_summary(input);
        // After tag->header conversion and collapse, should have at most 2
        // consecutive newlines between "Summary:" and "line1" and between
        // "line1" and "line2".
        assert!(
            !output.contains("\n\n\n"),
            "output must not contain 3+ consecutive newlines, got: {output:?}"
        );
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
    }

    #[test]
    fn format_handles_tags_embedded_in_other_text() {
        let input = "Some text before.\n\
                     <analysis>scratchpad here</analysis>\n\
                     <summary>Real summary content</summary>\n\
                     Some text after.";
        let output = format_compact_summary(input);
        assert!(!output.contains("scratchpad"), "analysis must be stripped");
        assert!(
            output.contains("Real summary content"),
            "summary content must be preserved"
        );
        assert!(
            output.contains("Summary:"),
            "must contain Summary: header, got: {output:?}"
        );
        // Text outside tags is preserved (graceful, not stripped)
        assert!(output.contains("Some text before."));
        assert!(output.contains("Some text after."));
    }

    #[test]
    fn format_handles_missing_summary_tag() {
        // Only analysis, no summary → graceful degradation (trimmed input).
        let input = "  <analysis>thinking</analysis>  ";
        let output = format_compact_summary(input);
        // Without <summary> tag, the code takes the else branch and returns
        // the trimmed result. Since <analysis> was removed first, then the
        // summary check fails, it returns the trimmed remaining text.
        // The remaining text after analysis removal is "  " (just spaces).
        assert_eq!(
            output, "",
            "without summary tag, should return trimmed post-analysis removal"
        );
    }

    #[test]
    fn format_handles_only_summary_tag_with_no_analysis() {
        let input = "<summary>Just a summary</summary>";
        let output = format_compact_summary(input);
        assert!(output.starts_with("Summary:"));
        assert!(output.contains("Just a summary"));
    }
}
