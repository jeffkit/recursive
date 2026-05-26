"""Unit tests for RecursiveClient using mocked HTTP responses."""

import unittest
from unittest.mock import MagicMock, patch

from recursive_client import RecursiveClient
from recursive_client.models import (
    MessageResponse,
    RunResponse,
    SessionInfo,
    ToolInfo,
    UsageInfo,
)


class TestRecursiveClient(unittest.TestCase):
    def setUp(self):
        self.client = RecursiveClient("http://localhost:3000")

    @patch.object(RecursiveClient, "__init__", lambda self, *a, **kw: None)
    def _make_client(self):
        """Create a client with a mocked session."""
        client = RecursiveClient()
        client.base_url = "http://localhost:3000"
        client.session = MagicMock()
        return client

    def test_health(self):
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.text = "ok"
        mock_resp.raise_for_status = MagicMock()
        client.session.get.return_value = mock_resp

        result = client.health()

        self.assertEqual(result, "ok")
        client.session.get.assert_called_once_with("http://localhost:3000/health")
        mock_resp.raise_for_status.assert_called_once()

    def test_list_tools(self):
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.json.return_value = [
            {
                "name": "shell",
                "description": "Run a shell command",
                "parameters": {"command": {"type": "string"}},
            },
            {
                "name": "read_file",
                "description": "Read a file",
                "parameters": {"path": {"type": "string"}},
            },
        ]
        mock_resp.raise_for_status = MagicMock()
        client.session.get.return_value = mock_resp

        tools = client.list_tools()

        self.assertEqual(len(tools), 2)
        self.assertIsInstance(tools[0], ToolInfo)
        self.assertEqual(tools[0].name, "shell")
        self.assertEqual(tools[1].name, "read_file")
        client.session.get.assert_called_once_with("http://localhost:3000/tools")

    def test_run(self):
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.json.return_value = {
            "status": "success",
            "finish_reason": "Complete",
            "messages": [{"role": "assistant", "content": "Done."}],
            "usage": {"total_steps": 3, "total_tokens": 1500},
        }
        mock_resp.raise_for_status = MagicMock()
        client.session.post.return_value = mock_resp

        result = client.run("Write hello.txt", max_steps=10)

        self.assertIsInstance(result, RunResponse)
        self.assertEqual(result.status, "success")
        self.assertEqual(result.finish_reason, "Complete")
        self.assertEqual(len(result.messages), 1)
        self.assertIsInstance(result.usage, UsageInfo)
        self.assertEqual(result.usage.total_steps, 3)
        self.assertEqual(result.usage.total_tokens, 1500)
        client.session.post.assert_called_once_with(
            "http://localhost:3000/run",
            json={"goal": "Write hello.txt", "max_steps": 10},
        )

    def test_create_session(self):
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.json.return_value = {
            "id": "abc123",
            "created_at": "2025-01-01T00:00:00Z",
        }
        mock_resp.raise_for_status = MagicMock()
        client.session.post.return_value = mock_resp

        session_id = client.create_session(system_prompt="Be helpful.")

        self.assertEqual(session_id, "abc123")
        client.session.post.assert_called_once_with(
            "http://localhost:3000/sessions",
            json={"system_prompt": "Be helpful."},
        )

    def test_send_message(self):
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.json.return_value = {
            "role": "assistant",
            "content": "Hello! How can I help?",
        }
        mock_resp.raise_for_status = MagicMock()
        client.session.post.return_value = mock_resp

        result = client.send_message("abc123", "Hi there")

        self.assertIsInstance(result, MessageResponse)
        self.assertEqual(result.role, "assistant")
        self.assertEqual(result.content, "Hello! How can I help?")
        client.session.post.assert_called_once_with(
            "http://localhost:3000/sessions/abc123/messages",
            json={"content": "Hi there"},
        )

    def test_list_sessions(self):
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.json.return_value = [
            {"id": "s1", "created_at": "2025-01-01T00:00:00Z", "message_count": 5},
            {"id": "s2", "created_at": "2025-01-02T00:00:00Z", "message_count": 2},
        ]
        mock_resp.raise_for_status = MagicMock()
        client.session.get.return_value = mock_resp

        sessions = client.list_sessions()

        self.assertEqual(len(sessions), 2)
        self.assertIsInstance(sessions[0], SessionInfo)
        self.assertEqual(sessions[0].id, "s1")
        self.assertEqual(sessions[0].message_count, 5)

    def test_get_session(self):
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.json.return_value = {
            "id": "s1",
            "created_at": "2025-01-01T00:00:00Z",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
            ],
        }
        mock_resp.raise_for_status = MagicMock()
        client.session.get.return_value = mock_resp

        detail = client.get_session("s1")

        self.assertEqual(detail.id, "s1")
        self.assertEqual(len(detail.messages), 2)
        client.session.get.assert_called_once_with(
            "http://localhost:3000/sessions/s1"
        )

    def test_delete_session(self):
        client = self._make_client()
        mock_resp = MagicMock()
        mock_resp.raise_for_status = MagicMock()
        client.session.delete.return_value = mock_resp

        client.delete_session("s1")

        client.session.delete.assert_called_once_with(
            "http://localhost:3000/sessions/s1"
        )
        mock_resp.raise_for_status.assert_called_once()


if __name__ == "__main__":
    unittest.main()
