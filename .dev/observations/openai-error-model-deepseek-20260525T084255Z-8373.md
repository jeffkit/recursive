# Run run-20260525T084255Z-8373

| field | value |
| --- | --- |
| goal | `openai-error-model` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | d863482 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 36 |
| total tool calls | 35 |
| ERROR results from tools | 9 |
| hit anti-stuck | no |
| hit step budget | no |
| hit length truncation | no |
| apply_patch invocations | 10 |
| write_file invocations | 0 |

## Tool-call distribution

  - run_shell: 20
  - apply_patch: 10
  - read_file: 4
  - search_files: 1

## Patch discipline

apply_patch:write_file ratio = 10:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

