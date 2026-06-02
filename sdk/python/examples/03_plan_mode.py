"""03_plan_mode — drive Plan Mode 2.0 (g165–167) via RecursiveClient.

The agent enters ``plan_pending_approval`` after calling ``exit_plan_mode``.
Approve or reject via these endpoints:

    POST /sessions/{id}/plan/confirm  { edits?: str }
    POST /sessions/{id}/plan/reject   { reason?: str }
"""

from __future__ import annotations

import os
import sys

from recursive_client import RecursiveClient
from recursive_sdk import Agent


def main() -> int:
    base_url = os.environ.get("RECURSIVE_BASE_URL", "http://127.0.0.1:3000")
    client = RecursiveClient(base_url=base_url)

    with Agent.create(base_url=base_url) as agent:
        print("session :", agent.session_id)

        run = agent.send("Plan: refactor http.rs into smaller modules.")
        # The run may park in plan_pending_approval before completing —
        # swallow any wait() failure and inspect the session state directly.
        try:
            run.wait()
        except Exception:
            pass

        detail = client.get_session(agent.session_id)
        if detail.status != "plan_pending_approval" or not detail.pending_plan:
            print("session not in plan_pending_approval (status:", detail.status, ")")
            return 0

        print("\n--- pending plan ---")
        print(detail.pending_plan)
        print("--------------------\n")

        # Approve as-is. To edit before approving, pass edits=...
        resp = client.approve_plan(agent.session_id)
        print("approval :", resp["status"])

        # Drive the now-approved run to completion.
        agent.send("(continue)").wait()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
