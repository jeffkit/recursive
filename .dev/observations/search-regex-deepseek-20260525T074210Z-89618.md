# Run run-20260525T074210Z-89618

| field | value |
| --- | --- |
| goal | `search-regex` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | ca64f83 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 12 |
| total tool calls | 13 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| hit length truncation | no |
| apply_patch invocations | 5 |
| write_file invocations | 0 |

## Tool-call distribution

  - run_shell: 6
  - apply_patch: 5
  - read_file: 2

## Patch discipline

apply_patch:write_file ratio = 5:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

