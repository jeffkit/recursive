"""Unit tests for the Agent class in recursive_sdk."""

import unittest
from unittest.mock import MagicMock, patch

from recursive_sdk.agent import Agent, _AgentSession
from recursive_sdk._http import _HttpClient
from recursive_sdk.models import SessionInfo


class TestAgentForkSession(unittest.TestCase):
    """Tests for Agent.fork_session."""

    def _make_http(self, post_return: dict) -> _HttpClient:
        """Return a patched _HttpClient whose post() returns the given dict."""
        http = MagicMock(spec=_HttpClient)
        resp = MagicMock()
        resp.json.return_value = post_return
        http.post.return_value = resp
        return http

    @patch("recursive_sdk.agent._make_client")
    def test_fork_session_returns_session_info(self, mock_make_client):
        """fork_session calls POST /sessions/:id/fork and returns a SessionInfo."""
        http = self._make_http(
            {
                "id": "forked-id",
                "created_at": "2026-06-02T12:00:00Z",
                "message_count": 5,
            }
        )
        mock_make_client.return_value = http

        result = Agent.fork_session("src-id", base_url="http://localhost:3000")

        http.post.assert_called_once_with("/sessions/src-id/fork", {})
        self.assertIsInstance(result, SessionInfo)
        self.assertEqual(result.id, "forked-id")
        self.assertEqual(result.created_at, "2026-06-02T12:00:00Z")
        self.assertEqual(result.message_count, 5)

    @patch("recursive_sdk.agent._make_client")
    def test_fork_session_closes_http(self, mock_make_client):
        """fork_session closes the temporary HTTP client."""
        http = self._make_http(
            {"id": "fork-2", "created_at": "", "message_count": 0}
        )
        mock_make_client.return_value = http

        Agent.fork_session("any-id", base_url="http://localhost:3000")

        http.close.assert_called_once()


if __name__ == "__main__":
    unittest.main()
