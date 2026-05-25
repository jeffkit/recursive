# Goal 30 — OpenAI error messages include model name

## Why

When `OpenAiProvider` hits an HTTP 4xx/5xx, the error message
currently looks like:

```
llm error: HTTP 429 Too Many Requests: {"error":{"code":"1113",...}}
```

In a multi-provider rotation (DeepSeek + MiniMax + GLM), it's not
obvious *which* model the failure came from without grepping the
log for surrounding context. Goal-23's GLM-5.1 run hit a 429 and we
had to look 30 lines back to confirm it was GLM. Embed the model
name in the error string itself.

## Scope

Touches: `src/llm/openai.rs` only (plus tests in the same file).

1. In `OpenAiProvider::complete()` (or wherever the HTTP error
   bubble-up happens):
   - When constructing the `Error::Llm` (or whichever variant carries
     the HTTP failure), prefix the message with
     `format!("model={}: HTTP {} {}: {}", self.model, status, reason, body)`
     — or some equivalent that puts the model name first.
   - Apply this to **all** error sites in this file, not just the
     one closest to the request. That includes JSON parse failures
     (`"model={}: response not valid JSON: {}"`), missing-fields
     errors, and network errors from `reqwest` (`"model={}: network
     error: {}"`).

2. Tests in the same file:
   - **Test A**: build an `OpenAiProvider` with model="test-model"
     pointed at an invalid URL (`http://127.0.0.1:1`). Call
     `complete()`, expect an error whose `to_string()` contains
     `"model=test-model"`.
   - **Test B**: build an `OpenAiProvider` pointed at a tiny mock
     HTTP server (you can use the existing `MockProvider` test
     scaffolding *or* spawn a one-shot listener with
     `std::net::TcpListener::bind("127.0.0.1:0")` and serve a
     single `400 Bad Request` response). Assert the resulting
     error contains both `"model="` and `"400"`.

## Acceptance

- `cargo build` green.
- `cargo test` green (132 baseline + 2 new = 134).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- This is **scoped to one file** — `src/llm/openai.rs`. Don't touch
  `src/llm/mod.rs`, `src/error.rs`, or the `MockProvider`.
- The simplest implementation is often: factor a helper
  `fn make_err(&self, ctx: &str) -> Error { Error::Llm(format!(
  "model={}: {}", self.model, ctx)) }` and use it consistently.
- If Test B turns out to need extra test-only deps to build a mock
  HTTP server (e.g. `httpmock`), prefer Test A alone — it's still
  meaningful coverage. Don't add a new Cargo.toml dependency for
  this goal.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
