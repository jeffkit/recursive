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
- **Cloud runtime** — S3 transcript storage (wired into `recursive http`),
  Redis session store + Docker / E2B sandboxes (library API / features
  `cloud-runtime` / `e2b-sandbox`)
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
agent loop. This surface has been stable since v0.7 (the legacy `Agent`
type from v0.5 was split into `AgentKernel` (stateless) and `AgentRuntime`
(stateful wrapper) during Goal 219); it is the API shipped in v0.8.1.

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
scriptable `MockProvider` — see `examples/basic.rs` (requires the
`test-utils` feature: `cargo run --example basic --features test-utils`)
and `examples/with_tools.rs`.

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
| `RECURSIVE_MAX_STEPS` | `200` | Loop budget per turn. Issue #94: the default is now a conservative 200 instead of unlimited — set `0` to restore unbounded steps |
| `RECURSIVE_WALL_TIMEOUT_SECS` | `3600` | Wall-clock budget per turn in seconds; expiry finishes with `wall_clock_exceeded`. Issue #94: the default is now a conservative 1 h instead of unlimited — set `0` to restore unbounded runs |
| `RECURSIVE_MAX_BUDGET_USD` | _(unlimited)_ | Per-turn spend ceiling in USD. Issue #94: the turn stops with `budget_exceeded` at the first step boundary where its spend reaches the ceiling. Unpriced models degrade to a token ceiling at a pessimistic $10/M blend |
| `RECURSIVE_THINKING_BUDGET` | _(model default)_ | Anthropic extended-thinking budget (`thinking.budget_tokens`). Issue #94: actually sent on the wire; `0` disables thinking, unset leaves the model default |
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

#### Memory (optional)

`remember` / `recall` / `forget` persist notes to
`<workspace>/.recursive/memory.json` — written atomically, capped at
`RECURSIVE_MEMORY_MAX_NOTES` (default `1000`, `0` = unlimited) with the oldest
notes evicted first, and re-remembering identical text refreshes that note
instead of appending a duplicate.

Semantic recall (cosine search instead of substring matching) additionally
needs the `vector-memory` feature and an embeddings endpoint:

```bash
cargo build -p recursive-cli --features vector-memory
```

| Env | Fallback | Default | Purpose |
|---|---|---|---|
| `RECURSIVE_EMBEDDING_API_BASE` | `RECURSIVE_API_BASE` | `https://api.openai.com/v1` | Embeddings endpoint |
| `RECURSIVE_EMBEDDING_API_KEY` | `RECURSIVE_API_KEY` | _(none)_ | Embeddings bearer token |
| `RECURSIVE_EMBEDDING_MODEL` | — | `text-embedding-3-small` | Embedding model |

The dedicated vars win, so embeddings can point somewhere other than the chat
provider — needed when the chat provider is Anthropic or an in-house gateway
with no OpenAI-compatible credential to reuse. Reusing the shared
`RECURSIVE_API_KEY` for embeddings logs one warning at startup: a chat
credential is not necessarily an embeddings credential. Embedding requests are
bounded (5 s to connect, 30 s in total), so a dead endpoint costs one failed
request and then falls back to keyword recall rather than stalling the turn. The
index at `<workspace>/.recursive/memory_vectors.db` is created on the first
write, not when the workspace is opened. `recall` always unions the durable
`memory.json` hits with the semantic ones — so a note written before the index
existed stays reachable — and drops semantic hits below a cosine floor (every
query has a nearest neighbour, even an unrelated one). Without the feature or
without a key, `recall` degrades to the keyword path instead of failing.

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

### Full cloud stack (S3 + Redis)

Use the bundled `docker-compose.yml` to spin up LocalStack S3 (transcript
persistence) and Redis (reserved for session hot-state) locally:

```bash
cp .env.example .env          # fill in RECURSIVE_API_KEY
docker compose up
```

