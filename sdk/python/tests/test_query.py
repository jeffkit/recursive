"""Tests for Claude-compatible query() API."""

from __future__ import annotations

import unittest
from unittest.mock import MagicMock, patch

from recursive_sdk.control_session import build_control_cli_args
from recursive_sdk.models import AssistantMessage, RunResult, TextContent
from recursive_sdk.query import (
    ClaudeAgentOptions,
    map_permission,
    options_to_spawn,
    query,
    wire_item_to_query_message,
)


class TestOptionsMapping(unittest.TestCase):
    def test_claude_names(self):
        opts = ClaudeAgentOptions(
            cwd="/tmp/ws",
            model="deepseek-chat",
            max_turns=7,
            permission_mode="bypassPermissions",
            resume="sess-9",
            path_to_cli="/usr/bin/recursive",
            system_prompt="be brief",
            max_budget_usd=1.5,
            allowed_tools=["Read", "Write"],
        )
        spawn = options_to_spawn("fix auth", opts)
        self.assertEqual(spawn["prompt"], "fix auth")
        self.assertEqual(spawn["cwd"], "/tmp/ws")
        self.assertEqual(spawn["max_steps"], 7)
        self.assertEqual(spawn["permission_mode"], "auto")
        self.assertEqual(spawn["resume_session_id"], "sess-9")
        self.assertEqual(spawn["cli_path"], "/usr/bin/recursive")
        self.assertEqual(spawn["system_prompt"], "be brief")
        self.assertEqual(spawn["allowed_tools"], ["Read", "Write"])

    def test_plan_mode(self):
        spawn = options_to_spawn("x", ClaudeAgentOptions(permission_mode="plan"))
        self.assertEqual(spawn["planning_mode"], "plan_first")

    def test_preset_append(self):
        spawn = options_to_spawn(
            "x",
            ClaudeAgentOptions(
                system_prompt={
                    "type": "preset",
                    "preset": "claude_code",
                    "append": "\nAlways run tests.",
                }
            ),
        )
        self.assertIsNone(spawn["system_prompt"])
        self.assertEqual(spawn["append_system_prompt"], "\nAlways run tests.")

    def test_map_permission(self):
        self.assertEqual(map_permission("bypassPermissions"), "auto")
        self.assertEqual(map_permission("acceptEdits"), "auto")
        self.assertEqual(map_permission("default"), "default")


class TestControlCliArgs(unittest.TestCase):
    def test_no_headless_has_input_format(self):
        args = build_control_cli_args(prompt="hello")
        self.assertIn("-p", args)
        self.assertIn("--input-format", args)
        self.assertIn("stream-json", args)
        self.assertNotIn("-H", args)

    def test_allow_tools(self):
        args = build_control_cli_args(prompt="x", allowed_tools=["Read", "Bash"])
        self.assertIn("--allow-tools", args)
        self.assertIn("Read,Bash", args)


class TestWireToQueryMessage(unittest.TestCase):
    def test_result(self):
        item = {
            "kind": "result",
            "result": RunResult(
                id="s1",
                status="finished",
                finish_reason="NoMoreToolCalls",
                result="hello",
                num_turns=1,
            ),
        }
        msg = wire_item_to_query_message(item)
        assert msg is not None
        self.assertEqual(msg["type"], "result")
        self.assertEqual(msg["result"], "hello")
        self.assertEqual(msg["subtype"], "success")
        self.assertFalse(msg["is_error"])

    def test_assistant(self):
        item = {
            "kind": "message",
            "message": AssistantMessage(
                type="assistant",
                content=[TextContent(text="hi")],
                session_id="s1",
            ),
        }
        msg = wire_item_to_query_message(item)
        assert msg is not None
        self.assertEqual(msg["type"], "assistant")
        self.assertEqual(msg["message"]["content"][0]["text"], "hi")


class TestQueryAsync(unittest.IsolatedAsyncioTestCase):
    async def test_yields_result_in_stream(self):
        handle = MagicMock()
        handle.items.return_value = iter(
            [
                {
                    "kind": "message",
                    "message": AssistantMessage(
                        type="assistant",
                        content=[TextContent(text="hello")],
                        session_id="s1",
                    ),
                },
                {
                    "kind": "result",
                    "result": RunResult(
                        id="s1",
                        status="finished",
                        finish_reason="NoMoreToolCalls",
                        result="hello",
                    ),
                },
            ]
        )
        handle.cancel = MagicMock()
        handle.close = MagicMock()
        handle.interrupt = MagicMock()

        with patch(
            "recursive_sdk.query.spawn_control_session", return_value=handle
        ):
            messages = []
            async for msg in query(
                prompt="hi", options=ClaudeAgentOptions(max_turns=3)
            ):
                messages.append(msg)

        self.assertTrue(any(m["type"] == "assistant" for m in messages))
        result = next(m for m in messages if m["type"] == "result")
        self.assertEqual(result["result"], "hello")

    async def test_interrupt(self):
        handle = MagicMock()
        handle.items.return_value = iter([])
        handle.interrupt = MagicMock()
        handle.cancel = MagicMock()
        handle.close = MagicMock()

        with patch(
            "recursive_sdk.query.spawn_control_session", return_value=handle
        ):
            q = query(prompt="x")
            await q.interrupt()
            handle.interrupt.assert_called()


if __name__ == "__main__":
    unittest.main()
