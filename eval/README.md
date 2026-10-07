# Preset benchmark harness (issue #129)

Quantify a preset's gain instead of arguing about it. This directory holds the
fixed task set's committed **baseline run** — raw per-run records in
`results/`, the rendered comparison in [`report.md`](report.md) — and this
document is the methodology + decision record.

The code lives in the product: `src/eval.rs` (task set, runner, report) and the
`recursive-eval` binary (`src/bin/recursive-eval.rs`).

## One command

```bash
# Replay baseline — deterministic, no API key, ~1 s.
cargo run --bin recursive-eval -- run \
    --out eval/results/replay-baseline.jsonl --report eval/report.md

# Live measurement — the configured provider, N repeats for a variance interval.
cargo run --bin recursive-eval -- run --mode live --repeat 3 \
    --out eval/results/live-$(date -u +%Y%m%dT%H%M%SZ).jsonl --report eval/live-report.md

# Re-render a report from an existing raw file (no re-run).
cargo run --bin recursive-eval -- report eval/results/replay-baseline.jsonl
```

Run it from the repository root. `run` prints one line per (task × preset) to
stderr and writes the JSONL + markdown.

## Axes

### Task set — fixed, 10 tasks, 5 categories

| category | tasks | shape |
|---|---|---|
| single-file | `single-append-row`, `single-fix-config` | read → modify → write back one file |
| search-locate | `search-find-marker`, `search-count-lines` | locate a file / a value across several |
| multi-step | `chain-script-run`, `chain-transform` | dependent chain (write → run → read → write) |
| long-context | `long-extract-line`, `long-many-reads` | a large input the preset has to carry |
| failure-recovery | `recover-missing-read`, `recover-bad-edit` | the first step fails; recover |

Each task declares its own seed workspace, a deterministic replay script, and a
machine-checkable success predicate over the resulting files. The set is
compiled into `src/eval.rs` (`TASKS`) so it cannot drift between machines; the
list is what makes numbers comparable across time. `recursive-eval list` prints
it.

Worked example — replay is **not** a token-perfect model of the model:

```
[standard  single-append-row] pass replay in=2799 out=56 wall=93ms turns=3 tools=2
[minimal   single-append-row] pass replay in=156  out=56 wall=58ms turns=3 tools=2
```

### Presets — every built-in preset

`minimal` and `standard` today (`recursive::preset::builtin`). The harness
resolves each with a default preset environment, so an ambient
`RECURSIVE_AGENT_PRESET` / `RECURSIVE_COMPACT_THRESHOLD` cannot silently move
the baseline.

### Modes

* **replay** (default) — the task's script drives a `MockProvider`. Tokens and
  success are identical on every run; the fixed system-prompt cost is *real*
  (the harness sees the exact prompt bytes), the model's output is scripted.
  This is what the committed baseline uses and what makes the harness runnable
  in CI with no key.
* **live** — the configured provider (`RECURSIVE_*` / `--api-key`). This is the
  quality measurement: real success rate, real provider usage, real latency.
  Repeats are recommended (`--repeat 3`).

## Metrics

Per run (one row in the JSONL):

| field | meaning |
|---|---|
| `system_prompt_tokens` | the preset's system-prompt weight — the **fixed cost** per request. Model-independent; identical for every task under a preset. |
| `input_tokens` | prompt tokens paid across the run. Replay: harness estimate. Live: provider usage. |
| `output_tokens` | tokens the model produced. |
| `cache_read_tokens` | provider cache hits (live only; `0` in replay). |
| `wall_ms` | wall clock for the run. In replay this is tool execution only. |
| `turns` | LLM calls in the run. |
| `tool_calls` | tool calls the model issued. |
| `success` | the task's success predicate held **and** the run did not error. |
| `failure` | failure class: `budget_exceeded` / `stuck` / `context_limit` / `provider_error` / `wall_clock` / `permission_denied` / `run_error` / `wrong_result` (finished cleanly but the files were wrong). |

The report also renders a **repeat spread** table (`min..max` per task × preset)
whenever a task was run more than once — the variance interval for acceptance 2.

## Baseline (committed, replay, 3 repeats)

`results/replay-baseline.jsonl` (60 rows = 10 tasks × 2 presets × 3 repeats) +
`report.md`, from HEAD at the time of the commit. Headline:

| axis | minimal | standard | delta |
|---|---|---|---|
| system prompt tok (fixed cost) | 12 | 893 | **74.4×** |
| input tokens (total, 30 runs) | 26 667 | 127 101 | 4.8× |
| success rate | 30/30 = 100 % | 30/30 = 100 % | 0 pp |

Reproduce it with the first command above (`--repeat 3`). The report's
**Repeat spread** table is the reproducibility evidence: tokens are identical
across repeats (`input tok min..max` = `279..279` …), while `wall_ms` moves
(`940..2097`) because in replay it is real tool execution — and the committed
run was taken on a loaded machine, so its absolute wall numbers are not a
performance measurement.

## Decision use cases (acceptance 3)

1. **Preset gain — `minimal` vs `standard` (issue #128).** The fixed cost of a
   standard session is 74× the minimal one's, and the total input paid across
   this task set is 4.8× larger (the transcript, not the prompt, dominates the
   longer tasks — `long-extract-line` is 6 490 vs 9 133 input tokens). The
   one-line prompt does not change
   whether the simple tasks complete (100 % in replay). This is the quantified
   form of #128's acceptance 1: *minimal exists to be compared against*.
2. **Standing before/after harness for prompt / mechanism changes.** Any change
   to the prompt, the preset, or a context mechanism gets a re-run and a report
   diff instead of a vibe check:

   ```bash
   cargo run --bin recursive-eval -- run --mode live --repeat 3 \
       --out eval/results/live-after.jsonl
   diff eval/report-live-before.md eval/report-live-after.md
   ```

   The #110–#124 observability work is what makes this possible at all: before
   it, a run could not attribute prompt vs output vs cache tokens, a failure
   had no class, and a "finished" turn was indistinguishable from a failed one.
   The harness consumes exactly that surface (`FinishReason`, `TokenUsage`
   with the cache split, per-session cost). Running the harness on a build
   predating #110–#124 gives `cache_read_tokens = 0` and an empty failure
   column — that is the "before" side of the decision case.

## Files

| path | role |
|---|---|
| `src/eval.rs` | task set, runner, metrics, report renderer (+ unit tests) |
| `src/bin/recursive-eval.rs` | the `recursive-eval` CLI |
| `eval/results/*.jsonl` | committed raw per-run records |
| `eval/report.md` | the rendered comparison for the baseline above |

## Honest limits

* Replay proves the plumbing and measures the fixed cost; it cannot measure
  model quality. The success-rate and latency *deltas between presets* only
  mean something in live mode.
* Live numbers are model- and temperature-dependent. Pin the model, use
  `--repeat`, and compare reports produced on the same model.
* The task set is deliberately small (10) and its success predicates are
  file-state assertions — a task can be "solved" by a wrong-but-scoring edit.
  Grow the set in `TASKS`; keep it fixed once published.
