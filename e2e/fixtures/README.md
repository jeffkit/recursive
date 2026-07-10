# aimock fixtures

aimock (`ghcr.io/copilotkit/aimock`) replays LLM responses from JSON fixture
files in this directory. Each fixture file is loaded by the `mocks.aimock`
section of `e2e/e2e.yaml` (`-f /fixtures`) and matched against every incoming
chat-completions request.

## Fixture schema

```json
{
  "fixtures": [
    {
      "match":       { "userMessage": "<substring>", "hasToolResult": <bool> },
      "response":    { "toolCalls": [ ... ] } | { "content": "<text>" }
    }
  ]
}
```

- `match.userMessage` — **substring** matched against the **latest** `role:user`
  text message in the request body (NOT the first/original goal, NOT tool
  results). Empty/absent ⇒ matches any user message.
- `match.hasToolResult` — `true` iff the conversation history already contains
  at least one tool result. Use this to distinguish "first LLM call of a turn"
  (`false`) from "follow-up call after a tool ran" (`true`).
- `response.toolCalls` — a list of `{ "name": "<PascalCase>", "arguments": {…} }`
  tool calls the mock model "makes". Tool names MUST be PascalCase
  (`Write`, `Read`, `Glob`) — the registry exports PascalCase, and asserting
  snake_case is a silent lie.
- `response.content` — a plain assistant text reply (no tool calls).

Fixtures are evaluated **top-to-bottom; the first match wins**. Put specific
matches before generic fallbacks.

## Multi-turn fixtures — the `userMessage` trap

This is the single most common aimock authoring bug:

> `match.userMessage` keys off the **latest** user message, not the original
> goal. In a multi-turn run the follow-up user message is the latest, so a
> fixture keyed on the goal's keyword will **not** match the follow-up turn —
> aimock returns `404 no_fixture_match`, the agent errors out, and (in
> `stream-json` mode) the run dies mid-stream.

> **Authoritative source:** the aimock docs spell this out — see
> <https://aimock.copilotkit.dev/multi-turn>. In particular `userMessage`
> matches only the **last** `role:"user"` message ("everything before it is
> ignored"), and multi-turn turns should be disambiguated with `turnIndex` /
> `hasToolResult` / `toolCallId` rather than `userMessage`. The Gotchas
> section there ("Prior turns are invisible", "First-wins ordering") is
> required reading before authoring any multi-turn fixture.

### Rule

**Every turn whose latest user message differs from the goal needs its own
fixture entry keyed on a substring unique to that latest message.**

### Worked example (`40-claude-json.json`)

A `run "Create greet.txt with content hi"` plus a follow-up
`"What file did you just create?"` needs three entries:

| # | `userMessage` | `hasToolResult` | matches which LLM call | response |
|---|---------------|-----------------|------------------------|----------|
| 1 | `greet.txt`   | `false`         | turn 1, call 1 (goal, no tool result yet) | `toolCalls: [Write greet.txt]` |
| 2 | `greet.txt`   | `true`          | turn 1, call 2 (after Write result; latest user msg is still the goal) | `content: "Done! I created greet.txt…"` |
| 3 | `just create` | `true`          | turn 2 (follow-up user msg `"…did you just create?"`; goal no longer latest) | `content: "I just created greet.txt…"` |

Drop entry #3 and the follow-up turn 404s. The follow-up text MUST contain
the `userMessage` substring (`"just create"`), and that substring should NOT
appear in the goal (otherwise entry #2 shadows it).

## Provider type

The `recursive` CLI must reach aimock with `RECURSIVE_PROVIDER_TYPE=openai`
(so the request body is OpenAI chat-completions shaped). `e2e/e2e.yaml` sets
this in `service.container.environment`; if you spin up a manual container,
set it yourself or aimock 404s every request regardless of fixtures.

## Asserting on the error path

A goal that matches **no** fixture makes the first LLM call 404. The CLI's
Claude `stream-json` mode still emits a terminal `result` envelope
(`is_error:true`, `subtype:error_during_execution`) in that case — this is
codified by the `error:` cases in `tests/40-claude-json-stream.yaml`. Use the
same pattern to assert that any future failure mode still closes the stream.
