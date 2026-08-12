from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from pikamux.models import Session
from pikamux.providers import ClaudeProvider, CodexProvider
from pikamux.store import Store


class UsageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.store = Store(self.root / "pika.db")

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_codex_uses_latest_cumulative_counter(self) -> None:
        path = self.root / "rollout.jsonl"
        rows = [
            {
                "payload": {
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 10,
                            "cached_input_tokens": 2,
                            "output_tokens": 3,
                            "total_tokens": 13,
                        }
                    }
                }
            },
            {
                "payload": {
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 20,
                            "cached_input_tokens": 4,
                            "output_tokens": 5,
                            "total_tokens": 25,
                        }
                    }
                }
            },
        ]
        path.write_text("\n".join(json.dumps(row) for row in rows))
        session = Session("codex", "id", transcript_path=str(path), model="gpt-5.4")
        usage = CodexProvider(self.root).usage(session, self.store)
        self.assertEqual(usage.total_tokens if usage else None, 25)
        self.assertIsNotNone(usage.estimated_cost_usd if usage else None)

    def test_codex_reads_latest_counter_from_large_rollout_tail(self) -> None:
        path = self.root / "large-rollout.jsonl"
        filler = json.dumps({"type": "event", "payload": {"text": "x" * 4000}})
        rows = [filler] * 1000
        rows.append(
            json.dumps(
                {
                    "payload": {
                        "info": {
                            "total_token_usage": {
                                "input_tokens": 100,
                                "output_tokens": 20,
                                "total_tokens": 120,
                            }
                        }
                    }
                }
            )
        )
        path.write_text("\n".join(rows) + "\n")
        session = Session("codex", "large", transcript_path=str(path), model="gpt-5.4")
        usage = CodexProvider(self.root).usage(session, self.store)
        self.assertEqual(usage.total_tokens if usage else None, 120)

    def test_claude_sums_structured_usage_only(self) -> None:
        path = self.root / "claude.jsonl"
        rows = [
            {
                "type": "assistant",
                "message": {
                    "model": "claude-opus-4-8",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 4,
                        "cache_read_input_tokens": 2,
                        "cache_creation_input_tokens": 3,
                    },
                },
                "content": "private text is ignored",
            },
            {
                "type": "assistant",
                "message": {
                    "model": "claude-opus-4-8",
                    "usage": {
                        "input_tokens": 5,
                        "output_tokens": 1,
                    },
                },
            },
        ]
        path.write_text("\n".join(json.dumps(row) for row in rows))
        session = Session("claude", "id", transcript_path=str(path))
        usage = ClaudeProvider(self.root).usage(session, self.store)
        self.assertEqual(usage.input_tokens if usage else None, 15)
        self.assertEqual(usage.total_tokens if usage else None, 25)
        self.assertIsNotNone(usage.estimated_cost_usd if usage else None)


if __name__ == "__main__":
    unittest.main()
