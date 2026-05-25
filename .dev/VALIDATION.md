# Validation Strategy — Feature Confidence

> **Rule**: No feature is "done" until it has been exercised in a real
> scenario beyond unit tests. Unit tests prove the code compiles and
> handles synthetic inputs. Validation proves the feature actually works
> in the system.

## Three Layers

### Layer 1: self-improve.sh Dogfood (Highest Value)

Wire the feature into the self-improve loop so every subsequent batch
exercises it under real LLM traffic. This is how we found the Compactor
orphan-tool bug (batch 15) and the streaming startup panic (batch 13).

| Feature | Dogfood status | How to wire |
|---------|---------------|-------------|
| Compactor | ✅ active | threshold 200KB, fires on multi-read goals |
| Skills / Project Context | ✅ active | AGENTS.md auto-loaded |
| Transcript persistence | ✅ active | auto-resume relies on it |
| External Pricing (g51) | ✅ wired | `--pricing-file .dev/pricing.yaml` |
| Lifecycle Hooks (g48) | ❌ pending | Add a `DurationLogger` hook that writes tool timings to `.dev/runs/<id>.hooks.log` |
| Permission Hook (g43) | ❌ pending | Add a hook that denies `run_shell rm -rf /` patterns (safety demo) |
| Anthropic Provider | ❌ pending | Run one goal per batch with `RECURSIVE_PROVIDER_TYPE=anthropic` via DeepSeek's Anthropic endpoint |
| Anthropic Streaming (g52) | ❌ pending | Same as above + `--stream` |
| Sub-agent (g40) | ❌ pending | Write a goal that explicitly requires delegation ("split into subtasks") |
| MCP Client (g35) | ❌ pending | Add a `.mcp.json` with a trivial server (e.g. `echo` tool) |
| Tool Transport (g53) | ❌ pending | After landing, the LocalTransport IS the dogfood (transparent refactor) |
| Web Fetch (g37) | ❌ pending | Write a goal requiring URL retrieval |

### Layer 2: Integration Tests (`tests/integration/`)

Multi-feature combination tests using MockProvider + real filesystem:
- Agent with hooks + compaction + skills (the "full stack" test)
- Agent with permission_hook + sub_agent (inheritance verification)
- MCP discovery → tool registration → tool execution pipeline
- External pricing load → cost calculation accuracy
- Session pause → resume across process boundary

### Layer 3: Example Scripts (`examples/`)

Runnable Rust examples that demonstrate the public API:
- `examples/basic.rs` — minimal agent run
- `examples/with_hooks.rs` — lifecycle hooks logging
- `examples/with_mcp.rs` — MCP server discovery
- `examples/sub_agent.rs` — delegation pattern
- `examples/docker_transport.rs` — remote execution (future)

## Batch Composition Rule (NEW)

Starting from batch 19:

> **Every batch MUST contain at least one "validation goal"** — a goal
> that wires an existing but untested feature into the self-improve loop,
> writes an integration test, or creates an example. Pure feature-add
> goals without a validation companion are not allowed.

Ratio target: **3 feature : 1 validation** in a 4-wide batch, or
**1 feature : 1 validation** in a 2-wide batch.

## Validation Goal Template

```markdown
# Goal NN — Dogfood <Feature>

**Roadmap**: validation — wire <feature> into self-improve loop

**Design principle check**:
- Implemented as: dev-infra change to self-improve.sh / new integration test
- Does NOT modify product code (only tests/ or .dev/)

## Why
<Feature> landed in goal-XX but has never been exercised under real LLM
traffic. This goal wires it into the self-improve loop to surface latent
bugs through real usage.

## Scope
1. Modify self-improve.sh to enable <feature>
2. Run one full self-improve cycle with it active
3. If bugs surface: fix + add regression test
4. If clean: commit the config change so future runs benefit

## Acceptance
- Feature is active in self-improve.sh for all future runs
- OR: integration test proves the feature works end-to-end
- No regressions in existing tests
```

## Priority Queue (ordered by risk × impact)

1. **Hooks dogfood** — high risk of subtle bugs in event dispatch timing
2. **Anthropic streaming real-call** — mock tests can't catch SSE framing issues
3. **Sub-agent + permission inheritance** — never tested in real delegation
4. **MCP workspace discovery** — needs a real server to exercise
5. **Integration "full stack" test** — catches feature interaction bugs
