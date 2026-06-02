# Run run-20260602T073648Z-5141

| field | value |
| --- | --- |
| goal | `provider-presets` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | a17d830 |
| verdict | committed |
| termination reason | NoMoreToolCalls |
| steps used | 119 |
| total tool calls | 121 |
| ERROR results from tools | 38 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 42 |
| write_file invocations | 3 |

## Tool-call distribution

  - run_shell: 49
  - apply_patch: 42
  - read_file: 21
  - search_files: 5
  - write_file: 3
  - list_dir: 1

## Patch discipline

apply_patch:write_file ratio = 42:3.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

