# agui-protocol

AG-UI protocol types, serde definitions, and an SSE parser. Pure data
types — no transport, no UI. Use this if you want to write a client
or server that speaks the [AG-UI protocol] and don't want to bring in
the upstream Rust SDK or invent the wire format yourself.

[AG-UI protocol]: https://docs.ag-ui.com/

## What it covers

- The 16 standard event variants (lifecycle, text-message, tool-call,
  state, messages-snapshot) plus `Custom` and `Raw` for extension
  points.
- `RunAgentInput`, `Message`, `Tool`, `ContextItem`, `Resume` — the
  request side of the protocol.
- `SseParser`: robust against partial chunks across reads (including
  mid-UTF-8 codepoint), multi-line `data:` per spec, comment lines,
  blank-line keep-alives, and malformed-JSON recovery.

## Why a separate crate (vs. the upstream `ag-ui-core`)

The official AG-UI repo has community-maintained Rust crates under
`sdks/community/rust/crates/` (see [ag-ui-protocol/ag-ui]). At the
time of writing those crates are **not published to crates.io** and
their API is web-first.

We chose to keep `agui-protocol` self-contained for three reasons:

1. **Local-agent extensions.** This crate is shaped to carry the
   `agui-tui/permission_request`, `agui-tui/checkpoint_post`,
   `agui-tui/heartbeat`, and `agui-tui/file_artifact` Custom events
   that local agents need (the upstream spec doesn't standardise
   these yet).
2. **Crates.io publishability.** We can release without git-dep
   chains.
3. **Robustness around SSE chunking.** Our parser is hardened for
   partial-codepoint splits and malformed frames, which matters for
   real network deployments.

If the upstream community crates eventually land on crates.io and
stabilise, this crate is small enough to either swap out for it or
stay as a thin protocol-extensions layer on top.

[ag-ui-protocol/ag-ui]: https://github.com/ag-ui-protocol/ag-ui

## License

MIT.
