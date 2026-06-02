"""02_multi_turn — create a session, send two messages, stream output."""

from __future__ import annotations

import os

from recursive_sdk import Agent


def main() -> None:
    base_url = os.environ.get("RECURSIVE_BASE_URL", "http://127.0.0.1:3000")

    with Agent.create(base_url=base_url) as agent:
        print("session :", agent.session_id)
        for prompt in ("Say hi.", "What did I just ask you?"):
            print(f"\n> {prompt}")
            run = agent.send(prompt)
            for msg in run.messages():
                if msg.type == "assistant":
                    print(msg.text(), end="", flush=True)
            result = run.wait()
            print(f"\n[finish: {result.finish_reason}]")


if __name__ == "__main__":
    main()
