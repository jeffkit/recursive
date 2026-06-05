# Run run-20260605T080548Z-8910

| field | value |
| --- | --- |
| goal | `openai-provider-tool-search-fallback` |
| provider | anthropic-deepseek |
| model | deepseek-chat |
| baseline | a5e372e |
| verdict | committed |
| termination reason | stuck:Write:3 |
| steps used | 12 |
| total tool calls | 15 |
| ERROR results from tools | 5 |
| hit anti-stuck | no |
| hit step budget | no |
| auto-resumed | no |
| hit length truncation | no |
| apply_patch invocations | 0 |
| write_file invocations | 0 |

## Tool-call distribution

  - Read: 6
  - Write: 4
  - TodoWrite: 3
  - Edit: 1
  - Bash: 1

## Patch discipline

apply_patch:write_file ratio = 0:0.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

