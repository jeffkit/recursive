//! Budgeted line comparison for the per-turn change ledger.
//!
//! Every comparison is bounded twice: by a cell budget on the LCS table
//! (so a pathological pair of large files cannot allocate unbounded memory)
//! and by a wall-clock deadline (default 100 ms). When either bound trips,
//! the comparison degrades to an explicit **whole-file replacement** marked
//! `coarse` — it never fails and never silently drops the change.

use std::time::{Duration, Instant};

/// Maximum number of cells in the LCS table (`(old+1) * (new+1)`).
/// 1M cells ≈ 4 MiB of `u32` — reached at roughly 1000×1000 lines.
pub const MAX_DIFF_TABLE_CELLS: usize = 1_000_000;

/// Maximum size of a rendered diff. Beyond this the patch is cut and the
/// outcome is marked `coarse` (explicit truncation, not a silent drop).
pub const MAX_RENDERED_DIFF_BYTES: usize = 64 * 1024;

/// How many unchanged lines surround a hunk.
pub const DEFAULT_CONTEXT_LINES: usize = 3;

/// Result of a bounded comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOutcome {
    /// Unified-diff text (a whole-file replacement when coarse). Empty when
    /// the two sides are identical.
    pub text: String,
    /// `Some(reason)` when the comparison degraded; the reason is also
    /// embedded in `text` so a reader sees the degradation without extra
    /// plumbing.
    pub coarse: Option<String>,
}

impl DiffOutcome {
    fn identical() -> Self {
        Self {
            text: String::new(),
            coarse: None,
        }
    }

    pub fn is_coarse(&self) -> bool {
        self.coarse.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep(usize),
    Del(usize),
    Ins(usize),
}

/// Split `text` into lines, ignoring a single trailing newline so a file
/// ending with `\n` does not produce a phantom empty last line.
fn lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n').collect()
}

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

fn file_header(path: &str) -> String {
    format!("--- a/{path}\n+++ b/{path}\n")
}

/// Render a whole-file replacement — the explicit degradation path.
fn whole_file(path: &str, old_lines: &[&str], new_lines: &[&str], reason: &str) -> DiffOutcome {
    let mut body = file_header(path);
    body.push_str(&format!(
        "@@ whole-file replacement (coarse: {reason}) @@\n"
    ));
    for l in old_lines {
        body.push('-');
        body.push_str(l);
        body.push('\n');
    }
    for l in new_lines {
        body.push('+');
        body.push_str(l);
        body.push('\n');
    }
    let (text, truncated) = truncate_at_char_boundary(&body, MAX_RENDERED_DIFF_BYTES);
    let coarse = if truncated {
        format!("{reason}; rendered diff truncated at {MAX_RENDERED_DIFF_BYTES} bytes")
    } else {
        reason.to_string()
    };
    if truncated {
        return DiffOutcome {
            text: format!("{text}\n... [diff truncated at {MAX_RENDERED_DIFF_BYTES} bytes]\n"),
            coarse: Some(coarse),
        };
    }
    DiffOutcome {
        text: body,
        coarse: Some(coarse),
    }
}

