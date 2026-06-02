"""01_prompt — one-shot Agent.prompt() against a running Recursive server.

Run::

    cargo run --bin recursive -- http   # in another shell
    pip install -e sdk/python       # once
    python sdk/python/examples/01_prompt.py
"""

from __future__ import annotations

import os
import sys

from recursive_sdk import Agent


def main() -> int:
    base_url = os.environ.get("RECURSIVE_BASE_URL", "http://127.0.0.1:3000")
    print(f"→ POST {base_url}/run")

    result = Agent.prompt(
        "List the files in the current directory.",
        base_url=base_url,
        max_steps=5,
    )

    print("status       :", result.status)
    print("finish_reason:", result.finish_reason)
    print("session id   :", result.id)
    if result.usage:
        print(f"usage        : {result.usage.input_tokens} in / "
              f"{result.usage.output_tokens} out")
    if result.status != "finished":
        print("error        :", result.error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
