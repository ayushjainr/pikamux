from __future__ import annotations

import io
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from pikamux.models import Session
from pikamux.providers import (
    ClaudeProvider,
    CodexProvider,
    OpenCodeProvider,
    _timestamp,
    codex_lifecycle_status,
    codex_worker_originator,
    opencode_native_placeholder_title,
)


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
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_iso_provider_timestamp_is_preserved(self) -> None:
        self.assertEqual(
            _timestamp("2026-08-12T10:30:00Z"),
            1786530600.0,
        )

    def test_opencode_native_placeholder_titles_are_provider_scaffolding(self) -> None:
        self.assertTrue(
            opencode_native_placeholder_title(
                "New session - 2026-08-26T00:00:00Z"
            )
        )
        self.assertTrue(opencode_native_placeholder_title("Research (fork #2)"))
        self.assertFalse(opencode_native_placeholder_title("oc_research"))

    def test_claude_import_excludes_ai_generated_title(self) -> None:
        project = self.root / "projects" / "repo"
        project.mkdir(parents=True)
        session_id = "11111111-1111-4111-8111-111111111111"
        (project / f"{session_id}.jsonl").write_text(
            json.dumps({"type": "ai-title", "aiTitle": "derived hint"}) + "\n"
        )
        self.assertEqual(ClaudeProvider(self.root).import_candidates(), [])

    def test_claude_exact_uuid_finds_unnamed_resumable_history(self) -> None:
        project = self.root / "projects" / "repo"
        project.mkdir(parents=True)
        session_id = "11111111-1111-4111-8111-111111111111"
        (project / f"{session_id}.jsonl").write_text(
            json.dumps({"type": "user", "message": "hello"}) + "\n"
        )
        matches = ClaudeProvider(self.root).find_candidates(session_id)
        self.assertEqual([item.session_id for item in matches], [session_id])
        self.assertIsNone(matches[0].name)

    def test_claude_import_includes_explicit_historical_title(self) -> None:
        project = self.root / "projects" / "repo"
        project.mkdir(parents=True)
        session_id = "11111111-1111-4111-8111-111111111111"
        (project / f"{session_id}.jsonl").write_text(
            json.dumps({"type": "ai-title", "aiTitle": "derived hint"})
            + "\n"
            + json.dumps({"type": "custom-title", "title": "chosen name"})
            + "\n"
        )
        candidates = ClaudeProvider(self.root).import_candidates()
        self.assertEqual(len(candidates), 1)
        self.assertEqual(candidates[0].name, "chosen name")
        self.assertEqual(candidates[0].source, "claude-history")

    def test_claude_sdk_cli_title_is_automation_not_a_conversation(self) -> None:
        project = self.root / "projects" / "repo"
        project.mkdir(parents=True)
        worker_id = "11111111-1111-4111-8111-111111111111"
        interactive_id = "22222222-2222-4222-8222-222222222222"
        worker = project / f"{worker_id}.jsonl"
        interactive = project / f"{interactive_id}.jsonl"
        worker.write_text(
            json.dumps({"type": "custom-title", "customTitle": "sample_plugin"})
            + "\n"
            + json.dumps(
                {
                    "type": "user",
                    "sessionId": worker_id,
                    "entrypoint": "sdk-cli",
                    "isSidechain": False,
                }
            )
            + "\n"
            + json.dumps(
                {
                    "type": "user",
                    "sessionId": worker_id,
                    "entrypoint": "cli",
                    "isSidechain": False,
                }
            )
            + "\n"
        )
        interactive.write_text(
            json.dumps({"type": "custom-title", "customTitle": "sample_plugin"})
            + "\n"
            + json.dumps(
                {
                    "type": "user",
                    "sessionId": interactive_id,
                    "entrypoint": "cli",
                    "isSidechain": False,
                }
            )
            + "\n"
            + json.dumps(
                {
                    "type": "user",
                    "sessionId": interactive_id,
                    "entrypoint": "sdk-cli",
                    "isSidechain": False,
                }
            )
            + "\n"
        )
        sessions = self.root / "sessions"
        sessions.mkdir()
        (sessions / f"{worker_id}.json").write_text(
            json.dumps(
                {
                    "sessionId": worker_id,
                    "kind": "interactive",
                    "name": "sample_plugin",
                    "nameSource": "custom",
                    "cwd": "/tmp/automation",
                }
            )
        )
        provider = ClaudeProvider(self.root)

        self.assertEqual(
            provider.worker_originator(worker_id, str(worker)), "claude-sdk-cli"
        )
        self.assertIsNone(provider.worker_originator(interactive_id, str(interactive)))
        self.assertEqual(
            [item.session_id for item in provider.import_candidates()],
            [interactive_id],
        )
        self.assertEqual(provider.find_candidates(worker_id), [])
        self.assertFalse(provider.is_resumable(worker_id))
        self.assertTrue(provider.is_resumable(interactive_id))

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

    def test_claude_explicit_title_beats_later_generated_titles(self) -> None:
        project = self.root / "projects" / "repo"
        sessions = self.root / "sessions"
        project.mkdir(parents=True)
        sessions.mkdir()
        session_id = "11111111-1111-4111-8111-111111111111"
        (sessions / f"{session_id}.json").write_text(
            json.dumps(
                {
                    "sessionId": session_id,
                    "kind": "interactive",
                    "name": "generated-old-name",
                    "cwd": "/tmp",
                }
            )
        )
        (project / f"{session_id}.jsonl").write_text(
            json.dumps({"type": "custom-title", "customTitle": "sample_plugin"})
            + "\n"
            + json.dumps({"type": "ai-title", "aiTitle": "generated-old-name"})
            + "\n"
        )

        candidate = ClaudeProvider(self.root).import_candidates()[0]
        self.assertEqual(candidate.name, "sample_plugin")
        self.assertEqual(candidate.source, "claude-live+explicit-history")

    def test_codex_candidate_exposes_lineage_and_structured_lifecycle(self) -> None:
        parent = "11111111-1111-4111-8111-111111111111"
        child = "22222222-2222-4222-8222-222222222222"
        rollout = self.root / "child.jsonl"
        rollout.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": child,
                        "forked_from_id": parent,
                        "originator": "Codex Desktop",
                    },
                }
            )
            + "\n"
            + json.dumps(
                {"type": "event_msg", "payload": {"type": "task_started"}}
            )
            + "\n"
        )
        with sqlite3.connect(self.root / "state_current.sqlite") as db:
            db.execute(
                "CREATE TABLE threads "
                "(id TEXT PRIMARY KEY, name TEXT, cwd TEXT, rollout_path TEXT, "
                "created_at INTEGER, updated_at INTEGER, archived INTEGER)"
            )
            db.execute(
                "INSERT INTO threads VALUES (?,?,?,?,?,?,0)",
                (child, "research-notes", "/repo", str(rollout), 10, 20),
            )

        candidate = CodexProvider(self.root).discover()[0]
        self.assertEqual(candidate.parent_session_id, parent)
        self.assertEqual(candidate.lifecycle_status, "WORKING")
        self.assertEqual(candidate.created_at, 10)
        self.assertEqual(codex_lifecycle_status(rollout), "WORKING")

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

    def test_codex_exact_uuid_finds_unnamed_resumable_thread(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        rollout = self.root / "unnamed.jsonl"
        rollout.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {"id": session_id, "cwd": "/tmp"},
                }
            )
            + "\n"
        )
        with sqlite3.connect(self.root / "state_current.sqlite") as db:
            db.execute(
                "CREATE TABLE threads "
                "(id TEXT PRIMARY KEY, name TEXT, cwd TEXT, rollout_path TEXT, "
                "archived INTEGER, updated_at INTEGER)"
            )
            db.execute(
                "INSERT INTO threads VALUES (?,?,?,?,0,1)",
                (session_id, None, "/tmp", str(rollout)),
            )
        provider = CodexProvider(self.root)
        self.assertEqual(provider.discover(), [])
        matches = provider.find_candidates(session_id)
        self.assertEqual([item.session_id for item in matches], [session_id])
        self.assertIsNone(matches[0].name)

    def test_codex_archived_row_is_hidden_even_with_legacy_name(self) -> None:
        active_id = "11111111-1111-4111-8111-111111111111"
        archived_id = "22222222-2222-4222-8222-222222222222"
        active_rollout = self.root / "active.jsonl"
        archived_rollout = self.root / "archived.jsonl"
        active_rollout.write_text("{}\n")
        archived_rollout.write_text("{}\n")
        with sqlite3.connect(self.root / "state_current.sqlite") as db:
            db.execute(
                "CREATE TABLE threads "
                "(id TEXT PRIMARY KEY, name TEXT, rollout_path TEXT, archived INTEGER)"
            )
            db.executemany(
                "INSERT INTO threads(id,name,rollout_path,archived) VALUES (?,?,?,?)",
                (
                    (active_id, None, str(active_rollout), 0),
                    (archived_id, None, str(archived_rollout), 1),
                ),
            )
        (self.root / "session_index.jsonl").write_text(
            json.dumps({"id": active_id, "thread_name": "research-notes"})
            + "\n"
            + json.dumps({"id": archived_id, "thread_name": "research-notes"})
            + "\n"
        )

        provider = CodexProvider(self.root)
        self.assertEqual(provider.hidden_session_ids(), {archived_id})
        self.assertEqual(
            [candidate.session_id for candidate in provider.discover()], [active_id]
        )
        self.assertTrue(provider.is_resumable(active_id))
        self.assertFalse(provider.is_resumable(archived_id))

    def test_codex_index_title_is_lookup_evidence_not_setup_rename_proof(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        rollout = self.root / "rollout.jsonl"
        rollout.write_text("{}\n")
        with sqlite3.connect(self.root / "state_current.sqlite") as db:
            db.execute(
                "CREATE TABLE threads "
                "(id TEXT PRIMARY KEY, name TEXT, rollout_path TEXT, archived INTEGER)"
            )
            db.execute(
                "INSERT INTO threads(id,name,rollout_path,archived) VALUES (?,?,?,?)",
                (session_id, None, str(rollout), 0),
            )
        (self.root / "session_index.jsonl").write_text(
            json.dumps({"id": session_id, "thread_name": "generated-looking title"})
            + "\n"
        )

        provider = CodexProvider(self.root)
        self.assertEqual(provider.discover()[0].name, "generated-looking title")
        candidates = provider.import_candidates()
        self.assertEqual(candidates, [])
        self.assertEqual([item.name for item in provider.browse_candidates()], ["generated-looking title"])
        self.assertEqual(provider.find_candidates("generated-looking title")[0].session_id, session_id)

    def test_codex_native_name_alone_does_not_prove_rename_intent(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        rollout = self.root / "rollout.jsonl"
        rollout.write_text("{}\n")
        with sqlite3.connect(self.root / "state_current.sqlite") as db:
            db.execute(
                "CREATE TABLE threads "
                "(id TEXT PRIMARY KEY, name TEXT, rollout_path TEXT, archived INTEGER)"
            )
            db.execute(
                "INSERT INTO threads(id,name,rollout_path,archived) VALUES (?,?,?,?)",
                (session_id, "chosen name", str(rollout), 0),
            )

        provider = CodexProvider(self.root)
        self.assertEqual(provider.import_candidates(), [])
        self.assertEqual([item.name for item in provider.browse_candidates()], ["chosen name"])

    def test_codex_automation_origin_is_hidden_without_name_heuristics(self) -> None:
        worker_id = "11111111-1111-4111-8111-111111111111"
        human_id = "22222222-2222-4222-8222-222222222222"
        worker_rollout = self.root / "worker.jsonl"
        human_rollout = self.root / "human.jsonl"
        worker_rollout.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": worker_id,
                        "originator": "agentic_fund",
                        "thread_source": "user",
                    },
                }
            )
            + "\n"
        )
        human_rollout.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": human_id,
                        "originator": "codex-tui",
                        "thread_source": "user",
                    },
                }
            )
            + "\n"
        )
        with sqlite3.connect(self.root / "state_current.sqlite") as db:
            db.execute(
                "CREATE TABLE threads "
                "(id TEXT PRIMARY KEY, name TEXT, rollout_path TEXT, archived INTEGER)"
            )
            db.executemany(
                "INSERT INTO threads(id,name,rollout_path,archived) VALUES (?,?,?,0)",
                (
                    (worker_id, "codex-01a00072", str(worker_rollout)),
                    (human_id, "codex-01a00072", str(human_rollout)),
                ),
            )

        provider = CodexProvider(self.root)
        self.assertEqual([item.session_id for item in provider.discover()], [human_id])
        self.assertFalse(provider.is_resumable(worker_id))
        self.assertTrue(provider.is_resumable(human_id))

    def test_codex_worker_provenance_requires_matching_uuid(self) -> None:
        transcript = self.root / "mismatched.jsonl"
        transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": "different-uuid",
                        "originator": "codex_exec",
                        "source": "exec",
                    },
                }
            )
            + "\n"
        )
        self.assertIsNone(codex_worker_originator("wanted-uuid", transcript))

    def test_codex_exec_is_hidden_as_native_headless_worker(self) -> None:
        worker_id = "33333333-3333-4333-8333-333333333333"
        transcript = self.root / "exec.jsonl"
        transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": worker_id,
                        "originator": "codex_exec",
                        "source": "exec",
                        "thread_source": "user",
                    },
                }
            )
            + "\n"
        )
        with sqlite3.connect(self.root / "state_current.sqlite") as db:
            db.execute(
                "CREATE TABLE threads "
                "(id TEXT PRIMARY KEY, name TEXT, rollout_path TEXT, archived INTEGER)"
            )
            db.execute(
                "INSERT INTO threads(id,name,rollout_path,archived) VALUES (?,?,?,0)",
                (worker_id, "oc_qes_style", str(transcript)),
            )

        provider = CodexProvider(self.root)
        self.assertEqual(
            codex_worker_originator(worker_id, transcript), "codex-exec"
        )
        self.assertEqual(
            codex_worker_originator(
                worker_id, None, originator="codex_exec"
            ),
            "codex-exec",
        )
        self.assertEqual(provider.discover(), [])
        self.assertEqual(provider.import_candidates(), [])
        self.assertFalse(provider.is_resumable(worker_id))

    def test_codex_native_name_uses_official_thread_rpc(self) -> None:
        process = FakeAppServer()
        with (
            patch.object(CodexProvider, "installed", return_value=True),
            patch(
                "pikamux.providers.configured_executable",
                return_value="codex",
            ),
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

    def test_opencode_discovers_only_root_sessions_and_tracks_child_work(self) -> None:
        database = self.root / "opencode.db"
        with sqlite3.connect(database) as db:
            db.execute(
                "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, "
                "title TEXT, directory TEXT, time_created INTEGER, "
                "time_updated INTEGER, time_archived INTEGER, model TEXT, "
                "cost REAL, tokens_input INTEGER, tokens_output INTEGER, "
                "tokens_reasoning INTEGER, tokens_cache_read INTEGER, "
                "tokens_cache_write INTEGER)"
            )
            db.execute(
                "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, "
                "time_created INTEGER, data TEXT)"
            )
            model = json.dumps(
                {"providerID": "opencode", "id": "x-preview-f-free", "variant": "max"}
            )
            db.execute(
                "INSERT INTO session VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                ("ses_root123", None, "oc_qes_style", "/repo", 1000, 2000, None,
                 model, 0, 10, 2, 1, 20, 0),
            )
            db.execute(
                "INSERT INTO session VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                ("ses_child456", "ses_root123", "worker", "/repo", 1500, 3000,
                 None, model, 0, 1, 1, 0, 0, 0),
            )
            db.execute(
                "INSERT INTO session VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                ("ses_blank789", None, "New session - 2026-08-21T00:00:00Z", "/repo",
                 1200, 1200, None, model, 0, 0, 0, 0, 0, 0),
            )
            db.execute(
                "INSERT INTO session VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                ("ses_worker999", None, "agentic-fund:calibration",
                 "/runs/opencode-runtime/worker", 1300, 1300, None, model,
                 0, 0, 0, 0, 0, 0),
            )
            db.execute(
                "INSERT INTO message VALUES (?,?,?,?)",
                ("msg_root", "ses_root123", 2000, json.dumps({
                    "role": "assistant", "time": {"created": 1900, "completed": 2000}
                })),
            )
            db.execute(
                "INSERT INTO message VALUES (?,?,?,?)",
                ("msg_child", "ses_child456", 3000, json.dumps({
                    "role": "user", "time": {"created": 3000}
                })),
            )
        provider = OpenCodeProvider(self.root)
        self.assertEqual(provider.durable_state("ses_root123"), "present")
        self.assertEqual(provider.durable_state("ses_missing123"), "deleted")
        candidates = provider.discover()
        self.assertEqual([item.session_id for item in candidates], ["ses_root123"])
        self.assertEqual(candidates[0].lifecycle_status, "WORKING")
        self.assertEqual(candidates[0].updated_at, 3000)
        self.assertEqual(candidates[0].model, "opencode/x-preview-f-free[max]")
        self.assertTrue(provider.valid_session_id("ses_root123"))
        with patch("pikamux.providers.find_processes_with_session_id", return_value=[99]):
            self.assertEqual(provider.active_pids("ses_root123"), [])
        self.assertEqual(provider.resume_argv("ses_root123")[-2:], ["--session", "ses_root123"])
        self.assertEqual(
            [item.session_id for item in provider.find_candidates("ses_blank789")],
            ["ses_blank789"],
        )
        self.assertIn("ses_worker999", provider.hidden_session_ids())
        usage = provider.usage(Session("opencode", "ses_root123"), Mock())
        self.assertIsNotNone(usage)
        assert usage is not None
        self.assertEqual(usage.total_tokens, 35)
        self.assertEqual(usage.estimated_cost_usd, 0)

        with sqlite3.connect(database) as db:
            db.execute(
                "UPDATE session SET time_updated=4000 WHERE id='ses_child456'"
            )
            db.execute(
                "UPDATE message SET data=? WHERE id='msg_child'",
                (json.dumps({
                    "role": "assistant",
                    "time": {"created": 3000, "completed": 4000},
                }),),
            )
        completed = provider.discover()[0]
        self.assertEqual(completed.lifecycle_status, "READY")
        self.assertEqual(completed.updated_at, 4000)

        with sqlite3.connect(database) as db:
            db.execute(
                "UPDATE session SET time_archived=5000 WHERE id='ses_root123'"
            )
        self.assertEqual(provider.durable_state("ses_root123"), "archived")
        with sqlite3.connect(database) as db:
            db.execute("DELETE FROM session WHERE id='ses_root123'")
        self.assertEqual(provider.durable_state("ses_root123"), "deleted")

    def test_opencode_missing_store_is_unknown_not_deleted(self) -> None:
        self.assertEqual(
            OpenCodeProvider(self.root / "missing").durable_state("ses_root123"),
            "unknown",
        )

    def test_opencode_runtime_refuses_uncertified_binary_version(self) -> None:
        provider = OpenCodeProvider(self.root)
        with (
            patch.object(provider, "executable", return_value="/bin/opencode"),
            patch("pikamux.providers.executable_available", return_value=True),
            patch.object(provider, "version", return_value="1.18.20"),
        ):
            self.assertFalse(provider.installed())
            self.assertIn("requires opencode >= 1.18.21", provider.compatibility_error())
        with (
            patch.object(provider, "executable", return_value="/bin/opencode"),
            patch("pikamux.providers.executable_available", return_value=True),
            patch.object(provider, "version", return_value="1.18.21"),
        ):
            self.assertTrue(provider.installed())


if __name__ == "__main__":
    unittest.main()
