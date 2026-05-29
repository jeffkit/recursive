# Goal 143 — Generic AG-UI Terminal Client (`agui-tui`)

> **Roadmap**: New ecosystem track. Sits between Phase 15 (observability)
> and Phase 19 (distribution): exposes a transport-standard way for any
> AG-UI–compatible agent to drive a terminal client, and brings recursive
> into that ecosystem on the **server** side too.
> **Design principle check**: Three new crates, all independent of the
> recursive kernel.
> - `agui-protocol`: pure data types + SSE parser, no transport, no UI.
> - `agui-client`: HTTP/SSE transport on top of `agui-protocol`.
> - `agui-tui`: ratatui binary on top of `agui-client`. Independent of
>   `recursive-agent`. Connects to *any* AG-UI server.
>
> The recursive HTTP server gains an opt-in AG-UI event stream so it
> can be driven by `agui-tui` (or any other AG-UI client like the
> CopilotKit web playground).

## Why

AG-UI is becoming the lingua franca for agent ↔ UI integration. Today
the only ready-to-use clients are CopilotKit (web) and the AG-UI Dojo
(also web). The official "terminal" support is a 50-line `readline`
tutorial — there's no real TUI.

Two concrete payoffs:

1. **For users of any AG-UI agent**: `cargo install agui-tui` →
   `agui-tui http://my-agent.example.com` and you have a real
   terminal interface, including streaming text, tool-call inspection,
   and permission prompts. No web stack required.

2. **For recursive**: by emitting AG-UI events from our HTTP server,
   we plug recursive into the AG-UI client ecosystem (web Dojo,
   CopilotKit playground, this new TUI) without writing any of those
   UIs ourselves.

## What AG-UI lacks for local agents (and how we cover it)

The 16 standard events focus on web-style streaming. Local agent
flows need a few things the spec doesn't standardise:

| Need | Coverage |
|---|---|
| Tool-call permission gate (Y/N before tool runs) | `Custom` event with `name = "agui-tui/permission_request"`; client replies via `resume` array on the next run |
| Checkpoint id surfaced to UI (so user can rewind) | `Custom` event `name = "agui-tui/checkpoint_post"` with `{turn, post_id}` |
| Heartbeat for long-running tools | `Custom` event `name = "agui-tui/heartbeat"` with elapsed ms |
| File-result hint (this string is a path, the TUI may open it) | `Custom` event `name = "agui-tui/file_artifact"` with absolute path |

These are intentionally namespaced under `agui-tui/`. If they prove
out we can propose them to the AG-UI standard later; until then no
other implementation has to know about them.

## Crate layout

```
crates/
├── agui-protocol/       (lib)
│   ├── events.rs        ← 16 standard event variants + Custom
│   ├── input.rs         ← RunAgentInput, Resume, Tool, Context
│   ├── sse.rs           ← `data: <json>\n\n` parser → Event stream
│   └── lib.rs
├── agui-client/         (lib)
│   ├── http.rs          ← reqwest stream → agui_protocol::Event
│   └── lib.rs
└── agui-tui/            (bin = `agui-tui`)
    ├── app.rs           ← ratatui state machine
    ├── ui.rs            ← layout: messages | tool-calls | state
    ├── input.rs         ← user prompt + permission Y/N keybindings
    └── main.rs
```

`crates/recursive-tui` (the existing TUI from g97-101, currently being
refactored) is not modified by this goal.

## `agui-protocol` API sketch

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    RunStarted(RunStarted),
    RunFinished(RunFinished),
    RunError(RunError),
    StepStarted(StepStarted),
    StepFinished(StepFinished),
    TextMessageStart(TextMessageStart),
    TextMessageContent(TextMessageContent),
    TextMessageEnd(TextMessageEnd),
    TextMessageChunk(TextMessageChunk),
    ToolCallStart(ToolCallStart),
    ToolCallArgs(ToolCallArgs),
    ToolCallEnd(ToolCallEnd),
    ToolCallResult(ToolCallResult),
    StateSnapshot(StateSnapshot),
    StateDelta(StateDelta),
    MessagesSnapshot(MessagesSnapshot),
    Custom(Custom),
    Raw(Raw),
}

pub struct RunAgentInput {
    pub thread_id: String,
    pub run_id: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub context: Vec<ContextItem>,
    pub resume: Option<Vec<Resume>>,
    pub state: Option<serde_json::Value>,
}

pub struct SseParser { buf: String }
impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Event>;
}
```

Field naming uses serde camelCase to match the AG-UI wire format
(`runId`, `messageId`, `toolCallName`, `delta`, `parentMessageId`...).

## `agui-client` API sketch

```rust
pub struct AguiClient {
    http: reqwest::Client,
    endpoint: Url,
    headers: HeaderMap,
}

impl AguiClient {
    pub fn new(endpoint: Url) -> Self;
    pub fn with_header(self, k: &str, v: &str) -> Self;

