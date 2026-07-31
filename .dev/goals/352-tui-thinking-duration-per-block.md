# Goal 352 — Fix TUI Thinking duration accumulating across thoughts in a turn

**Roadmap**: TUI UX — the `∴ Thought for Xs` header and the live `∴
Thinking…` spinner both display the **wrong elapsed time** when a single
turn contains more than one reasoning/thinking block (e.g. think → tool
call → think again). The second thought inherits the first thought's
elapsed time and keeps growing. The headline symptom reported by the
user: *"first thought = 10s, second thought also took 10s in reality but
displays as 20s; the Running timer below resets each time, but Thought
should too."*

**Design principle check**:
- Implemented as: additions to the `recursive-tui` crate only — record a
  per-block start instant on the `Reasoning` transcript block, stamp
  `duration_ms` from that instant (not the turn start), and re-arm the
  spinner's step timer when reasoning begins.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT change the agent runtime, tool registry, providers, kernel,
  or any non-TUI crate.
- ❌ Does NOT change any `UiEvent` variant shape or `UserAction` semantics
  (no new events; the existing `ReasoningPartial` / `Reasoning` events
  already carry everything needed).

## Why

Two bugs, same root cause family — the thinking timers are anchored to
the wrong instant:

### Bug A — `∴ Thought for Xs` accumulates across thoughts

`crates/recursive-tui/src/app/event_loop.rs:506-507`:

```rust
fn finalise_streaming_reasoning(&mut self, content: String) {
    let duration_ms = self.turn.started_at.map(|t| t.elapsed().as_millis() as u64);
```

`self.turn.started_at` is the **whole-turn start** (set once by
`TurnState::start` in `crates/recursive-tui/src/cost.rs:184`, cleared only
by `finish`). A turn that contains `reasoning → tool call → reasoning`
has two reasoning blocks; both finalise against the *same* turn clock, so
the second block's `duration_ms` already includes the first thought's
duration **plus** the tool execution time. The displayed `Thought for Xs`
grows monotonically and never reflects the time the model *actually*
spent on that single thought.

### Bug B — live `∴ Thinking…` spinner does not reset at the start of a later thought

`crates/recursive-tui/src/ui/chat.rs:162-173`:

```rust
if app.turn.running {
    let elapsed = app.turn.step_started_at
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    lines.push(... spinner::format_line(..., app.turn.spinner_verb, elapsed) ...);
}
```

The spinner reads `step_started_at`, which is re-armed only on
`ToolCall` (`crates/recursive-tui/src/app/event_loop.rs:61`,
`self.turn.start_step()`). When the model thinks **again after a tool**,
the spinner verb flips back to `"Thinking"` but `step_started_at` is **not**
re-armed — so the live counter keeps climbing from where the previous step
left off, instead of restarting at 0.0. This is the "the live timer
should also start from 0, not from the previous thought's count"
complaint.

### Why a per-block start instant is the right fix

The turn-level clock (`started_at`) and the step-level clock
(`step_started_at`) are both too coarse:
- `started_at` spans the entire turn (Bug A).
- `step_started_at` is re-armed on tool calls, not on reasoning starts,
  and is shared mutable state on `turn` — wrong ownership for "when did
  *this* thought begin."

Each `Reasoning` transcript block is its own visible unit (`∴ Thinking…`
then `∴ Thought for Xs`), so the duration that renders in **its** header
must be measured from **its own** birth. Storing the start instant on the
block makes the data local, correct-by-construction, and immune to
whatever else the turn does.

## Scope (do exactly this, no more)

### 1. Add a start instant to the `Reasoning` block

`crates/recursive-tui/src/model.rs:90-97` — add a `started` field of type
`Option<std::time::Instant>` to the `Reasoning` variant:

