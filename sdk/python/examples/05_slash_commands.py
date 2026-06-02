"""05_slash_commands — list registered slash commands (g169)."""

from __future__ import annotations

import os

from recursive_client import RecursiveClient


def main() -> None:
    client = RecursiveClient(
        base_url=os.environ.get("RECURSIVE_BASE_URL", "http://127.0.0.1:3000"),
    )
    cmds = client.list_slash_commands()

    print(f"{len(cmds)} slash command(s):\n")
    for c in sorted(cmds, key=lambda c: c.name):
        aliases = f" (aliases: {', '.join(c.aliases)})" if c.aliases else ""
        hint = f" {c.argument_hint}" if c.argument_hint else ""
        print(f"  /{c.name}{hint}{aliases}  [{c.source}]")
        print(f"      {c.description}")


if __name__ == "__main__":
    main()
