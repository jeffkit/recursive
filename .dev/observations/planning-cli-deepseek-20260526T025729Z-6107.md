# Run run-20260526T025729Z-6107

| field | value |
| --- | --- |
| goal | `planning-cli` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | 5e8502f |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 167 |
| total tool calls | 168 |
| ERROR results from tools | 4 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 26 |
| write_file invocations | 0 |

## Tool-call distribution

  - read_file: 68
  - run_shell: 41
  - search_files: 33
  - apply_patch: 26

## Patch discipline

apply_patch:write_file ratio = 26:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