    /// Start a run. Returns an mpsc receiver streaming Events as they
    /// arrive over SSE. Sender side is driven by the HTTP task; the
    /// stream ends when the server closes the connection.
    pub async fn run(
        &self,
        input: RunAgentInput,
    ) -> Result<mpsc::UnboundedReceiver<Event>>;
}
```

The client owns no agent-specific knowledge. It posts to whatever
URL it was given and parses the SSE stream.

## `agui-tui` UX

```
┌──────────────────────────────────────────┬──────────────────┐
│ #1 user: refactor the auth middleware    │ State            │
│                                          │ ─────            │
│ #2 assistant: I'll start by reading...   │ thread_id 7c…    │
│   ⚙ read_file(src/auth.rs)               │ run_id   af…     │
│   ↪ <truncated 18 lines>                 │ steps    3       │
│   ⚙ apply_patch(...)                     │ tokens   1.2k    │
│   ↪ updated 1 hunk                       │                  │
│                                          │ Tools used       │
│ #3 assistant: Done. The middleware now…  │ • read_file ×2   │
│                                          │ • apply_patch ×1 │
├──────────────────────────────────────────┤                  │
│ ⚠ permission requested:                  │                  │
│   run_shell "cargo test"                 │                  │
│   [y] approve  [n] reject  [esc] later   │                  │
├──────────────────────────────────────────┴──────────────────┤
│ > _                                                          │
└──────────────────────────────────────────────────────────────┘
```

CLI:

```
agui-tui <endpoint-url>
agui-tui http://localhost:3000/agui
agui-tui --header 'Authorization: Bearer …' https://my-agent/run
```

Keybindings:
- `Enter` to send the current prompt
- `y` / `n` / `Esc` to respond to a permission request
- `Tab` to toggle focus between message pane and state pane
- `Ctrl-C` to quit (terminates the in-flight run gracefully)

## recursive-side integration

A new endpoint, opt-in via flag:

```
recursive http --agui  # adds POST /agui that streams AG-UI events
```

Or by always exposing it under `/agui` and leaving `/run` (the legacy
SSE shape) untouched. We'll pick whichever is simpler once we look
at the current `src/http.rs`.

The mapping from `AgentEvent` → AG-UI is straightforward:

| AgentEvent | AG-UI |
|---|---|
| TurnStarted | RunStarted |
| AssistantText (streaming) | TextMessageStart / Content / End |
| ToolCallRequested | ToolCallStart / Args / End |
| ToolCallCompleted | ToolCallResult |
| TurnFinished (success) | RunFinished |
| TurnFinished (error) | RunError |
| (g141) checkpoint id at turn end | Custom `agui-tui/checkpoint_post` |
| Permission hook fires before tool | Custom `agui-tui/permission_request` + RunFinished interrupt |

The permission flow leans on AG-UI's interrupt mechanism:

1. Server sees a permission_hook that wants user input.
2. Server emits the Custom permission_request event.
3. Server emits `RunFinished` with `outcome = { type: "interrupt", interrupts: [...] }` and pauses execution.
4. TUI shows the prompt, gets Y/N, sends a new `RunAgentInput` with `resume = [{ id: …, value: "approve" }]`.
5. Server resumes the paused run, runs (or aborts) the tool, continues streaming.

## Tests

`agui-protocol`:

- `sse_parser_splits_events_at_blank_line`
- `event_round_trip_camel_case` — every variant deserialises and
  serialises with the expected JSON keys.
- `text_message_chunk_auto_expands`
- `custom_event_preserves_unknown_fields`

`agui-client`:

- `client_streams_events_from_mock_server` — mockito or wiremock SSE
  responder; assert the receiver yields the events in order.
- `client_propagates_4xx_as_error`
- `client_handles_partial_chunks_across_reads`

`agui-tui`:

- Headless render tests (snapshot the buffer for a fixed event
  sequence). Use `ratatui::Backend::TestBackend`.
- Permission flow keybindings (simulate y/n/Esc).

E2E (`tests/agui_e2e.rs` in the recursive crate, gated on `feature = "http"`):

- Spin up `recursive http --agui` against a MockProvider.
- Drive `agui-client` against `localhost:<port>/agui`.
- Assert at least one TextMessageContent + RunFinished arrives.

## Acceptance

- `cargo build` green for the workspace (3 new crates + recursive).
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- `cargo install --path crates/agui-tui` produces a working binary
  that can connect to a recursive `--agui` server and to the AG-UI
  Dojo's mock server.
- README in each new crate explains its scope.

## Out of scope (defer)

- Generative UI rendering (HTML / React) inside the TUI. Generative
  UI is a web feature and won't fit a terminal cleanly. We skip the
  GenerativeUI events at first; the TUI ignores them with a one-line
  log.
- WebSocket transport. Stick with SSE for v1 — every AG-UI server
  speaks SSE.
- Theming, mouse support, or windowing. Default ratatui visuals are
  enough.
- Wide-character / RTL polish.
- Auth flows beyond static `--header` injection.
- Standardising the `agui-tui/*` Custom events into the AG-UI spec
  (separate effort once they're battle-tested).
