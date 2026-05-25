# Run run-20260525T084253Z-7240

| field | value |
| --- | --- |
| goal | `shell-env-passthrough` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | d863482 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 17 |
| total tool calls | 16 |
| ERROR results from tools | 2 |
| hit anti-stuck | no |
| hit step budget | no |
| hit length truncation | no |
| apply_patch invocations | 4 |
| write_file invocations | 0 |

## Tool-call distribution

  - run_shell: 9
  - apply_patch: 4
  - read_file: 3

## Patch discipline

apply_patch:write_file ratio = 4:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

