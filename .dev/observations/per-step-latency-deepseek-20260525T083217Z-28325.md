# Run run-20260525T083217Z-28325

| field | value |
| --- | --- |
| goal | `per-step-latency` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | deefbb5 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 48 |
| total tool calls | 57 |
| ERROR results from tools | 7 |
| hit anti-stuck | no |
| hit step budget | no |
| hit length truncation | no |
| apply_patch invocations | 11 |
| write_file invocations | 0 |

## Tool-call distribution

  - run_shell: 39
  - apply_patch: 11
  - read_file: 7

## Patch discipline

apply_patch:write_file ratio = 11:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