```rust
Reasoning {
    text: String,
    streaming: bool,
    /// Wall-clock duration of the thinking phase, stamped when the
    /// block is finalised. `None` while still streaming, or when
    /// the block's start instant is unavailable (e.g. session resume,
    /// where reasoning is reconstructed from history with no timing).
    duration_ms: Option<u64>,
    /// Goal-352: when this reasoning block began streaming, so its
    /// `duration_ms` can be measured per-block instead of from the
    /// turn start. `None` only for reconstructed/resume blocks that
    /// have no live timing; those render as `∴ Thought` (no duration).
    started: Option<std::time::Instant>,
},
```

This is a TUI-internal model change; it does not affect persistence
(session transcript JSON stores the reasoning *text*, not these timing
fields — verify the (de)serialise path if `TranscriptBlock` is serde,
but the block is a render-only view type rebuilt by
`blocks_from_messages`, not persisted directly).

### 2. Stamp the start when a streaming reasoning block is created

`crates/recursive-tui/src/app/event_loop.rs` — in
`append_streaming_reasoning` (around line 477-491), set `started` to
`Some(Instant::now())` on the freshly created block:

```rust
fn append_streaming_reasoning(&mut self, chunk: &str) {
    if let Some(TranscriptBlock::Reasoning {
        text,
        streaming: true,
        ..
    }) = self.blocks.last_mut()
    {
        text.push_str(chunk);
    } else {
        self.blocks.push(TranscriptBlock::Reasoning {
            text: chunk.to_string(),
            streaming: true,
            duration_ms: None,
            started: Some(std::time::Instant::now()),   // Goal-352
        });
    }
}
```

Subsequent deltas append to the existing block and must **not** touch
`started` (it is already `Some`).

### 3. Measure `duration_ms` from the block's own start instant

Same file, `finalise_streaming_reasoning` (around line 506-541). Replace
the turn-clock computation with a per-block computation:

```rust
fn finalise_streaming_reasoning(&mut self, content: String) {
    // Goal-352: measure from THIS block's start, not the turn start.
    // The old code used self.turn.started_at, which accumulated across
    // every thought+tool in the turn (second thought showed 20s after a
    // 10s first thought + 0s gap, etc.).
    let now = std::time::Instant::now();
    for block in self.blocks.iter_mut().rev() {
        if let TranscriptBlock::Reasoning {
            text,
            streaming,
            duration_ms,
            started,
        } = block
        {
            if *streaming {
                *text = content;
                *streaming = false;
                *duration_ms = started.and_then(|t| {
                    now.checked_duration_since(t).map(|d| d.as_millis() as u64)
                });
                return;
            }
        }
    }
    // Non-streaming path (no prior ReasoningPartial): no start instant
    // was recorded, so duration is unknown → renders as `∴ Thought`.
    let block = TranscriptBlock::Reasoning {
        text: content,
        streaming: false,
        duration_ms: None,
        started: None,
    };
    let insert_before_last = matches!(
        self.blocks.last(),
        Some(TranscriptBlock::Assistant { streaming: true, .. })
    );
    if insert_before_last {
        let last_idx = self.blocks.len() - 1;
        self.blocks.insert(last_idx, block);
    } else {
        self.blocks.push(block);
    }
}
```

Use `checked_duration_since` (never `unwrap()` — invariant #5), so a
monotonic-clock oddity yields `None` (→ `∴ Thought`) rather than a panic.
`now` is captured once before the loop so the measured duration is not
inflated by iteration.

### 4. Re-arm the spinner step timer when reasoning begins

`crates/recursive-tui/src/app/event_loop.rs` — in
`append_streaming_reasoning`, when a **new** block is created (the `else`
branch), also call `self.turn.start_step()`. This makes the live
`∴ Thinking…` spinner restart from 0.0 at the start of *each* thought,
symmetric with how `start_step()` already restarts it on each tool call.
Keep the spinner verb as `"Thinking"` (it already is on turn start and
after `finish`).

```rust
} else {
    self.turn.start_step();   // Goal-352: live Thinking timer restarts per thought
    self.blocks.push(TranscriptBlock::Reasoning {
        ...
        started: Some(std::time::Instant::now()),
    });
}
```

Order the `start_step()` and `Instant::now()` adjacent so the spinner and
the block's internal clock share essentially the same origin (sub-millisecond
difference is irrelevant for a 0.1s-resolution display).

