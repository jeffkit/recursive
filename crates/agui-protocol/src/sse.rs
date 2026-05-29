//! SSE (Server-Sent Events) frame parser.
//!
//! Pure transport-independent decoder: callers feed in byte chunks
//! as they arrive over whatever transport, and get back fully
//! decoded [`Event`]s. We do **not** speak HTTP here — that lives in
//! `agui-client`.
//!
//! ## Wire format we accept
//!
//! Per the SSE spec (and what AG-UI servers actually send):
//!
//! - A *frame* (one event) is terminated by a blank line, i.e. either
//!   `\n\n` or `\r\n\r\n`. Frames containing only blank/comment lines
//!   are heartbeat keep-alives and produce no event.
//! - Within a frame, lines beginning with `:` are comments — skipped.
//! - Lines beginning with `data:` carry the payload. Multiple
//!   `data:` lines in one frame are concatenated with `\n` between
//!   them (per SSE spec).
//! - `event:` and `id:` lines are recognised but currently ignored —
//!   AG-UI puts the discriminator inside the JSON itself.
//!
//! ## UTF-8 safety
//!
//! Chunks may split a multi-byte UTF-8 codepoint. We buffer raw bytes
//! and only decode complete frames (whose boundary is the ASCII
//! sequence `\n\n` / `\r\n\r\n`, which can never occur inside a
//! UTF-8 continuation byte), so partial codepoints are preserved
//! across `feed` calls.
//!
//! ## Error tolerance
//!
//! A frame whose `data:` payload doesn't parse as a valid AG-UI
//! [`Event`] is logged via [`tracing::warn`] and dropped; subsequent
//! frames continue to parse normally. This matches the SSE design
//! intent that one bad event must not poison the stream.

use crate::events::Event;

/// Stateful SSE → [`Event`] decoder.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Raw byte buffer. Holds whatever bytes haven't yet ended a frame.
    buf: Vec<u8>,
}

impl SseParser {
    /// Construct an empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes from the transport. Returns any [`Event`]s
    /// that are now fully decoded. Trailing partial data (incomplete
    /// frame, or a multi-byte codepoint split across reads) stays in
    /// the internal buffer for the next call.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(chunk);

        let mut events = Vec::new();

        // Pull off as many complete frames as we can.
        while let Some((frame_bytes, advance)) = next_frame(&self.buf) {
            // Decode the frame. SSE is text-only — non-UTF-8 frames
            // are malformed; drop them with a warning rather than
            // crashing the stream.
            match std::str::from_utf8(frame_bytes) {
                Ok(frame_str) => {
                    if let Some(ev) = parse_frame(frame_str) {
                        events.push(ev);
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "SSE frame contained invalid UTF-8; dropping",
                    );
                }
            }
            // Drop the consumed bytes (frame body + delimiter).
            self.buf.drain(..advance);
        }

        events
    }
}

/// Locate the next complete frame in `buf`.
///
/// Returns `Some((frame_body_bytes, total_bytes_consumed))` when a
/// blank-line delimiter (`\n\n` or `\r\n\r\n`) is found, otherwise
/// `None`. `frame_body_bytes` excludes the delimiter; the caller
/// drains `total_bytes_consumed` from the front of the buffer.
fn next_frame(buf: &[u8]) -> Option<(&[u8], usize)> {
    // Walk bytes looking for the earliest `\n\n` or `\r\n\r\n`.
    // Because both sequences only contain ASCII (0x0A/0x0D), they
    // can never overlap with a UTF-8 continuation byte (0x80..=0xBF),
    // so byte-level scanning is safe even on partial codepoints.
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == b'\n' {
            // `\n\n`
            if i + 1 < buf.len() && buf[i + 1] == b'\n' {
                return Some((&buf[..i], i + 2));
            }
            // `\n\r\n` (rare but legal — first newline ended a line,
            // the next CRLF is a blank line).
            if i + 2 < buf.len() && buf[i + 1] == b'\r' && buf[i + 2] == b'\n' {
                return Some((&buf[..i], i + 3));
            }
        }
        if buf[i] == b'\r'
            && i + 3 < buf.len()
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            // `\r\n\r\n`
            return Some((&buf[..i], i + 4));
        }
        i += 1;
    }
    None
}

/// Decode one frame (already known to be valid UTF-8, delimiter
/// stripped) into an [`Event`], or `None` if the frame is a
/// heartbeat / comment-only / unparseable.
fn parse_frame(frame: &str) -> Option<Event> {
    // Per SSE: split on `\n` (and tolerate `\r\n`); each line is
    // either `field: value` or a comment / blank.
    let mut data_parts: Vec<&str> = Vec::new();

    for raw_line in frame.split('\n') {
        // Tolerate CRLF line endings inside the frame.
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        if line.is_empty() {
            continue;
        }
        if line.starts_with(':') {
            // Comment / keep-alive marker.
            continue;
        }

        // Field parsing per SSE: split at the first `:`, strip an
        // optional single leading space from the value.
        let (field, value) = match line.find(':') {
            Some(idx) => {
                let (f, v) = line.split_at(idx);
                let v = &v[1..]; // skip the `:`
                let v = v.strip_prefix(' ').unwrap_or(v);
                (f, v)
            }
            None => {
                // SSE: a line without `:` is a field name with empty value.
                (line, "")
            }
        };

        match field {
            "data" => data_parts.push(value),
            "event" | "id" | "retry" => {
                // Recognised but ignored — AG-UI puts its discriminator
                // inside the JSON payload.
            }
            other => {
                tracing::debug!(field = other, "ignoring unknown SSE field");
            }
        }
    }

    if data_parts.is_empty() {
        // Heartbeat / comment-only frame.
        return None;
    }

    // Per SSE: multi-line `data:` becomes one string with `\n` separators.
    let payload = data_parts.join("\n");

    match serde_json::from_str::<Event>(&payload) {
        Ok(ev) => Some(ev),
        Err(err) => {
            tracing::warn!(
                error = %err,
                payload = %payload,
                "failed to parse AG-UI event JSON; dropping frame",
            );
            None
        }
    }
}
