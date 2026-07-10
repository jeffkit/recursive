"""Tests for CLI subprocess transport helpers."""

from __future__ import annotations

import os
import unittest
from unittest.mock import MagicMock

from recursive_sdk.cli import build_cli_args
from recursive_sdk.exceptions import RecursiveAgentError
from recursive_sdk.binary import find_recursive_binary
from recursive_sdk.wire import parse_wire_object
from recursive_sdk.run import Run
from recursive_sdk.models import AssistantMessage, TextContent, RunResult
from recursive_sdk.agent import Agent


class TestBuildCliArgs(unittest.TestCase):
    def test_basic_flags(self):
        args = build_cli_args(prompt="hello")
        self.assertIn("-p", args)
        self.assertIn("hello", args)
        self.assertIn("stream-json", args)
        self.assertIn("-H", args)

    def test_bypass_maps_to_auto(self):
        args = build_cli_args(prompt="x", permission_mode="bypass")
        idx = args.index("--permission-mode")
        self.assertEqual(args[idx + 1], "auto")

    def test_plan_first(self):
        args = build_cli_args(prompt="x", planning_mode="plan_first")
        idx = args.index("--permission-mode")
        self.assertEqual(args[idx + 1], "plan")

    def test_resume_and_workspace(self):
        args = build_cli_args(
            prompt="cont",
            resume_session_id="sess-1",
            cwd="/tmp/ws",
            model="deepseek-chat",
            max_steps=5,
        )
        self.assertIn("-r", args)
        self.assertIn("sess-1", args)
        self.assertIn("--workspace", args)
        self.assertIn("-m", args)
        self.assertIn("5", args)


class TestParseWire(unittest.TestCase):
    def test_init_session(self):
        item = parse_wire_object(
            {"type": "system", "subtype": "init", "session_id": "abc"},
            "",
        )
        assert item is not None
        self.assertEqual(item["kind"], "session")
        self.assertEqual(item["session_id"], "abc")

    def test_assistant(self):
        item = parse_wire_object(
            {
                "type": "assistant",
                "session_id": "s1",
                "message": {"content": [{"type": "text", "text": "hi"}]},
            },
            "s1",
        )
        assert item is not None
        self.assertEqual(item["kind"], "message")
        msg = item["message"]
        self.assertIsInstance(msg, AssistantMessage)
        self.assertEqual(msg.text(), "hi")

    def test_result_success(self):
        item = parse_wire_object(
            {
                "type": "result",
                "subtype": "success",
                "is_error": False,
                "session_id": "s1",
                "result": "done",
                "num_turns": 2,
                "usage": {"input_tokens": 10, "output_tokens": 5},
            },
            "s1",
        )
        assert item is not None
        result = item["result"]
        self.assertTrue(result.ok)
        self.assertEqual(result.result, "done")
        self.assertEqual(result.subtype, "success")


class TestFindBinary(unittest.TestCase):
    def test_missing_raises(self):
        old_path = os.environ.get("PATH")
        old_bin = os.environ.get("RECURSIVE_BIN")
        try:
            os.environ.pop("RECURSIVE_BIN", None)
            os.environ["PATH"] = "/nonexistent-dir-for-sdk-test"
            with self.assertRaises(RecursiveAgentError):
                find_recursive_binary()
        finally:
            if old_path is None:
                os.environ.pop("PATH", None)
            else:
                os.environ["PATH"] = old_path
            if old_bin is None:
                os.environ.pop("RECURSIVE_BIN", None)
            else:
                os.environ["RECURSIVE_BIN"] = old_bin


class TestRunFromCli(unittest.TestCase):
    def test_stream_and_wait(self):
        handle = MagicMock()
        handle.get_session_id.return_value = "sess-cli"
        handle.items.return_value = iter(
            [
                {
                    "kind": "message",
                    "message": AssistantMessage(
                        type="assistant",
                        content=[TextContent(text="hello")],
                        session_id="sess-cli",
                    ),
                },
                {
                    "kind": "result",
                    "result": RunResult(
                        id="sess-cli",
                        status="finished",
                        finish_reason="NoMoreToolCalls",
                        result="hello",
                    ),
                },
            ]
        )
        captured = []
        run = Run._from_cli("", handle, lambda sid: captured.append(sid))
        texts = []
        for msg in run.messages():
            if isinstance(msg, AssistantMessage):
                texts.append(msg.text())
        result = run.wait()
        self.assertEqual(texts, ["hello"])
        self.assertTrue(result.ok)
        self.assertEqual(captured, ["sess-cli"])
        self.assertEqual(run.id, "sess-cli")


class TestAgentCliMode(unittest.TestCase):
    def setUp(self):
        os.environ.pop("RECURSIVE_BASE_URL", None)

    def test_create_without_base_url_is_cli(self):
        agent = Agent.create(permission_mode="auto")
        self.assertEqual(agent.session_id, "")
        agent.close()


if __name__ == "__main__":
    unittest.main()