### 5. Session-resume path stays untimed

`crates/recursive-tui/src/app/render.rs:43-47` reconstructs `Reasoning`
blocks from persisted history with no live timing. Set the new `started`
field to `None` there (duration already `None`) — these blocks render as
`∴ Thought` with no duration, exactly as today. Do not invent fake
timestamps for resumed reasoning.

```rust
blocks.push(TranscriptBlock::Reasoning {
    text: reasoning.clone(),
    streaming: false,
    duration_ms: None,
    started: None,   // Goal-352: resumed reasoning has no live timing
});
```

### 6. Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs`, `src/runtime/`,
  `src/llm/`, `src/tools/`, `src/event.rs`, `src/message.rs` — the
  runtime already emits `Reasoning` / `PartialReasoning` correctly; this
  is purely a TUI display bug.
- `crates/recursive-tui/src/cost.rs` `TurnState` — do NOT add a
  `reasoning_started_at` field there. Per-block timing belongs on the
  block (step 1), not on the shared turn state (that is the original
  mistake).
- `crates/recursive-tui/src/backend.rs` event mapping — unchanged; no
  new `UiEvent` variant is needed.
- The spinner format string (`ui/spinner.rs`) and the `∴ Thought for
  Xs` format string (`ui/transcript.rs:284-318`) — unchanged; only the
  *value* fed into them changes.

## Tests

Add tests in `crates/recursive-tui/src/app/event_loop.rs` under the
existing `#[cfg(test)] mod tests` (mirror the style of
`reasoning_partials_stream_then_finalise` around line 616):

- `second_thought_duration_does_not_include_first` — **the headline
  regression test.** Simulate a turn with two reasoning blocks separated
  by a tool call, with controlled timing:
  1. `app.handle_ui_event(UiEvent::ReasoningPartial { text: "first".into() })`
     — record `Instant` A.
  2. Sleep ~50ms (or stub the clock if the codebase has a clock seam;
     if not, a real short sleep is acceptable — assert the *inequality*
     not an exact value).
  3. `app.handle_ui_event(UiEvent::Reasoning { content: "first".into() })`
     — capture `duration_ms` of block 0 as `d0`. Assert `d0` is `Some`
     and roughly matches the elapsed since A (e.g. `< 2000ms`).
  4. `app.handle_ui_event(UiEvent::ToolCall { .. })` (any tool).
  5. `app.handle_ui_event(UiEvent::ReasoningPartial { text: "second".into() })`
     — record `Instant` B.
  6. Sleep ~50ms.
  7. `app.handle_ui_event(UiEvent::Reasoning { content: "second".into() })`
     — capture `duration_ms` of the second block as `d1`.
  8. **Assert `d1 < d0 + (B-A)`-ish**: concretely assert `d1` is `Some`
     and `d1 < d0` is FALSE but `d1` is NOT `>= elapsed_since_turn_start`.
     The robust, non-flaky assertion: `d1` should be on the order of the
     *second* sleep (~50ms), NOT the order of the full turn (first sleep
     + tool + second sleep). A clean way: assert `d1.unwrap() < 500ms`
     while the full turn took > 100ms for the first thought alone — i.e.
     the second thought's duration is independent of the first. If real
     sleeps are too flaky in CI, gate the timing test behind a
     `#[ignore]` manual test and instead add a **structural** test that
     asserts `finalise_streaming_reasoning` reads the block's `started`
     field (not `self.turn.started_at`) — e.g. set
     `app.turn.started_at = None` and assert `duration_ms` is still
     `Some` (proving it came from the block, not the turn).
  9. Prefer the **structural** assertion as the primary CI test (no
     sleeps, not flaky): with `app.turn.started_at = None`, a finalised
     streaming reasoning block must still get a `Some(duration_ms)`. This
     directly encodes "duration comes from the block's `started`, not the
     turn's `started_at`." Add the real-timing variant as a separate
     `#[ignore]` test.

