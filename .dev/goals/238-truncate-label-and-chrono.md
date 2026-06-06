# Goal 238 — truncate_label char/byte bug + replace chrono_lite_now

**Roadmap**: Arch-review bugfixes (part 2/3)

**Design principle check**:
- Implemented as: two surgical bug fixes, no new abstractions
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Two correctness issues from the architecture review:

1. `truncate_label()` in `src/runtime.rs` (line ~1052) collects up to 120
   chars correctly, but then compares `trimmed.len() < s.len()` using byte
   counts. For multibyte input the trimmed string's byte length can be larger
   than 120 (CJK chars = 3 bytes each), so the `< s.len()` comparison will
   produce the wrong result. The correct check is whether the original string
   was longer than 120 chars (single-line check) or had multiple lines.

2. `chrono_lite_now()` in `src/session.rs` is a hand-rolled UTC timestamp
   function using manual epoch arithmetic and a custom `epoch_day_to_ymd`
   helper. The crate already depends on `chrono`. This is unnecessary
   complexity and a maintenance hazard.

## Scope (do exactly this, no more)

### 1. `src/runtime.rs` — `truncate_label`

Replace the comparison so it correctly detects truncation:

```rust
fn truncate_label(s: &str) -> String {
    const MAX: usize = 120;
    let first_line = s.lines().next().unwrap_or("");
    let truncated: String = first_line.chars().take(MAX).collect();
    // was truncated if original had multiple lines OR first line was longer than MAX
    let was_cut = s.lines().count() > 1 || first_line.chars().count() > MAX;
    if was_cut {
        format!("{truncated}…")
    } else {
        truncated
    }
}
```

### 2. `src/session.rs` — replace `chrono_lite_now` and `epoch_day_to_ymd`

Check `Cargo.toml` to confirm the `chrono` dependency is present and
what features are enabled (it is used elsewhere in the codebase).

Replace `chrono_lite_now()` with a small wrapper that calls chrono:

```rust
fn chrono_lite_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
```

Remove the `epoch_day_to_ymd` function and the old `chrono_lite_now`
implementation entirely. All call sites of `chrono_lite_now()` stay
the same (they already use the return value).

If `epoch_day_to_ymd` is `pub(crate)` and used from tests, check first
with grep; remove it only if it has no other callers.

### 3. Tests

- Add a unit test in `src/runtime.rs` for `truncate_label`:
  - ASCII string under 120 chars → no ellipsis
  - ASCII string over 120 chars → ellipsis added
  - Multi-line string → ellipsis added (only first line returned)
  - CJK string of exactly 120 chars → no ellipsis (char count = 120)
  - CJK string of 121 chars → ellipsis added

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `truncate_label` uses char-count comparison, not byte-count
- `chrono_lite_now` uses `chrono::Utc::now()`, no manual epoch math
- `epoch_day_to_ymd` removed (or left if still used elsewhere — check first)

## Notes for the agent

- Read `src/runtime.rs` around line 1049 and `src/session.rs` around
  lines 193-230 to understand the current code before editing.
- Run `grep -n "epoch_day_to_ymd" src/` to find all callers before deleting.
- `chrono` is already in `Cargo.toml` — do NOT add it as a new dependency.
- Use `apply_patch` / surgical edits only. Do NOT rewrite whole files.
- **DO NOT modify** `src/http/`, `src/tools/`, `src/llm/`, `src/run_core.rs`.