The compose image is built with `--build-arg FEATURES=http,cloud-runtime`, so
`RECURSIVE_S3_BUCKET` selects the shared `S3StorageBackend`: transcripts are
written on session teardown (DELETE / idle eviction / graceful shutdown) and
cold-loaded by `GET /sessions/:id` on any replica pointed at the same bucket.
Redis is provisioned for forward compatibility but **not consumed by
`recursive http` yet** — the kernel owns the `SessionStore` injection point but
does not checkpoint per turn, so hot-state is still in-process (see the
cheatsheet below).

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
| `RECURSIVE_MAX_STEPS` | `200` | Max tool-call loop iterations per turn. Issue #94: conservative finite default (was unlimited); `0` = unlimited |
| `RECURSIVE_WALL_TIMEOUT_SECS` | `3600` | Wall-clock budget per turn in seconds; expiry finishes with `wall_clock_exceeded`. Issue #94: conservative finite default (was unlimited); `0` = unlimited |
| `RECURSIVE_MAX_BUDGET_USD` | _(unlimited)_ | Per-turn USD spend ceiling (`--max-budget-usd`). Issue #94: the step loop compares the turn's accumulated spend after every completed step and stops with `budget_exceeded` once it reaches the ceiling — no further LLM call is issued. `0` / unset = no cap. When the model has no entry in `providers.toml` pricing, the ceiling is converted to a token cap at a deliberately pessimistic $10 per million tokens |
| `RECURSIVE_THINKING_BUDGET` | _(model default)_ | Anthropic extended-thinking budget, sent as `thinking = {type: "enabled", budget_tokens: n}` (`--effort low/normal/high` sets this too). Issue #94: previously stored and never consumed. `0` = disable thinking, unset = model default |
| `RECURSIVE_HARD_STEP_CAP` | _(unset)_ | Process-level step ceiling. When set (>0), the effective step limit is `min(max_steps, cap)` — an operator ceiling that clamps even `max_steps=0` sessions |
| `RECURSIVE_TEMPERATURE` | `0.2` | Sampling temperature |
| `RECURSIVE_SYSTEM_PROMPT_FILE` | _(built-in)_ | Path to a custom system-prompt file |
| `RECURSIVE_WORKSPACE` | cwd | Filesystem sandbox root |

#### HTTP server

