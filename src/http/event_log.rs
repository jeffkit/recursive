//! Issue #97: bounded, per-session replay buffer for the SSE stream.
//!
//! `GET /sessions/:id/events` used to be live-only. A subscriber that fell
//! behind the per-session broadcast channel had everything it missed dropped
//! silently — [`tokio_stream::wrappers::BroadcastStream`] reports a *lagged*
//! receiver as an error, and the handler discarded it. A reconnect (mobile
//! network switch, proxy timeout, or the stream's own 1 h cap) likewise
//! restarted from the subscription instant, so `PartialMessage` / `ToolCall` /
//! `ToolResult` / `Done` frames emitted while the client was away were gone
//! for good.
//!
//! This log keeps the last [`SESSION_EVENT_LOG_CAPACITY`] frames of a session
//! in a ring, each stamped with a monotonically increasing *position*. A
//! subscriber resolves its resume cursor (the `Last-Event-ID` header or
//! `?since=`) against the ring and is replayed what it missed; the live
//! broadcast then acts only as a wake-up, so a lagged receiver recovers from
//! the ring instead of losing frames. When the ring itself no longer reaches
//! back to the client's cursor the replay is flagged `truncated` and the
//! handler emits an explicit `gap` frame — a gap is never silent.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use super::SseFrame;

/// How many frames a session retains for replay.
///
/// A streaming turn emits one frame per token delta, so this is a
/// replay-window bound, not a whole-turn guarantee: it comfortably covers a
/// reconnect after seconds of disconnection (the case the issue reports) and
/// degrades to an explicit `gap` notice beyond that.
pub const SESSION_EVENT_LOG_CAPACITY: usize = 512;

/// What a subscriber must receive to (re)join the stream at a cursor, plus
/// the position its live pump resumes from.
#[derive(Debug, Default)]
pub struct SessionReplay {
    pub frames: Vec<SseFrame>,
    /// Position of the next frame the subscriber must receive — an *inclusive*
    /// lower bound, i.e. already-delivered frames are the ones below it. It is
    /// always the log's write position at the time of the call, so the pump
    /// never re-sends a replayed frame and never skips a later one.
    pub next_pos: u64,
    /// `true` when frames the subscriber asked for — or should have received —
    /// are no longer retained. The caller emits a `gap` frame.
    pub truncated: bool,
}

/// One frame in the ring, tagged with the position it was assigned on push.
#[derive(Debug, Clone)]
struct StoredFrame {
    pos: u64,
    frame: SseFrame,
}

/// Bounded, per-session ring of SSE frames (issue #97).
#[derive(Debug)]
pub struct SessionEventLog {
    capacity: usize,
    inner: Mutex<LogInner>,
}

#[derive(Debug)]
struct LogInner {
    /// Position the next pushed frame will get. Retained frames always have a
    /// position `< next_pos`, which makes gaps detectable by arithmetic.
    next_pos: u64,
    frames: VecDeque<StoredFrame>,
    /// Goal 422 W1 (#132): set on a ring rebuilt from storage (cold load).
    /// Every event sequence number below the floor was emitted before this
    /// ring existed — by a previous process or a previous instance of the
    /// session — so a cursor below it can never resolve here, and the frames
    /// it names are *really* gone rather than garbage. `None` on a ring born
    /// in this process, where an unresolved cursor means exactly what issue
    /// #97 said: nothing was ever evicted, or the cursor never named a frame.
    history_floor: Option<u64>,
}

