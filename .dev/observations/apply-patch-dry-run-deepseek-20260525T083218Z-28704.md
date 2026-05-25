# Run run-20260525T083218Z-28704

| field | value |
| --- | --- |
| goal | `apply-patch-dry-run` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | deefbb5 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 11 |
| total tool calls | 10 |
| ERROR results from tools | 0 |
| hit anti-stuck | no |
| hit step budget | no |
| hit length truncation | no |
| apply_patch invocations | 2 |
| write_file invocations | 0 |

## Tool-call distribution

  - run_shell: 7
  - apply_patch: 2
  - read_file: 1

## Patch discipline

apply_patch:write_file ratio = 2:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

