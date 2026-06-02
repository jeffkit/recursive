"""04_goal_loop — autonomous goal loop (g168).

Endpoints:

    POST   /sessions/{id}/goal   { condition, max_turns }
    DELETE /sessions/{id}/goal
    GET    /sessions/{id}        (returns ``goal: GoalState | None``)
"""

from __future__ import annotations

import os
import time

from recursive_client import RecursiveClient
from recursive_sdk import Agent


def main() -> None:
    base_url = os.environ.get("RECURSIVE_BASE_URL", "http://127.0.0.1:3000")
    client = RecursiveClient(base_url=base_url)

    with Agent.create(base_url=base_url) as agent:
        client.set_goal(
            agent.session_id,
            "all unit tests in this repo pass",
            max_turns=10,
        )
        print("goal armed.")

        # Kick off the loop with an initial nudge.
        try:
            agent.send("Get started on the goal.").wait()
        except Exception:
            pass

        try:
            while True:
                goal = client.get_goal(agent.session_id)
                if goal is None:
                    print("goal cleared.")
                    break
                tail = f"  — {goal.last_reason}" if goal.last_reason else ""
                print(f"[{goal.turns}/{goal.max_turns}] {goal.status}{tail}")
                if goal.status != "pursuing":
                    break
                time.sleep(1.5)
        finally:
            try:
                client.clear_goal(agent.session_id)
            except Exception:
                pass


if __name__ == "__main__":
    main()
