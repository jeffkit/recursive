- [AC07] `timeout 2 target/debug/recursive acp < /dev/null 2>/dev/null` exits with code 0, contract requires exit code 124 (timeout kill). Process exits immediately on stdin EOF instead of blocking and waiting for input. The stdio read loop likely treats EOF as a graceful shutdown signal rather than blocking indefinitely.
  - file: src/acp/server.rs:1
  - repro: timeout 2 target/debug/recursive acp < /dev/null 2>/dev/null; echo $?  # returns 0, expected 124

- [AC08] The initialized notification is emitted as `{"jsonrpc":"2.0","method":"initialized","params":null}` but the contract requires an exact match of `{"jsonrpc":"2.0","method":"initialized"}` (no `params` field at all). The serialization includes `"params":null` — likely from a serde `Option::None` field that should be `#[serde(skip_serializing_if = "Option::is_none")]` or omitted entirely.
  - file: src/acp/server.rs
  - repro: echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}' | target/debug/recursive acp 2>/dev/null | head -2 | tail -1  # outputs {"jsonrpc":"2.0","method":"initialized","params":null}
