# Goal 368 — Fix `estimate_tokens` byte-vs-char mismatch (CJK content over-counted 3×)

**Roadmap**: LLM / correctness — token estimation systematically wrong for non-ASCII

**Design principle check**:
- Implemented as: correcting the token-estimation contract to be honest about bytes-vs-chars, and aligning the docstrings + naming. Optionally switching to `chars().count()` if perf allows.
- ❌ Does NOT touch the agent kernel, run loop, or invariants. Estimation is a heuristic used for breakdown display and compaction thresholding; this goal makes it correct/consistent, not more accurate.
- No new deps.

## Why (the correctness bug, with evidence)

`src/llm/chat.rs:99-110`:
```rust
pub fn estimate_tokens(text: &str) -> u32 {
    let chars = text.len();                          // line 100 — BYTES, not chars
    let tokens = (chars as f64 / 4.0).ceil() as u32;
    ...
}
```
The variable is named `chars` and the docstring says "chars/4 heuristic" — but `str::len()` returns **bytes**. For ASCII this is fine; for CJK content (1 char = 3 UTF-8 bytes) the estimate is **~3× too high**. Consequences:
- `compute_breakdown` (`src/run_core.rs:282-318`) inflates the `conversation`/`overhead` buckets → the TUI context gauge wrongly reports near-full.
- Compaction thresholding (which uses `m.content.len()` consistently in bytes — `compact/mod.rs:99-111`) agrees with THIS function in bytes, so compaction itself isn't mis-triggered by the mismatch — but the **reported token counts** (breakdown, cost telemetry, any consumer trusting "tokens") are wrong for non-English transcripts.

The sibling `estimate_tokens_by_chars` (`run_core.rs:59`) takes a `usize` claimed as "char-count" but is actually fed `msg.content.len()` (bytes) — same latent mismatch.

## Scope (do exactly this, no more)

There are two defensible fixes. **Pick option B** (document + rename) unless the team explicitly wants char-accurate estimation — option B is lower-risk and the heuristic is English-biased anyway.

### Option B (recommended): make the contract honest about bytes

1. **`src/llm/chat.rs:99-110`** — rename the variable and fix the docstring:
```rust
/// Rough token estimate for budget/breakdown display. Uses a bytes/4
/// heuristic (≈4 bytes/token for English; over-counts CJK ~3× because CJK
/// chars are 3 UTF-8 bytes each). This is intentionally byte-based for
/// speed (no char iteration) and is only an estimate — exact tokenization
/// would require the provider's tokenizer. Do not use for billing.
pub fn estimate_tokens(text: &str) -> u32 {
    let bytes = text.len();
    let tokens = (bytes as f64 / 4.0).ceil() as u32;
    if tokens == 0 && !text.is_empty() { 1 } else { tokens }
}
```
Keep the function NAME (`estimate_tokens`) for API stability — only the docstring + local variable name change.

2. **`src/run_core.rs:59` `estimate_tokens_by_chars`** — this is misnamed (it receives bytes). Rename to `estimate_tokens_by_bytes` AND fix every call site (search `grep -n estimate_tokens_by_char src/run_core.rs`). The call sites feed it `msg.content.len()` (bytes) — so renaming to `_by_bytes` makes the contract honest. If renaming is risky (many call sites), alternatively just fix the docstring to say "bytes" and leave the name, but renaming is preferred for honesty.

3. **`src/compact/mod.rs:99-111` `estimate_chars`** — this ALSO uses `content.len()` (bytes) but is named `estimate_chars`. Same treatment: either rename to `estimate_bytes` (and its call sites) or fix the docstring to clarify it's byte-count. Check how widely it's called before renaming.

### Option A (NOT recommended, more invasive): switch to `chars().count()`

Replace `text.len()` with `text.chars().count()` in `estimate_tokens`, and `msg.content.len()` with `msg.content.chars().count()` everywhere it feeds token/char estimation. This makes the heuristic char-accurate but (a) iterates every string (perf cost on large transcripts), (b) makes the heuristic LESS accurate for English (4 chars/token is already rough; switching the unit doesn't fix the English bias), (c) changes compaction thresholds' calibration (they're tuned for bytes today). **Do not pick A unless the user explicitly asks for char-accurate estimation.**

### Test

Add a test in `src/llm/chat.rs`'s test module:
- `estimate_tokens_ascii_vs_cjk_ratio`: assert that for an ASCII string of N chars, `estimate_tokens` returns ~N/4; and document (via assertion on the byte count) that a CJK string of N chars returns ~3N/4 (3 bytes/char). The point is to PIN the current byte-based behavior and document the CJK over-count in the test, so a future change to char-based is a deliberate, visible decision.
  ```rust
  #[test]
  fn estimate_tokens_is_byte_based_documents_cjk_overcount() {
      let ascii = "hello world"; // 11 bytes, 11 chars
      let cjk = "你好世界";       // 12 bytes, 4 chars
      // Byte-based: ascii=11/4≈3, cjk=12/4=3 — same tokens despite cjk having 1/3 the chars.
      assert_eq!(estimate_tokens(ascii), estimate_tokens(cjk));
      // This equality IS the known over-count; the docstring explains it.
  }
  ```

## Files NOT to touch

- `src/run_core.rs` production logic beyond the rename/docstring of `estimate_tokens_by_char`.
- The compaction threshold tuning (it's byte-consistent today; don't recalibrate).
- `src/run_core.rs::run_inner`, the kernel, tools.
- `.dev/flows/`.

## Acceptance

- `cargo test --workspace` green, including the new doc-pin test.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean (the rename must not leave a dangling old name).
- `cargo fmt --all` clean.
- Grep: `rg "chars = .*\.len\(\)" src/llm/chat.rs src/run_core.rs` returns **0 hits** (the misleading `chars = ...len()` pattern is gone — either renamed to `bytes` or removed).

## Notes for the agent

- **Prefer Option B (document + rename).** It's lower-risk and the heuristic is a rough estimate anyway. Don't switch to `chars().count()` (Option A) unless the goal explicitly says so.
- **The rename must be complete.** If you rename `estimate_tokens_by_char` → `estimate_tokens_by_bytes`, EVERY call site must update — clippy/cargo check will catch misses, but grep first to size the change.
- **Don't "fix" compaction.** `Compactor::estimate_chars` uses bytes consistently with the threshold calibration; renaming it to `estimate_bytes` is fine, but don't change the unit it operates on — that would mis-trigger compaction.
- **The test pins behavior, doesn't prescribe correctness.** The assertion `estimate_tokens(ascii) == estimate_tokens(cjk)` documents the over-count so it's visible. A future goal can switch to char-based IF the team wants CJK accuracy.
