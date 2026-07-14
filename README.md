# Recursive

A Rust coding-agent platform: a small ReAct kernel plus the surrounding
HTTP API, MCP, multi-agent orchestration, and TUI that turn it into a
full development tool.

[![CI](https://github.com/jeffkit/recursive/actions/workflows/ci.yml/badge.svg)](https://github.com/jeffkit/recursive/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/recursive-agent.svg)](https://crates.io/crates/recursive-agent)
[![Docs.rs](https://docs.rs/recursive-agent/badge.svg)](https://docs.rs/recursive-agent)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

At its core Recursive is a ReAct loop that wires together:

- an **LLM provider** (OpenAI-compatible HTTP by default; works with OpenAI,
  GLM/Zhipu, DeepSeek, Moonshot, MiniMax, Together, Ollama, vLLM, …; plus a
  native Anthropic adapter)
- a **tool registry** (`Read`, `Write`, `Edit`, `Glob`, `Bash`,
  `WebFetch`, `WebSearch`, plan-mode, checkpoints, todo, …; plus
  coordinator-only `team_*` / `task_*` tools and a deferred-tool loader)
- a **transcript** plus a `StepEvent` stream you can observe

Around that kernel the platform adds opt-in surfaces:

- **HTTP API** — axum-based REST + SSE server with sessions, rate-limiting,
  JWT/API-key auth, OpenAPI spec (feature `http`)
- **MCP** — both as a client (consume external MCP servers) and a server
  (expose Recursive's tools to other MCP-aware agents) (feature `mcp`)
- **TUI** — ratatui-based interactive client with streaming tool indicators,
  plan mode, and command palette (`crates/recursive-tui`)
- **Multi-agent** — agent pool, shared memory, messaging bus, plan-mode
  coordination (feature `coordinator-mode`)
- **Cloud runtime** — Redis session store, S3 transcript storage, Docker /
  E2B sandboxes (features `cloud-runtime` / `e2b-sandbox`)
- **Vector memory** — sqlite-vec + OpenAI embeddings for episodic recall
  (feature `vector-memory`)
- **Loop mode** — `recursive loop` for self-scheduling autonomous agent runs

Embedding just the kernel (no HTTP / TUI / cloud) is supported via
`--no-default-features`.

> The crate is published as `recursive-agent` because the name `recursive`
> was taken on crates.io. The installed binary is still called `recursive`,
> and the library is imported as `use recursive::*;`.

## At a glance

Wire up an OpenAI-compatible LLM, register some tools, and drive the
agent loop. This is the v0.7 surface — the legacy `Agent` type from
v0.5 was split into `AgentKernel` (stateless) and `AgentRuntime`
(stateful wrapper) during Goal 219.

```rust
use recursive::llm::OpenAiProvider;
use recursive::runtime::AgentRuntime;
use recursive::tools::{ReadFile, RunShell, ToolRegistry, WriteFile};
use std::sync::Arc;

# async fn run() -> anyhow::Result<()> {
let llm = OpenAiProvider::new(
    "https://api.openai.com/v1",
    std::env::var("OPENAI_API_KEY")?,
    "gpt-4o-mini",
)?;

let tools = ToolRegistry::local()
    .register(Arc::new(ReadFile::new(".")))
    .register(Arc::new(WriteFile::new(".")))
    .register(Arc::new(RunShell::new(".")));

let mut runtime = AgentRuntime::builder()
    .llm(Arc::new(llm))
    .tools(tools)
    .max_steps(20)
    .system_prompt("You are a helpful coding assistant.")
    .build()?;

let outcome = runtime
    .run("list the files in src and summarise them")
    .await?;

println!("{}", outcome.final_text.unwrap_or_default());
# Ok(()) }
```

Run it with no API key by swapping `OpenAiProvider` for the
scriptable `MockProvider` — see `examples/basic.rs` and
`examples/with_tools.rs`.

## Design

The kernel has five concepts, each independently testable:

| Concept | Where | Role |
|---|---|---|
| `Message` | `src/message.rs` | The only data primitive: chat messages with optional tool calls. |
| `ChatProvider` | `src/llm/` | Trait for model backends. Adapters: HTTP (OpenAI-compatible), Anthropic, Mock. |
| `Tool` + `ToolRegistry` | `src/tools/` | Trait for side effects the model can request. Sandboxed to a workspace. |
| `AgentKernel` | `src/kernel.rs` | Stateless single-turn executor. Receives a `TurnContext`, returns a `TurnOutcome`. |
| `AgentRuntime` | `src/runtime.rs` | Stateful wrapper. Owns the transcript, message queue, compaction, and cross-turn state. |

The actual ReAct step loop lives in [`src/run_core.rs::RunCore::run_inner`](src/run_core.rs). The kernel/wrapper split was introduced after the legacy `Agent` / `StepEvent` types were removed (Goal 219). For a deeper tour, see [`docs/architecture/agent-loop.md`](docs/architecture/agent-loop.md).

### Orthogonality

- **New tool?** Implement `Tool`, register it. No kernel/runtime changes.
- **New model backend?** Implement `ChatProvider`. No tool/kernel changes.
- **New UI / observer?** Subscribe to the `AgentEvent` stream via `EventSink`. No loop changes.
- **New finish reason?** Add a variant to `FinishReason`. Callers can match if they care.

### Safety primitives baked in

- Every fs / shell tool resolves paths through `tools::resolve_within`, which
  rejects anything escaping the configured workspace root.
- `run_shell` enforces a configurable timeout and caps captured output.
- Agent loop respects a step budget (`max_steps`) and emits
  `FinishReason::BudgetExceeded` rather than looping forever.

## Installation

### Homebrew (macOS Apple Silicon)

```bash
brew install jeffkit/tap/recursive
```

Intel Macs: `brew install` will print a message pointing at
`cargo install recursive-cli --locked` — see the formula for context.

### Pre-built binaries (Linux, macOS, Windows)

Grab the asset matching your platform from
<https://github.com/jeffkit/recursive/releases/latest>, extract, and put
`recursive` on your `$PATH`.

### From source

```bash
cargo install --path .   # or, once published: cargo install recursive-cli
```

```bash
# one-off goal
recursive run "list files in src and summarise the kernel"

# interactive REPL (one goal per line, :q to exit)
recursive repl

# loop mode — agent self-schedules wakeups
recursive loop "monitor src/ for changes and report"

# HTTP API server
recursive http --addr 127.0.0.1:3000

# Terminal UI (connects to HTTP server)
cargo run -p recursive-tui

# inspect what tools are registered (no API key needed)
recursive tools
```

### Configuration

Anything OpenAI-compatible works. Override via env vars (or CLI flags):

| Env | Default | Purpose |
|---|---|---|
| `RECURSIVE_API_BASE` | `https://api.openai.com/v1` | Chat-completions endpoint |
| `RECURSIVE_API_KEY` | _(required)_ | Bearer token |
| `RECURSIVE_MODEL` | `gpt-4o-mini` | Model name |
| `RECURSIVE_MAX_STEPS` | `32` | Loop budget |
| `RECURSIVE_TEMPERATURE` | `0.2` | Sampling temperature |
| `RECURSIVE_WORKSPACE` | cwd | Root all fs/shell tools are sandboxed to |
| `RECURSIVE_SYSTEM_PROMPT_FILE` | _(built-in)_ | Path to a system prompt to load |

Example with GLM (Zhipu):

```bash
export RECURSIVE_API_BASE="https://open.bigmodel.cn/api/paas/v4"
export RECURSIVE_API_KEY="$GLM_API_KEY"
export RECURSIVE_MODEL="glm-4-flash"
recursive run "create hello.txt and read it back"
```

Example with a local Ollama:

```bash
export RECURSIVE_API_BASE="http://localhost:11434/v1"
export RECURSIVE_API_KEY="ollama"   # ollama ignores it but the field is required
export RECURSIVE_MODEL="qwen2.5-coder"
recursive run "explain the repo layout"
```

#### Adding a custom provider

The bundled catalog (see `providers.toml`) ships presets for OpenAI,
Anthropic, DeepSeek, MiniMax, GLM/Zhipu, Moonshot, Doubao, DashScope,
Hunyuan, StepFun, Gemini, Groq, Mistral, xAI, and a local Ollama. If
the vendor you want isn't there, you have two paths.

**Interactive wizard** — run `recursive init`, pick `0` for "custom API
base", then enter the URL, the model name, and the API key. The wizard
will offer to save the result as a reusable preset under
`~/.recursive/providers.d/<your-id>.toml` so the next run can call it
by id.

**Hand-written preset** — drop a file into
`~/.recursive/providers.d/`:

```toml
# ~/.recursive/providers.d/myvendor.toml

[[providers]]
id = "myvendor"                      # the id you'll pass to --provider
name = "My Vendor"                   # shown in `recursive init` / `providers list`
provider_type = "openai"             # "openai" or "anthropic"
api_base = "https://api.myvendor.com/v1"
default_model = "myvendor-1"
key_env = "MYVENDOR_API_KEY"        # the env var the runtime reads at start
key_url = "https://myvendor.com/keys"
models = [
  { name = "myvendor-1", context_window = 32_000 },
  { name = "myvendor-2-mini", context_window = 16_000,
    pricing = { input_per_million = 0.10, output_per_million = 0.30 } },
]
mainland_accessible = false
```

Verified on the next `recursive` launch — `recursive providers list`
will show it, and `recursive init` will offer it in the picker. To
override a bundled preset's models or pricing (e.g. to ride out a
catalog drift until the next release), give your file the same `id`
as the bundled one: the bundled entry stays visible but your file's
`models[]` and `pricing` win.

**Remote catalog** — `recursive providers update` pulls the latest
catalog from the upstream JSON the project maintains
(`RECURSIVE_PROVIDERS_URL` overrides the URL; defaults to a GitHub
raw URL). The wizard prompts to refresh on every run; one-shot
commands read whatever is already cached (TTL 7 days). Use
`recursive providers update` explicitly when you want the latest
without waiting for the TTL to expire.

`recursive providers list` / `status` cover the operations side of
catalog management — see `--help` for the available sub-commands.

## Docker / Cloud Deployment

### Single-container (local mode)

```bash
docker build -t recursive:dev --target runtime .
docker run -p 3000:3000 \
  -e RECURSIVE_API_KEY="$OPENAI_API_KEY" \
  -e RECURSIVE_API_BASE="https://api.openai.com/v1" \
  -e RECURSIVE_MODEL="gpt-4o-mini" \
  recursive:dev
```

The image defaults to `recursive http --addr 0.0.0.0:3000` and exposes `/health` for probes.

> **⚠️ Auth required**: The HTTP server now rejects requests with 503
> unless `RECURSIVE_HTTP_AUTH_KEYS` or `RECURSIVE_HTTP_AUTH_JWT_SECRET` is
> configured. For local dev, set `RECURSIVE_HTTP_AUTH_INSECURE_OK=1` as a
> debug escape hatch — never use this in production.

### Full cloud stack (Redis + S3)

Use the bundled `docker-compose.yml` to spin up Redis (session hot-state) and
LocalStack S3 (transcript persistence) locally:

```bash
cp .env.example .env          # fill in RECURSIVE_API_KEY
docker compose up
```

Then talk to the agent over HTTP:

```bash
# create a session
SESSION=$(curl -sX POST http://localhost:3000/sessions \
  -H 'Content-Type: application/json' \
  -d '{"system_prompt":"You are a helpful assistant."}' | jq -r .session_id)

# send a message
curl -X POST http://localhost:3000/sessions/$SESSION/run \
  -H 'Content-Type: application/json' \
  -d '{"message":"List the files in /workspace"}'
```

### Environment variables — full reference

#### LLM provider

| Env | Default | Purpose |
|-----|---------|---------|
| `RECURSIVE_API_BASE` | `https://api.openai.com/v1` | Chat-completions endpoint |
| `RECURSIVE_API_KEY` | _(required)_ | Bearer token |
| `RECURSIVE_MODEL` | `gpt-4o-mini` | Model name |
| `RECURSIVE_PROVIDER_TYPE` | `openai` | Protocol: `openai` or `anthropic` |
| `RECURSIVE_MAX_STEPS` | `32` | Max tool-call loop iterations per run |
| `RECURSIVE_TEMPERATURE` | `0.2` | Sampling temperature |
| `RECURSIVE_SYSTEM_PROMPT_FILE` | _(built-in)_ | Path to a custom system-prompt file |
| `RECURSIVE_WORKSPACE` | cwd | Filesystem sandbox root |

#### HTTP server

| Env | Default | Purpose |
|-----|---------|---------|
| `RECURSIVE_HTTP_ADDR` | `0.0.0.0:3000` | Bind address |
| `RECURSIVE_HTTP_AUTH_KEYS` | _(required for prod)_ | Comma-separated `X-API-Key` allowlist |
| `RECURSIVE_HTTP_AUTH_JWT_SECRET` | _(none)_ | HMAC secret for JWT bearer-token auth |
| `RECURSIVE_HTTP_AUTH_JWT_AUDIENCE` | _(none)_ | Optional `aud` claim for JWT validation |
| `RECURSIVE_HTTP_AUTH_INSECURE_OK` | _(none)_ | Set to `1` to bypass auth (local dev ONLY) |

#### Cloud storage — Redis (session hot-state)

Requires the `cloud-runtime` feature flag (`--features cloud-runtime`).

| Env | Default | Purpose |
|-----|---------|---------|
| `RECURSIVE_REDIS_URL` | _(disabled)_ | Redis connection URL e.g. `redis://host:6379` |
| `RECURSIVE_REDIS_KEY_PREFIX` | `recursive:` | Key namespace prefix |
| `RECURSIVE_REDIS_SESSION_TTL_SECS` | `7200` | Session expiry (2 h) |

#### Cloud storage — S3 (transcript + memory)

Requires the `cloud-runtime` feature flag.

| Env | Default | Purpose |
|-----|---------|---------|
| `RECURSIVE_S3_BUCKET` | _(disabled)_ | S3 bucket name |
| `RECURSIVE_S3_PREFIX` | `recursive` | Object key prefix |
| `RECURSIVE_S3_TENANT_ID` | `default` | Tenant namespace inside the bucket |
| `AWS_ACCESS_KEY_ID` | _(from SDK)_ | AWS credential |
| `AWS_SECRET_ACCESS_KEY` | _(from SDK)_ | AWS credential |
| `AWS_DEFAULT_REGION` | `us-east-1` | AWS region |
| `AWS_ENDPOINT_URL` | _(AWS)_ | Override for LocalStack / MinIO |

#### Sandbox

| Env | Default | Purpose |
|-----|---------|---------|
| `RECURSIVE_SANDBOX_MODE` | `local` | `local` / `policy` / `docker` / `e2b` |
| `RECURSIVE_E2B_API_KEY` | _(required for e2b mode)_ | E2B API key |
| `RECURSIVE_E2B_TEMPLATE` | `base` | E2B sandbox template ID |
| `RECURSIVE_E2B_TIMEOUT_SECS` | `300` | Sandbox timeout in seconds |
| `RECURSIVE_SHELL_TIMEOUT_SECS` | `30` | Per-command shell timeout |

### Local vs cloud mode — cheatsheet

| Concern | Local (default) | Cloud (`cloud-runtime` feature) |
|---------|-----------------|----------------------------------|
| Transcript persistence | Local JSONL (`~/.recursive/...`) | S3 via `S3StorageBackend` |
| Session hot-state | In-memory (`NoopSessionStore`) | Redis via `RedisSessionStore` |
| Tool execution | Host shell | Docker (L2) or E2B microVM (L3) |
| Horizontal scaling | Single process | Stateless HTTP pods + shared Redis/S3 |
| Resume across restarts | Via `--session` flag | Automatic via `restore_from_storage()` |

## Library API

`recursive` is also a library — embed the loop in your own program if the CLI
isn't the right shell for your use case. See the example above; the public
surface lives in `src/lib.rs`.

## Testing

```bash
cargo test --workspace
```

540+ tests covering:

- Agent loop: termination, tool dispatch, error recovery, step budget,
  event stream order.
- Tool registry: dispatch, unknown-tool error, path sandboxing.
- Filesystem tools: round-trip, parent-dir creation, sort order, escape
  rejection.
- Shell tool: success / non-zero status / timeout.
- HTTP provider: request shape (with and without tools), response parsing
  (plain text / tool-call), tool-call argument round-trip.
- HTTP API: health, tools, run, sessions CRUD, SSE streaming, OpenAPI spec.
- TUI: app state, key handling, message styling, scroll, plan mode.
- Multi-Agent: pool, roles, shared memory, messaging bus, pipeline, orchestrator.
- End-to-end smoke (`tests/smoke.rs`): scripted `MockProvider` driving real
  filesystem tools.

## Python SDK

```bash
cd sdk/python && pip install -e .
```

```python
from recursive_client import RecursiveClient

client = RecursiveClient("http://127.0.0.1:3000")
print(client.health())  # "ok"
result = client.run("list files in src/")
print(result.finish_reason)
```

## TUI

The terminal UI is in `crates/recursive-tui/`. For an experience-level
comparison against fake-cc (Claude Code-style baseline), see
[docs/tui-fake-cc-gap.md](docs/tui-fake-cc-gap.md).

## License

MIT — see [LICENSE](LICENSE).
