# Manual journal — issue #117 event timeline envelope

- **Date**: 2026-10-06
- **Goal**: #117 fix(events) — events / SSE frames / spans / logs carried no
  time or correlation metadata, so they could not be reassembled into one
  timeline (P1 gap ticket, okguitar).
- **Files touched**:
  - `src/event.rs` — added `EventMeta { ts_ms, seq, session_id, turn }`,
    `EnvelopedEvent` (meta nested under `"meta"` alongside the `AgentEvent`;
    `#[serde(flatten)]` is impossible for the internally-tagged enum), and
    `EnvelopeSink` (stamps a monotonic, per-session `seq` and forwards
    envelopes).
  - `src/lib.rs` — re-export the new types.
  - `src/http/mod.rs` — new `SseFrame { id, event }`; `event_channels` now
    carries `SseFrame`; `SessionState.event_seq: Arc<AtomicU64>` gives each
    session a monotonic event counter.
  - `src/http/handlers.rs` — `send_session_message` wires an `EnvelopeSink`
    (session id + turn + shared `event_seq`) in place of `ChannelSink`; the
    forwarder consumes `EnvelopedEvent`; `session_events` emits the frame `id:`
    via `Event::default().id(id)`.
  - `src/runtime.rs` — `drive_turn` now creates a real per-turn `agent.turn`
    span with `session_id` / `turn` / `steps` fields **declared** and attaches
    it with `.instrument()`; the dead `Span::current().record("session_id", ..)`
    write in `run` is gone; `agent.turn: finished` carries the correlation
    fields explicitly.
  - `src/http/cold_load.rs`, `src/http/mod.rs`, `src/http/handlers.rs`,
    `tests/http.rs` — initialise the new `SessionState.event_seq`.
- **Tests added**:
  - `src/event.rs`: `event_meta_id_carries_wall_clock_turn_and_seq`,
    `enveloped_event_round_trips_meta_and_payload`,
    `enveloped_event_omits_absent_session_id`,
    `envelope_sink_stamps_monotonic_seq_and_correlation`.
  - `src/runtime/tests.rs`: `drive_turn_creates_instrumented_correlated_span`.
- **Notes**:
  - `seq` is monotonic per session (shared counter on `SessionState`), not per
    turn — combined with `turn` in the frame id (`<ts_ms>-<turn>-<seq>`) the
    whole session stream is totally ordered and wall-clock anchored.
  - Runtime visibility of the envelope requires a Langfuse sink that consumes
    the meta — tracked separately in #124; this change is the code half.
  - `agent.run.complete` inherits `session_id` / `turn` from the `agent.turn`
    span it is emitted under (no field added to the log call itself).
