"""Tests for PartialAssistantMessage (stream_event) in Run.messages()."""

import unittest
from unittest.mock import MagicMock, patch

from recursive_sdk.models import AssistantMessage, PartialAssistantMessage
from recursive_sdk.run import Run
from recursive_sdk._http import _HttpClient


def _make_run(events):
    """Return a Run backed by a fake SSE stream of *events*."""
    http = MagicMock(spec=_HttpClient)
    http.stream_events.return_value = iter(events)
    run = Run(session_id="test-sess", http=http)
    return run


class TestPartialAssistantMessage(unittest.TestCase):
    def test_partial_message_yields_stream_event(self):
        """partial_message SSE events become PartialAssistantMessage objects."""
        events = [
            {"type": "partial_message", "data": {"text": "Hello", "step": 0}},
            {"type": "partial_message", "data": {"text": " world", "step": 0}},
            {"type": "done", "data": {"status": "finished"}},
        ]
        run = _make_run(events)
        msgs = list(run.messages())

        deltas = [m for m in msgs if isinstance(m, PartialAssistantMessage)]
        self.assertEqual(len(deltas), 2)
        self.assertEqual(deltas[0].type, "stream_event")
        self.assertEqual(deltas[0].text, "Hello")
        self.assertEqual(deltas[0].step, 0)
        self.assertEqual(deltas[1].text, " world")
        self.assertEqual(deltas[0].session_id, "test-sess")

    def test_partial_message_not_included_in_result(self):
        """PartialAssistantMessage deltas are NOT included in RunResult.result."""
        events = [
            {"type": "partial_message", "data": {"text": "tok1", "step": 0}},
            {
                "type": "message",
                "data": {"role": "assistant", "content": "full reply"},
            },
            {"type": "done", "data": {"status": "finished"}},
        ]
        run = _make_run(events)
        result = run.wait()

        # result.result comes from AssistantMessage text, not from deltas
        self.assertEqual(result.result, "full reply")
        self.assertEqual(result.status, "finished")


if __name__ == "__main__":
    unittest.main()