- `reasoning_block_stamps_started_on_creation` — after a single
  `ReasoningPartial`, assert the block's `started` field `is_some()`.
  After the final `Reasoning` event, assert `duration_ms.is_some()` and
  `started` is unchanged from before finalisation (finalise must not
  overwrite `started`).

- `resumed_reasoning_has_no_duration` — construct a `Reasoning` block the
  way `blocks_from_messages` does (`started: None, duration_ms: None`)
  and assert `render_reasoning`/`render_block` yields a header containing
  `∴ Thought` and NOT `Thought for`. (Guards step 5 against a future
  regression where someone wires timing into the resume path.)

- Update the existing `reasoning_partials_stream_then_finalise` test
  (line ~616) to also assert the new `started` field is `Some` after the
  partial and remains `Some` (value unchanged) after finalise — keeps the
  existing test honest against the new field.

Then run the TUI gates (per `.dev/skills/tui-acceptance.md`, loaded for
any goal touching `crates/recursive-tui/`):

```bash
.dev/scripts/tui-test-presence.sh   # hard gate: confirms tests were added
.dev/scripts/tui-mutants.sh         # hard gate: no survivors in touched files
```

If `tui-mutants.sh` reports survivors only in terminal-IO code outside
the touched files, that is the expected/acceptable case per
tui-acceptance.md — document each such survivor in the journal.

## Acceptance

- `cargo test -p recursive-tui` green, including
  `second_thought_duration_does_not_include_first` (structural variant)
  and `reasoning_block_stamps_started_on_creation`.
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` clean.
- `cargo fmt --all --check` clean.
- `.dev/scripts/tui-test-presence.sh` exits 0.
- `.dev/scripts/tui-mutants.sh` exits 0 in the touched files
  (`app/event_loop.rs`, `model.rs`).
- No change outside `crates/recursive-tui/` (no runtime/kernel/tool/
  provider edits; no new `UiEvent`/`UserAction` variants).
- Manual/`#[ignore]` timing test (if added) confirms the second thought's
  displayed duration is on the order of its own think time, not the
  cumulative turn time.
- Existing single-thought behaviour unchanged: one reasoning block in a
  turn still shows a sensible `∴ Thought for Xs`.

## Notes for the agent

- **This is a TUI-only display fix.** Everything happens in
  `crates/recursive-tui/`. The runtime already streams `PartialReasoning`
  then `Reasoning` per thought — the bug is purely in how the TUI times
  the block.
- **Do NOT use `self.turn.started_at` for the thought duration.** That is
  the bug. Measure from the block's own `started` instant (step 1). The
  turn clock is fine for the status-bar `⏱ Xs` total-turn timer
  (`ui/status.rs:145`) — leave that alone.
- **`checked_duration_since`, never subtraction/unwrap.** Invariant #5
  (no `unwrap()`/`expect()`). A backwards clock yields `None` → renders
  `∴ Thought` — fail safe, no panic.
- **`started` is write-once.** Set it when the block is created; never
  overwrite on subsequent deltas or on finalise. Finalise computes
  `duration_ms` from it but does not change it.
- **Spinner re-arm (step 4) is what fixes the live timer.** The block's
  `started` fixes the *final* `Thought for Xs`; `turn.start_step()` fixes
  the *live* `Thinking…` counter. Both are needed to match the user's
  two complaints.
- **Timing tests are flaky if they assert exact durations.** Prefer the
  structural test (`turn.started_at = None` → `duration_ms` still `Some`)
  as the CI gate; treat any real-sleep duration test as `#[ignore]` /
  manual. The structural test fully encodes the fix: duration is sourced
  from the block, not the turn.
- **Follow `.dev/skills/tui-acceptance.md`.** Rendered/structural layer
  tests first, then `tui-test-presence.sh`, then `tui-mutants.sh` as a
  hard gate. A PTY tour is optional here (no new terminal-IO code), but
  run it if the flow's gates require it.
- **`git` discipline:** this goal lands in the `recursive` sub-repo
  (`git -C recursive ...`), not the infra4agent monorepo root. The
  self-improve flow commits in the sub-repo.
