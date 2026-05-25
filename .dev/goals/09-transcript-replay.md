# Goal 09 — Transcript replay subcommand

## What

Add a new CLI subcommand `recursive replay <transcript-file>` that
loads a previously persisted `TranscriptFile` (from goal 08's
`--transcript-out`) and pretty-prints it to stdout. No LLM calls, no
side effects — just a human-readable view of a past run.

## Why

Goal 08 made it possible to *save* a transcript. The natural next
step is being able to *read* one. Right now the only way to inspect
a saved JSON is `cat … | jq`, which is fine for machines but
unreadable when you have 30 messages with multi-line bodies.

A pretty-printed `replay` lets a developer:
- Compare two runs of the same goal across providers.
- Audit a long session without re-running it.
- Build muscle for future replay-with-different-provider work (out
  of scope here, but persistence + reader is the foundation).

## Scope (do exactly this, no more)

### 1. `src/transcript.rs`

Add a method on `TranscriptFile`:

```rust
impl TranscriptFile {
    /// Render the transcript as a human-readable string suitable for
    /// piping to a pager.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("=== transcript ({} messages) ===\n", self.messages.len()));
        out.push_str(&format!(
            "saved_at: {}\nsteps: {}\nmodel: {}\n\n",
            self.meta.saved_at,
            self.meta.steps,
            self.meta.model.as_deref().unwrap_or("(unknown)"),
        ));
        for (i, msg) in self.messages.iter().enumerate() {
            out.push_str(&format!("--- [{}] {:?} ---\n", i, msg.role));
            if !msg.content.is_empty() {
                out.push_str(&msg.content);
                if !msg.content.ends_with('\n') {
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        out
    }
}
```

Use `{:?}` on `msg.role` — `Role` already derives `Debug` (it's an
enum, the variant name is readable). Don't add any new derives.

### 2. New CLI subcommand

In `src/main.rs`, add a new variant to the `Cmd` enum:

```rust
/// Pretty-print a previously saved transcript JSON file.
Replay {
    /// Path to the transcript JSON file (as written by --transcript-out).
    path: PathBuf,
},
```

And handle it in `main`'s match:

```rust
Cmd::Replay { path } => {
    let file = recursive::TranscriptFile::read_from(&path)?;
    print!("{}", file.pretty());
    Ok(())
}
```

Place it after the existing arms, ordering doesn't matter for clap.

### 3. Tests

Add to `src/transcript.rs`:

1. `pretty_includes_header_and_meta` — build a `TranscriptFile` with
   one message, call `pretty()`, assert the output contains
   `"=== transcript (1 messages) ==="`, the timestamp, and `steps: N`.
2. `pretty_renders_each_message_with_index_and_role` — three messages
   (system, user, assistant), assert `[0]`, `[1]`, `[2]`, and each
   role name appears in the output.
3. `pretty_handles_empty_content_gracefully` — a message with empty
   `content` doesn't blow up; the section header still renders.

These are string-content assertions, no IO needed.

## Out of scope

- Replaying *with a new provider* (loading the messages and
  re-prompting an LLM from a chosen point). That's a much bigger
  feature; pretty-printing is the prerequisite.
- Colourised / paginated output. Plain text only.
- Filtering by role or step range. Not needed yet.
- Re-validating tool call structure. The JSON either round-trips or
  it doesn't.
- Touching the existing `Cmd::Run` or `Cmd::Repl` arms.

## Definition of done

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` all green.
- `recursive replay path/to/saved.json` prints something that ends
  in a final blank line and contains every message.
- 3 new tests pass; existing tests untouched.
- No new dependencies.

## Notes for the agent

- `TranscriptFile::read_from` already exists from goal 08.
- This is a small, additive change. Use `apply_patch` for
  `src/transcript.rs` and `src/main.rs`. Both files are short
  enough that anchors will be easy to find.
- Don't try to be clever about formatting — the tests check for
  fixed substrings, so over-formatting will just fail them.
