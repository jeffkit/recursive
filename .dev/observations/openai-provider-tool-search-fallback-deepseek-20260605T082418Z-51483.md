# Run run-20260605T082418Z-51483

| field | value |
| --- | --- |
| goal | `openai-provider-tool-search-fallback` |
| provider | deepseek |
| model | deepseek-chat |
| baseline | a3ed12c |
| verdict | committed |
| termination reason | stuck:Write:3 |
| steps used | 13 |
| total tool calls | 16 |
| ERROR results from tools | 8 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Write: 8
  - Read: 6
  - TodoWrite: 2

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