| Env | Default | Purpose |
|-----|---------|---------|
| `RECURSIVE_HTTP_ADDR` | `0.0.0.0:3000` | Bind address |
| `RECURSIVE_MAX_CONCURRENT_RUNS` | `8` | Max concurrent agent runs (`0` = unlimited) |
| `RECURSIVE_ADMISSION_TIMEOUT_SECS` | `30` | Max seconds a request may wait for a run slot before `503` + `Retry-After`; `0` = wait indefinitely (legacy) |
| `RECURSIVE_HTTP_AUTH_KEYS` | _(required for prod)_ | Comma-separated `X-API-Key` allowlist |
| `RECURSIVE_HTTP_AUTH_JWT_SECRET` | _(none)_ | HMAC secret for JWT bearer-token auth |
| `RECURSIVE_HTTP_AUTH_JWT_AUDIENCE` | _(none)_ | Optional `aud` claim for JWT validation |
| `RECURSIVE_HTTP_AUTH_INSECURE_OK` | _(none)_ | Set to `1` to bypass auth (local dev ONLY) |
| `RECURSIVE_HTTP_MAX_STEPS` | `100` | Safe default step budget for HTTP-created sessions (request `max_steps` still wins; `0` = unlimited) |
| `RECURSIVE_HTTP_WALL_TIMEOUT_SECS` | `1800` | Safe default wall-clock budget per turn for HTTP-created sessions; expiry finishes with `wall_clock_exceeded` (`0` = unlimited) |
| `RECURSIVE_COMPACT_THRESHOLD` | auto (from model context window) | Cross-turn compaction char threshold (`0`/`off`/`false` = disable). Goal 393: effective for HTTP sessions too, same semantics as the CLI |
| `RECURSIVE_MICROCOMPACT_TRIGGER` / `RECURSIVE_MICROCOMPACT_KEEP` | _(disabled)_ / `4` | Opt-in proactive tool-result pruning after N tool messages, keeping the most recent K (`0` = off). Goal 393: effective for HTTP sessions too |
| `RECURSIVE_MAX_TRANSCRIPT_CHARS` | _(unlimited)_ | Hard transcript char cap per session (Goal 393: honored by HTTP session runtimes; the CLI also takes `--max-transcript-chars`) |
| `RECURSIVE_REINJECT_FILES` / `RECURSIVE_REINJECT_FILE_BUDGET` | `5` / `50000` | Post-compaction re-injection of recently-read files (`0`/`off`/`false` = off). Issue #127: the CLI/TUI contract now applies to HTTP, AG-UI and trigger runs too |
| `RECURSIVE_REINJECT_SKILLS` / `RECURSIVE_REINJECT_SKILL_BUDGET` | _(enabled)_ / `25000` | Post-compaction re-injection of invoked skills (`0`/`off`/`false` = off, a positive integer = token budget) |
| `RECURSIVE_AGENT_PRESET` | `standard` | Session preset (issue #127) selecting the whole assembly — prompt profile, tool profile, context management, re-injection. `GET /presets` lists the built-in presets with their capability inventory (including what is off by default); `POST /sessions` takes a per-session `preset` which `GET /sessions/:id` echoes. The variables above override the preset's declaration |

Request bodies carry the per-session run budgets (issue #94): `POST /run` and
`POST /sessions` accept `max_budget_usd` (per-turn spend ceiling — the turn
finishes with `finish_reason: "budget_exceeded"` once reached, and omitting it
falls back to `RECURSIVE_MAX_BUDGET_USD`) and `thinking_budget` (Anthropic
extended thinking; a request-level value builds a provider for that
session/run, since the budget is a per-request field in the Anthropic body).

#### Cloud storage — Redis (session hot-state)

Requires the `cloud-runtime` feature flag (`--features cloud-runtime`).
**Not consumed by `recursive http` yet**: `RedisSessionStore` is available
through the library API (`AgentRuntimeBuilder::session_store`), but the kernel
does not checkpoint per turn, so the HTTP server keeps `NoopSessionStore` and
only logs a note when `RECURSIVE_REDIS_URL` is set (issue #92). Exposing Redis
as the session table so replicas share in-flight sessions is future work.

| Env | Default | Purpose |
|-----|---------|---------|
| `RECURSIVE_REDIS_URL` | _(disabled)_ | Redis connection URL e.g. `redis://host:6379` |
| `RECURSIVE_REDIS_KEY_PREFIX` | `recursive:` | Key namespace prefix |
| `RECURSIVE_REDIS_SESSION_TTL_SECS` | `7200` | Session expiry (2 h) |

#### Cloud storage — S3 (transcript + memory)

Requires the `cloud-runtime` feature flag. **Wired into `recursive http`**:
when `RECURSIVE_S3_BUCKET` is set the server uses `S3StorageBackend` for
transcripts, memory entries and per-session metadata, so sessions survive a
restart and are visible to sibling replicas pointed at the same bucket. The
bucket is only consulted when the feature is compiled in; without it the var is
inert (a warning is logged).

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
| `RECURSIVE_SANDBOX` | _(unset = local)_ | `none` / `policy` / `container` / `microvm` |
| `RECURSIVE_E2B_API_KEY` | _(required for microvm)_ | E2B API key |
| `RECURSIVE_E2B_TEMPLATE` | `base` | E2B sandbox template ID |
| `RECURSIVE_E2B_TIMEOUT_SECS` | `3600` | Sandbox TTL in seconds |
| `RECURSIVE_SHELL_TIMEOUT_SECS` | `30` | Per-command shell timeout |

### Local vs cloud mode — cheatsheet

| Concern | Local (default) | Cloud (`cloud-runtime` feature) |
|---------|-----------------|----------------------------------|
| Transcript persistence | Local JSONL via `LocalStorageBackend`; HTTP sessions persist on delete / idle eviction / graceful shutdown — at most the turns after the last save are lost | S3 via `S3StorageBackend` when `RECURSIVE_S3_BUCKET` is set (same teardown-only timing) |
| Session hot-state | In-memory (`NoopSessionStore`), owned by the process | Redis via `RedisSessionStore` — library API only; `recursive http` does not checkpoint per turn yet |
| Tool execution | Host shell | Docker (L2) or E2B microVM (L3) |
| Horizontal scaling | Single process | Shared S3 makes transcripts visible to any replica after teardown; in-flight sessions still live on one pod, so route with sticky sessions (Redis session table is future work) |
| Resume across restarts | Via `--session` flag (CLI/TUI sessions) | HTTP `GET /sessions/:id` cold-loads from the storage backend (Goal 397) |

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

## AG-UI (frontend protocol)

`POST /agui` exposes agent runs to frontend clients (CopilotKit, `@ag-ui/client`)
over the AG-UI protocol: request a run with a `RunAgentInput` JSON body, consume
the SSE event stream (`RunStarted` … `RunFinished`). Frontend-owned tools,
interrupts, and resume round-trips are supported. See
[docs/architecture/agui.md](docs/architecture/agui.md) for the layer map
(`crates/agui-protocol` / `agui-client` / `agui-tui` + the transport-free
server session layer in `src/http/agui.rs`).

## Self-Improving Agents

Recursive develops itself. The same kernel you embed above is the one that
implements new features in Recursive — a run reads a goal from
`.dev/goals/`, drives `recursive` over the codebase in an isolated
worktree, runs the quality gates, self-reviews through a *different*
provider, then commits on success (a failing gate preserves the worktree
for a stronger agent or a human instead of discarding it), and records the
outcome in `.dev/journal/` so the next run starts from the last run's
lessons.

The loop runs on the **plaita engine**:

```bash
.dev/scripts/launch-flow-plaita.sh \
  --goal-file .dev/goals/01-my-goal.md \
  --provider deepseek
```

`launch-flow-plaita.sh` takes the same core flag surface as
`launch-flow.sh` (`--goal` / `--goal-file` / `--provider` / `--model` /
`--run-id` / `--hitl` / `--no-review` / `--no-commit` / `--max-steps` /
`--reviewer-provider`), starts the run in the background, and prints the
run id plus the log path. Every run writes to `.flowcast/runs/<run-id>/`;
a supervisor (or you) follows progress by polling `state.json` until a
terminal verdict appears.

### Architecture: thin flow, thick engine

Definition and observation belong to **plaita-console**; execution stays
local:

| Layer | Where | Owns |
|---|---|---|
| Node graph (thin) | `.dev/flows/self_improve_flow.py`, compiled to `self-improve.plaita.json` | The 45-node skeleton — one node per step of the loop |
| Engine (thick) | `.dev/flows/self_improve_engine.py` | All the real logic — watchdog, gate fix-loops, cross-provider review, commit/rebase, preserve |
| Bridge | `.dev/flows/self_improve_bridge.py` | Resolves the flow definition, then runs it locally |

Each node is a thin shim that shells out to
`self_improve_engine.py step <name>` and reads back `step-result.json`;
the engine owns the cross-step state. So changing loop *behaviour* means
editing only `self_improve_engine.py` — no re-publish of the flow
definition. Changing the node *graph* means editing
`self_improve_flow.py`, re-running `build_self_improve_flow.py`, and
publishing a new version to the console. Definitions resolve with a
three-tier fallback — published console version → cached copy in the run
directory → the in-repo JSON — while execution always stays on the local
machine.

### flowcast path (rollback)

`.dev/scripts/launch-flow.sh` — the Flowcast orchestrator
(`.dev/flows/self-improve.flow.js`) — is retained as the **rollback
path**, behaviourally equivalent to the plaita engine: same gates, same
cross-provider review, same verdicts (`committed` / `failed-preserved` /
`skip-commit` / `panic-preserved`), and the same run directory and
`state.json` contract. (flowcast has one extra, rare terminal value —
`rolled-back` — emitted only when an attempt error coincides with a failed
scene-preserve; the plaita engine always preserves instead.) A run can be
resumed or supervised identically on either engine; reach for flowcast if
the plaita path misbehaves on a given goal.

See [`.dev/flows/SELF_IMPROVE.md`](.dev/flows/SELF_IMPROVE.md) for the
operative guide and
[website/en/guide/self-improve.md](website/en/guide/self-improve.md) for
a walkthrough.

## Docs

- [LLM gateway compatibility](docs/llm-gateway-compat.md) — known traps with
  new-api/one-api/Bedrock (#15/#16/#17).
- [Self-improving agents](website/en/guide/self-improve.md) — how Recursive
  develops itself (plaita engine + flowcast rollback).

## License

MIT — see [LICENSE](LICENSE).
