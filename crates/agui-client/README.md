# agui-client

HTTP/SSE transport for AG-UI agents. Built on top of [`agui-protocol`].

## What it does

```rust
use agui_client::{AguiClient, RunAgentInput};
use url::Url;

let client = AguiClient::new("https://my-agent.example.com/agui".parse()?);
let mut rx = client
    .run(RunAgentInput { /* … */ })
    .await?;

while let Some(event) = rx.recv().await {
    // handle Event…
}
```

`AguiClient::run` POSTs the input as JSON, opens an SSE stream against
the response, and forwards every parsed `Event` into a tokio
`mpsc::UnboundedReceiver`. It tolerates partial chunks across reads
and surfaces 4xx / 5xx as `ClientError::HttpStatus`.

## Why not the upstream `ag-ui-client`?

The official AG-UI repo contains a community-maintained
`sdks/community/rust/crates/ag-ui-client`, but it isn't published to
crates.io and its API is shaped for the web case. We use this crate
because:

- It can be published from this repo without git-dep chains.
- The mpsc-based event delivery fits ratatui apps cleanly.
- It builds on `agui-protocol`'s SSE parser (see that crate's README
  for the rationale).

[`agui-protocol`]: ../agui-protocol/README.md

## License

MIT.
