# Run run-20260525T113444Z-89129

| field | value |
| --- | --- |
| goal | `compactor-uses-structured-output` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 523eb35 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 23 |
| total tool calls | 24 |
| ERROR results from tools | 1 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 8 |
| write_file invocations | 1 |

## Tool-call distribution

  - run_shell: 8
  - apply_patch: 8
  - read_file: 7
  - write_file: 1

## Patch discipline

apply_patch:write_file ratio = 8:1.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