impl SessionEventLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(LogInner {
                next_pos: 0,
                frames: VecDeque::new(),
                history_floor: None,
            }),
        }
    }

    /// Ring for a session restored from storage (goal 422 W1, issue #132).
    ///
    /// Two things differ from a fresh ring, both driven by the persisted
    /// event watermark:
    ///
    /// - `history_floor` (the watermark's `next_seq`) classifies cursors: a
    ///   reconnecting client whose cursor names a sequence below the floor is
    ///   holding a *real* pre-restart cursor, so [`Self::replay`] resolves it
    ///   to the snapshot-or-gap path instead of a silent empty resume.
    /// - `next_pos` starts at 1, not 0: this ring replaced an earlier one, so
    ///   from birth it must report `anything_evicted` — a cursor that
    ///   unresolvedly names the un-persisted window between watermarks (a
    ///   crash mid-turn) still gets the explicit gap/snapshot path, never a
    ///   silent one.
    pub fn restored(capacity: usize, history_floor: Option<u64>) -> Self {
        Self {
            capacity,
            inner: Mutex::new(LogInner {
                next_pos: 1,
                frames: VecDeque::new(),
                history_floor,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LogInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Append `frame`, evicting the oldest entry once the ring is full.
    pub fn push(&self, frame: SseFrame) {
        let mut inner = self.lock();
        let pos = inner.next_pos;
        inner.next_pos += 1;
        inner.frames.push_back(StoredFrame { pos, frame });
        while inner.frames.len() > self.capacity {
            inner.frames.pop_front();
        }
    }

    /// Resolve `cursor` — the `id:` of the last frame a client saw, or a bare
    /// sequence number — into the frames it must receive to catch up.
    ///
    /// `None` means "start from now": a fresh subscriber is not sent the
    /// history it never asked for.
    pub fn replay(&self, cursor: Option<&str>) -> SessionReplay {
        let inner = self.lock();
        let Some(cursor) = cursor else {
            return SessionReplay {
                next_pos: inner.next_pos,
                ..SessionReplay::default()
            };
        };
        // A resolved cursor has been delivered, so the subscriber's next frame
        // is the one after it; an unresolved one falls back to the whole ring.
        //
        // Goal 422 W1 (#132): but first distinguish a *real* pre-restart
        // cursor from garbage. A cursor whose sequence sits below the
        // persisted watermark named frames this ring can never hold — the
        // history it asks for is genuinely elsewhere (the transcript), so the
        // caller must open with a snapshot or an explicit gap. A cursor at or
        // above the floor keeps the issue-#97 unresolved semantics: on a
        // restored ring that yields the whole ring + `truncated` (the
        // un-persisted window is real), on a never-evicted ring it stays a
        // non-gap unknown.
        if let (Some(floor), Some(seq)) = (inner.history_floor, frame_seq(cursor)) {
            if seq < floor {
                return SessionReplay {
                    frames: inner.all_frames(),
                    next_pos: inner.next_pos,
                    truncated: true,
                };
            }
        }
        let (frames, truncated) = match inner.position_of(cursor) {
            Some(pos) => (inner.frames_from(pos.saturating_add(1)), false),
            None => (inner.all_frames(), inner.anything_evicted()),
        };
        SessionReplay {
            frames,
            next_pos: inner.next_pos,
            truncated,
        }
    }

    /// Frames from `next` onward — what the live pump sends after a wake-up.
    ///
    /// `next` is the position the pump expects next (inclusive), which is
    /// always a value a previous call returned as [`SessionReplay::next_pos`].
    pub fn drain_from(&self, next: u64) -> SessionReplay {
        let inner = self.lock();
        SessionReplay {
            frames: inner.frames_from(next),
            next_pos: inner.next_pos,
            truncated: inner.gap_from(next),
        }
    }
}

impl LogInner {
    /// Position of the frame `cursor` refers to, when it is still retained.
    fn position_of(&self, cursor: &str) -> Option<u64> {
        if let Some(stored) = self.frames.iter().find(|s| s.frame.id == cursor) {
            return Some(stored.pos);
        }
        // A bare `?since=<seq>` (or an id whose frame was evicted): fall back
        // to the trailing sequence number shared by every id of one event.
        let seq = frame_seq(cursor)?;
        self.frames
            .iter()
            .rev()
            .find(|s| frame_seq(&s.frame.id) == Some(seq))
            .map(|s| s.pos)
    }

    fn frames_from(&self, next: u64) -> Vec<SseFrame> {
        self.frames
            .iter()
            .filter(|s| s.pos >= next)
            .map(|s| s.frame.clone())
            .collect()
    }

    fn all_frames(&self) -> Vec<SseFrame> {
        self.frames.iter().map(|s| s.frame.clone()).collect()
    }

    /// `true` when this ring evicted anything at all.
    fn anything_evicted(&self) -> bool {
        match self.frames.front() {
            Some(front) => front.pos > 0,
            None => self.next_pos > 0,
        }
    }

    /// `true` when the frames a subscriber waiting at `next` still needs are
    /// not all retained — i.e. the oldest surviving frame starts after `next`.
    /// Positions are contiguous and the ring always keeps a suffix, so this is
    /// exact rather than a guess.
    fn gap_from(&self, next: u64) -> bool {
        match self.frames.front() {
            Some(front) => front.pos > next,
            None => next < self.next_pos,
        }
    }
}

/// Trailing numeric component of a frame id — the cursor unit a client echoes.
///
/// Ids are `<ts_ms>-<turn>-<seq>`, with a `:progress` suffix on frames derived
/// from the same event ([`crate::event::EventMeta::id`]); all of them end in
/// the event's sequence number.
///
/// Goal 422 W1 (#132): also the unit the snapshot path dedupes on — a
/// snapshot carries the sequence it is current up to, and frames below it are
/// already folded in.
pub(crate) fn frame_seq(id: &str) -> Option<u64> {
    id.strip_suffix(":progress")
        .unwrap_or(id)
        .rsplit('-')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::SseEvent;

    fn frame(id: &str) -> SseFrame {
        SseFrame {
            id: id.to_string(),
            event: SseEvent::ToolCall {
                name: "Read".into(),
                step: 0,
            },
        }
    }

    fn ids(replay: &SessionReplay) -> Vec<&str> {
        replay.frames.iter().map(|f| f.id.as_str()).collect()
    }

    #[test]
    fn push_evicts_oldest_beyond_capacity() {
        let log = SessionEventLog::new(2);
        log.push(frame("1-0-0"));
        log.push(frame("1-0-1"));
        log.push(frame("1-0-2"));

        assert_eq!(ids(&log.drain_from(0)), vec!["1-0-1", "1-0-2"]);
        assert_eq!(log.drain_from(0).next_pos, 3);
    }

    #[test]
    fn replay_without_cursor_starts_from_now() {
        let log = SessionEventLog::new(4);
        log.push(frame("1-0-0"));
        log.push(frame("1-0-1"));

        let replay = log.replay(None);
        assert!(
            replay.frames.is_empty(),
            "a fresh subscriber sees no history"
        );
        assert_eq!(replay.next_pos, 2);
        assert!(!replay.truncated);
    }

    #[test]
    fn replay_after_id_resumes_strictly_after_it() {
        let log = SessionEventLog::new(4);
        log.push(frame("1-0-0"));
        log.push(frame("1-0-1:progress"));
        log.push(frame("1-0-2"));

        let replay = log.replay(Some("1-0-0"));
        assert_eq!(ids(&replay), vec!["1-0-1:progress", "1-0-2"]);
        assert_eq!(replay.next_pos, 3);
        assert!(!replay.truncated);
    }

    #[test]
    fn replay_accepts_a_bare_sequence_number() {
        let log = SessionEventLog::new(4);
        log.push(frame("1-0-0"));
        log.push(frame("1-0-1"));
        log.push(frame("1-0-2"));

        // `?since=1` skips everything up to and including the frame with seq 1.
        assert_eq!(ids(&log.replay(Some("1"))), vec!["1-0-2"]);
    }

    #[test]
    fn replay_of_evicted_cursor_returns_all_and_flags_gap() {
        let log = SessionEventLog::new(2);
        log.push(frame("1-0-0"));
        log.push(frame("1-0-1"));
        log.push(frame("1-0-2"));

        let replay = log.replay(Some("1-0-0"));
        assert_eq!(ids(&replay), vec!["1-0-1", "1-0-2"]);
        assert!(
            replay.truncated,
            "an evicted cursor must be reported as a gap"
        );
    }

    #[test]
    fn drain_from_flags_a_gap_only_when_frames_were_skipped() {
        let log = SessionEventLog::new(2);
        log.push(frame("1-0-0"));
        log.push(frame("1-0-1"));
        log.push(frame("1-0-2"));
        // Retained: positions 1 and 2.
        assert_eq!(ids(&log.drain_from(1)), vec!["1-0-1", "1-0-2"]);
        assert!(!log.drain_from(1).truncated, "the next frame is retained");
        // A pump still waiting at 0 cannot get position 0 back.
        assert_eq!(ids(&log.drain_from(0)), vec!["1-0-1", "1-0-2"]);
        assert!(log.drain_from(0).truncated, "position 0 was evicted");

        // A one-slot ring drops the frame a pump waiting at 0 expected next.
        let narrow = SessionEventLog::new(1);
        narrow.push(frame("1-0-0"));
        narrow.push(frame("1-0-1"));
        narrow.push(frame("1-0-2"));
        assert_eq!(ids(&narrow.drain_from(1)), vec!["1-0-2"]);
        assert!(
            narrow.drain_from(1).truncated,
            "position 1 was evicted while the pump waited for it"
        );
    }

    #[test]
    fn unknown_cursor_on_a_never_evicted_ring_is_not_a_gap() {
        let log = SessionEventLog::new(4);
        log.push(frame("1-0-0"));
        log.push(frame("1-0-1"));

        // A cursor that never named a frame rewinds to the whole ring, but
        // nothing was dropped, so the subscriber must not be told otherwise.
        let replay = log.replay(Some("not-a-frame-id"));
        assert_eq!(ids(&replay), vec!["1-0-0", "1-0-1"]);
        assert!(!replay.truncated);
    }

    // ── Goal 422 W1 (#132): rings rebuilt from storage ──────────────────

    #[test]
    fn restored_ring_resolves_a_pre_watermark_cursor_to_an_explicit_gap() {
        // Watermark 10: sequences 0..10 were emitted before this ring existed.
        let log = SessionEventLog::restored(4, Some(10));

        // The real pre-restart cursor cannot resolve here, and the frames it
        // asks for are genuinely gone — that must be said, not skipped.
        let replay = log.replay(Some("1-0-3"));
        assert!(
            replay.truncated,
            "a cursor below the watermark is a real gap, not a silent resume"
        );
        assert!(replay.frames.is_empty(), "nothing was pushed yet");
        assert_eq!(replay.next_pos, 1, "a restored ring is seeded past 0");
    }

    #[test]
    fn restored_ring_hands_a_pre_watermark_cursor_everything_pushed_since() {
        let log = SessionEventLog::restored(4, Some(10));
        // The restored session's own frames continue the sequence space.
        log.push(frame("2-1-10"));
        log.push(frame("2-1-11"));

        let replay = log.replay(Some("1-0-5"));
        assert!(replay.truncated);
        assert_eq!(
            ids(&replay),
            vec!["2-1-10", "2-1-11"],
            "post-restore frames were never seen by the client and must follow"
        );
        assert_eq!(replay.next_pos, 3);
    }

    #[test]
    fn restored_ring_resolves_its_own_frames_normally() {
        let log = SessionEventLog::restored(4, Some(10));
        log.push(frame("2-1-10"));
        log.push(frame("2-1-11"));
        log.push(frame("2-1-12"));

        // A cursor naming a frame this ring holds resumes strictly after it,
        // exactly as on a fresh ring — the floor only classifies the past.
        let replay = log.replay(Some("2-1-10"));
        assert_eq!(ids(&replay), vec!["2-1-11", "2-1-12"]);
        assert!(!replay.truncated);
        // A bare sequence works the same way.
        assert_eq!(ids(&log.replay(Some("11"))), vec!["2-1-12"]);
    }

    #[test]
    fn restored_ring_treats_the_unpersisted_window_as_a_gap_not_silence() {
        // Watermark persisted at 10, but the process crashed mid-turn after
        // emitting up to 14: a client holding `…-12` is above the floor yet
        // names nothing this ring has. Issue #97's "unresolved → whole ring"
        // rule applies, and the seeded position makes it `truncated`.
        let log = SessionEventLog::restored(4, Some(10));
        let replay = log.replay(Some("1-0-12"));
        assert!(
            replay.truncated,
            "an unresolved cursor on a rebuilt ring must be explicit"
        );
        assert!(replay.frames.is_empty());
    }

    #[test]
    fn restored_ring_without_a_watermark_is_still_explicit_about_lost_history() {
        // Older metadata blobs carry no watermark: the floor is unknown, but
        // the ring still replaced one — an unresolved cursor is a gap here.
        let log = SessionEventLog::restored(4, None);
        log.push(frame("2-0-0"));
        let replay = log.replay(Some("1-0-7"));
        assert!(replay.truncated);
        assert_eq!(ids(&replay), vec!["2-0-0"]);
    }

    #[test]
    fn restored_ring_without_a_cursor_starts_from_now_like_a_fresh_one() {
        let log = SessionEventLog::restored(4, Some(10));
        log.push(frame("2-1-10"));
        let replay = log.replay(None);
        assert!(replay.frames.is_empty());
        assert!(!replay.truncated);
        assert_eq!(replay.next_pos, 2);
    }

    #[test]
    fn restored_ring_live_pump_arithmetic_is_unaffected_by_the_seed() {
        let log = SessionEventLog::restored(2, Some(10));
        // A pump that subscribed at the seeded write position sees exactly
        // what is pushed afterwards, and nothing is flagged until eviction.
        let next = log.replay(None).next_pos;
        log.push(frame("2-1-10"));
        let drained = log.drain_from(next);
        assert_eq!(ids(&drained), vec!["2-1-10"]);
        assert!(!drained.truncated);

        log.push(frame("2-1-11"));
        log.push(frame("2-1-12"));
        let lagged = log.drain_from(drained.next_pos.saturating_sub(1));
        assert_eq!(ids(&lagged), vec!["2-1-11", "2-1-12"]);
        assert!(
            lagged.truncated,
            "the frame at the pump's position was evicted"
        );
    }
}
