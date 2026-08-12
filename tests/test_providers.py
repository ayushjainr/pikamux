from __future__ import annotations

import io
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from pikamux.models import Session
from pikamux.providers import ClaudeProvider, CodexProvider, _timestamp


class FakeAppServer:
    def __init__(self) -> None:
        self.stdin = io.StringIO()
        self.stdout = io.StringIO(
            json.dumps({"id": 1, "result": {}})
            + "\n"
            + json.dumps({"id": 2, "result": {}})
            + "\n"
        )
        self.terminated = False

    def terminate(self) -> None:
        self.terminated = True

    def wait(self, timeout: float | None = None) -> int:
        return 0

    def kill(self) -> None:
        self.terminated = True


class ProviderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(dir="/mnt/ebs1/ajain")
        self.root = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_iso_provider_timestamp_is_preserved(self) -> None:
        self.assertEqual(
            _timestamp("2026-08-12T10:30:00Z"),
            1786530600.0,
        )

    def test_claude_import_includes_selectable_ai_title(self) -> None:
        project = self.root / "projects" / "repo"
        project.mkdir(parents=True)
        session_id = "11111111-1111-4111-8111-111111111111"
        (project / f"{session_id}.jsonl").write_text(
            json.dumps({"type": "ai-title", "aiTitle": "derived hint"}) + "\n"
        )
        candidates = ClaudeProvider(self.root).import_candidates()
        self.assertEqual(len(candidates), 1)
        self.assertEqual(candidates[0].name, "derived hint")
        self.assertEqual(candidates[0].source, "claude-history")

    def test_claude_tracked_parked_rename_refreshes_from_transcript(self) -> None:
        project = self.root / "projects" / "repo"
        project.mkdir(parents=True)
        session_id = "11111111-1111-4111-8111-111111111111"
        transcript = project / f"{session_id}.jsonl"
        transcript.write_text(
            json.dumps({"type": "custom-title", "title": "renamed-parked"}) + "\n"
        )
        session = Session(
            "claude", session_id, name="before", transcript_path=str(transcript)
        )
        candidates = ClaudeProvider(self.root).tracked_candidates([session])
        self.assertEqual(len(candidates), 1)
        self.assertEqual(candidates[0].name, "renamed-parked")

    def test_codex_database_reader_tolerates_older_schema(self) -> None:
        database = self.root / "state_old.sqlite"
        rollout = self.root / "rollout.jsonl"
        rollout.write_text("{}\n")
        with sqlite3.connect(database) as db:
            db.execute(
                "CREATE TABLE threads "
                "(id TEXT PRIMARY KEY, name TEXT, cwd TEXT, rollout_path TEXT)"
            )
            db.execute(
                "INSERT INTO threads(id,name,cwd,rollout_path) VALUES (?,?,?,?)",
                (
                    "11111111-1111-4111-8111-111111111111",
                    "old-schema",
                    "/tmp",
                    str(rollout),
                ),
            )
        provider = CodexProvider(self.root)
        candidates = provider.discover()
        self.assertEqual(len(candidates), 1)
        self.assertEqual(candidates[0].name, "old-schema")
        self.assertTrue(provider.is_resumable("11111111-1111-4111-8111-111111111111"))

    def test_codex_database_row_without_rollout_is_not_resumable(self) -> None:
        database = self.root / "state_current.sqlite"
        with sqlite3.connect(database) as db:
            db.execute(
                "CREATE TABLE threads "
                "(id TEXT PRIMARY KEY, name TEXT, rollout_path TEXT)"
            )
            db.execute(
                "INSERT INTO threads(id,name,rollout_path) VALUES (?,?,?)",
                (
                    "77777777-7777-7777-8777-777777777777",
                    "missing",
                    str(self.root / "absent.jsonl"),
                ),
            )
        self.assertFalse(
            CodexProvider(self.root).is_resumable(
                "77777777-7777-7777-8777-777777777777"
            )
        )

    def test_codex_native_name_uses_official_thread_rpc(self) -> None:
        process = FakeAppServer()
        with (
            patch.object(CodexProvider, "installed", return_value=True),
            patch("pikamux.providers.subprocess.Popen", return_value=process) as popen,
            patch(
                "pikamux.providers.select.select",
                side_effect=lambda readable, _w, _x, _timeout: (readable, [], []),
            ),
        ):
            result = CodexProvider(self.root).set_native_name(
                "11111111-1111-4111-8111-111111111111", "pika-name"
            )
        self.assertTrue(result)
        self.assertTrue(process.terminated)
        self.assertEqual(popen.call_args.args[0], ["codex", "app-server", "--stdio"])
        messages = [json.loads(line) for line in process.stdin.getvalue().splitlines()]
        self.assertEqual(messages[-1]["method"], "thread/name/set")
        self.assertEqual(messages[-1]["params"]["name"], "pika-name")

    def test_malformed_provider_metadata_is_skipped(self) -> None:
        (self.root / "session_index.jsonl").write_text("not-json\n{}\n")
        (self.root / "sessions").mkdir()
        (self.root / "sessions" / "bad.json").write_text("{")
        self.assertEqual(CodexProvider(self.root).discover(), [])
        self.assertEqual(ClaudeProvider(self.root).discover(), [])


if __name__ == "__main__":
    unittest.main()