/// Compare `old` and `new` with a bounded LCS over lines and render a
/// unified diff with `context_lines` lines of context.
///
/// Degrades (never fails) when the table budget, the deadline, or the
/// rendered-size budget is exceeded — see [`DiffOutcome::coarse`].
pub fn unified_diff(
    path: &str,
    old: &str,
    new: &str,
    context_lines: usize,
    deadline: Duration,
) -> DiffOutcome {
    if old == new {
        return DiffOutcome::identical();
    }
    let old_lines = lines(old);
    let new_lines = lines(new);
    if old_lines == new_lines {
        // The bytes differ but the line split does not: a change in the final
        // newline. Report it explicitly — an empty patch would read as
        // "nothing changed".
        let body = format!(
            "{}@@ line ending changed (no line content differs) @@\n",
            file_header(path)
        );
        let (text, truncated) = truncate_at_char_boundary(&body, MAX_RENDERED_DIFF_BYTES);
        return DiffOutcome {
            text: if truncated {
                format!("{text}\n... [diff truncated at {MAX_RENDERED_DIFF_BYTES} bytes]\n")
            } else {
                text
            },
            coarse: None,
        };
    }
    let n = old_lines.len();
    let m = new_lines.len();
    let cells = (n + 1).saturating_mul(m + 1);
    if cells > MAX_DIFF_TABLE_CELLS {
        return whole_file(
            path,
            &old_lines,
            &new_lines,
            &format!("{n}x{m} lines exceeds the {MAX_DIFF_TABLE_CELLS}-cell compare budget"),
        );
    }

    let started = Instant::now();
    let stride = m + 1;
    // `table[i * stride + j]` = LCS length of old[i..] and new[j..].
    let mut table = vec![0u32; cells];
    for i in (0..n).rev() {
        if started.elapsed() > deadline {
            return whole_file(
                path,
                &old_lines,
                &new_lines,
                &format!(
                    "line comparison exceeded the {}ms budget",
                    deadline.as_millis()
                ),
            );
        }
        for j in (0..m).rev() {
            table[i * stride + j] = if old_lines[i] == new_lines[j] {
                table[(i + 1) * stride + (j + 1)] + 1
            } else {
                table[(i + 1) * stride + j].max(table[i * stride + (j + 1)])
            };
        }
    }

    let mut ops: Vec<Op> = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            ops.push(Op::Keep(i));
            i += 1;
            j += 1;
        } else if table[(i + 1) * stride + j] >= table[i * stride + (j + 1)] {
            ops.push(Op::Del(i));
            i += 1;
        } else {
            ops.push(Op::Ins(j));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Del(i));
        i += 1;
    }
    while j < m {
        ops.push(Op::Ins(j));
        j += 1;
    }

    let change_positions: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Keep(_)))
        .map(|(idx, _)| idx)
        .collect();
    if change_positions.is_empty() {
        return DiffOutcome::identical();
    }

    let mut rendered = file_header(path);
    let mut cursor = 0usize;
    while cursor < change_positions.len() {
        let first = change_positions[cursor];
        let mut last = first;
        cursor += 1;
        while cursor < change_positions.len()
            && change_positions[cursor]
                <= last
                    .saturating_add(context_lines.saturating_mul(2))
                    .saturating_add(1)
        {
            last = change_positions[cursor];
            cursor += 1;
        }
        let start = first.saturating_sub(context_lines);
        let end = last
            .saturating_add(context_lines)
            .saturating_add(1)
            .min(ops.len());

        let window = &ops[start..end];
        let old_count = window
            .iter()
            .filter(|op| matches!(op, Op::Keep(_) | Op::Del(_)))
            .count();
        let new_count = window
            .iter()
            .filter(|op| matches!(op, Op::Keep(_) | Op::Ins(_)))
            .count();
        // Unified-diff convention: an empty side reports the line *before*
        // the hunk, so a pure insertion is `-0,0` and a pure deletion is
        // `+0,0`.
        let step_back = |count: usize, start: usize| {
            if count == 0 {
                start.saturating_sub(1)
            } else {
                start
            }
        };
        let old_start = step_back(
            old_count,
            ops[..start]
                .iter()
                .filter(|op| matches!(op, Op::Keep(_) | Op::Del(_)))
                .count()
                + 1,
        );
        let new_start = step_back(
            new_count,
            ops[..start]
                .iter()
                .filter(|op| matches!(op, Op::Keep(_) | Op::Ins(_)))
                .count()
                + 1,
        );

        rendered.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for op in window {
            match op {
                Op::Keep(idx) => {
                    rendered.push(' ');
                    rendered.push_str(old_lines[*idx]);
                }
                Op::Del(idx) => {
                    rendered.push('-');
                    rendered.push_str(old_lines[*idx]);
                }
                Op::Ins(idx) => {
                    rendered.push('+');
                    rendered.push_str(new_lines[*idx]);
                }
            }
            rendered.push('\n');
        }
    }

    let (text, truncated) = truncate_at_char_boundary(&rendered, MAX_RENDERED_DIFF_BYTES);
    if truncated {
        return DiffOutcome {
            text: format!("{text}\n... [diff truncated at {MAX_RENDERED_DIFF_BYTES} bytes]\n"),
            coarse: Some(format!(
                "rendered diff truncated at {MAX_RENDERED_DIFF_BYTES} bytes"
            )),
        };
    }
    DiffOutcome {
        text: rendered,
        coarse: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_DEADLINE: Duration = Duration::from_secs(30);

    #[test]
    fn identical_inputs_produce_no_patch() {
        let out = unified_diff("a.txt", "one\ntwo\n", "one\ntwo\n", 3, NO_DEADLINE);
        assert!(out.text.is_empty());
        assert!(!out.is_coarse());
    }

    #[test]
    fn single_line_change_renders_context_and_counts() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nCHANGED\nd\ne\n";
        let out = unified_diff("f.txt", old, new, 1, NO_DEADLINE);
        assert!(!out.is_coarse());
        assert!(
            out.text
                .starts_with("--- a/f.txt\n+++ b/f.txt\n@@ -2,3 +2,3 @@\n"),
            "{}",
            out.text
        );
        assert!(out.text.contains("-c\n"), "{}", out.text);
        assert!(out.text.contains("+CHANGED\n"), "{}", out.text);
        assert!(out.text.contains(" d\n"), "context line must be present");
    }

    #[test]
    fn three_context_lines_is_the_default_window() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "1\n2\n3\n4\n5\nX\n7\n8\n9\n10\n";
        let out = unified_diff("f.txt", old, new, DEFAULT_CONTEXT_LINES, NO_DEADLINE);
        assert_eq!(DEFAULT_CONTEXT_LINES, 3);
        // Line 6 changed; three lines of context keep old lines 3..=9.
        assert!(out.text.contains("@@ -3,7 +3,7 @@"), "{}", out.text);
    }

    #[test]
    fn hunks_clip_context_at_the_file_edges() {
        let old = "1\n2\n3\n";
        let new = "1\nX\n3\n";
        let out = unified_diff("f.txt", old, new, 10, NO_DEADLINE);
        assert!(out.text.contains("@@ -1,3 +1,3 @@"), "{}", out.text);
    }

    #[test]
    fn far_apart_changes_produce_two_hunks() {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..40 {
            old.push_str(&format!("line{i}\n"));
            if i == 1 || i == 30 {
                new.push_str(&format!("line{i}-changed\n"));
            } else {
                new.push_str(&format!("line{i}\n"));
            }
        }
        let out = unified_diff("f.txt", &old, &new, 3, NO_DEADLINE);
        assert_eq!(out.text.matches("@@ -").count(), 2, "{}", out.text);
    }

    #[test]
    fn adjacent_changes_merge_into_one_hunk() {
        let old = "1\n2\n3\n4\n5\n";
        let new = "1\nX\nY\n4\n5\n";
        let out = unified_diff("f.txt", old, new, 3, NO_DEADLINE);
        assert_eq!(out.text.matches("@@ -").count(), 1, "{}", out.text);
    }

    #[test]
    fn added_file_renders_insert_only_hunk() {
        let out = unified_diff("new.txt", "", "hello\n", 3, NO_DEADLINE);
        assert!(out.text.contains("@@ -0,0 +1,1 @@"), "{}", out.text);
        assert!(out.text.contains("+hello\n"));
    }

    #[test]
    fn removed_file_renders_delete_only_hunk() {
        let out = unified_diff("gone.txt", "bye\n", "", 3, NO_DEADLINE);
        assert!(out.text.contains("@@ -1,1 +0,0 @@"), "{}", out.text);
        assert!(out.text.contains("-bye\n"));
    }

    #[test]
    fn deadline_zero_degrades_to_whole_file_replacement() {
        let old = "a\nb\nc\n";
        let new = "a\nX\nc\n";
        let out = unified_diff("f.txt", old, new, 3, Duration::ZERO);
        assert!(out.is_coarse());
        let reason = out.coarse.clone().unwrap_or_default();
        assert!(reason.contains("exceeded the 0ms budget"), "{reason}");
        assert!(
            out.text.contains("@@ whole-file replacement (coarse:"),
            "{}",
            out.text
        );
        assert!(out.text.contains("-a\n-b\n-c\n"), "{}", out.text);
        assert!(out.text.contains("+a\n+X\n+c\n"), "{}", out.text);
    }

    #[test]
    fn oversized_table_degrades_without_allocating() {
        // 2000x2000 lines -> 4_004_001 cells > MAX_DIFF_TABLE_CELLS.
        let old: String = (0..2000).map(|i| format!("o{i}\n")).collect();
        let new: String = (0..2000).map(|i| format!("n{i}\n")).collect();
        let out = unified_diff("big.txt", &old, &new, 3, NO_DEADLINE);
        assert!(out.is_coarse());
        assert!(
            out.coarse
                .clone()
                .unwrap_or_default()
                .contains("cell compare budget"),
            "{:?}",
            out.coarse
        );
    }

    #[test]
    fn rendered_size_budget_truncates_and_marks_coarse() {
        let old = "x".repeat(MAX_RENDERED_DIFF_BYTES);
        let new = "y".repeat(MAX_RENDERED_DIFF_BYTES);
        let out = unified_diff("huge.txt", &old, &new, 3, NO_DEADLINE);
        assert!(out.text.len() <= MAX_RENDERED_DIFF_BYTES + 128);
        assert!(out.text.contains("[diff truncated at"));
        assert_eq!(
            out.coarse.as_deref(),
            Some("rendered diff truncated at 65536 bytes")
        );
    }

    #[test]
    fn empty_to_empty_is_identical() {
        let out = unified_diff("e.txt", "", "", 3, NO_DEADLINE);
        assert!(out.text.is_empty() && !out.is_coarse());
    }

    #[test]
    fn utf8_is_never_cut_mid_character() {
        let s = "é".repeat(10);
        let (cut, truncated) = truncate_at_char_boundary(&s, 5);
        assert!(truncated);
        assert_eq!(cut, "éé");
    }

    #[test]
    fn trailing_newline_does_not_create_phantom_line() {
        assert_eq!(lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(lines("a\nb"), vec!["a", "b"]);
        assert!(lines("").is_empty());
    }

    #[test]
    fn newline_only_change_is_reported_not_swallowed() {
        let out = unified_diff("f.txt", "ab", "ab\n", 3, NO_DEADLINE);
        assert!(out.text.contains("@@ line ending changed"), "{}", out.text);
        assert!(!out.is_coarse());
        assert_eq!(out.text.matches("@@").count(), 2, "{}", out.text);
    }

    #[test]
    fn huge_context_lines_do_not_overflow() {
        let out = unified_diff("f.txt", "a\nb\n", "a\nX\n", usize::MAX, NO_DEADLINE);
        assert!(out.text.contains("@@ -1,2 +1,2 @@"), "{}", out.text);
    }
}
