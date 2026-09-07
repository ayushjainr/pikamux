from __future__ import annotations

import json
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from pikamux.core import Pika, PikaError
from pikamux.executables import (
    configured_executable,
    setup_executables,
    setup_runtime_path,
)
from pikamux.models import Candidate, FleetNode, Pane, Session, Status
from pikamux.store import Store


class LaunchProvider:
    name = "codex"

    def __init__(self, candidates: list[Candidate], active: list[int] | None = None):
        self.candidates = candidates
        self.active = active or []
        self.renames: list[tuple[str, str]] = []

    def launch_candidates(self) -> list[Candidate]:
        return self.candidates

    def discover(self) -> list[Candidate]:
        return self.candidates

    def import_candidates(self) -> list[Candidate]:
        return self.candidates

    def find_candidates(self, query: str) -> list[Candidate]:
        folded = query.casefold()
        return [
            item
            for item in self.candidates
            if item.session_id == query
            or (item.name and item.name.casefold() == folded)
        ]

    def is_resumable(self, _session_id: str) -> bool:
        return True

    def hidden_session_ids(self) -> set[str]:
        return set()

    def worker_originator(
        self, _session_id: str, _transcript_path: str | None
    ) -> None:
        return None

    def active_pids(self, _session_id: str) -> list[int]:
        return self.active

    def set_native_name(self, session_id: str, name: str) -> bool:
        self.renames.append((session_id, name))
        return True

    def usage(self, _session: Session, _store: Store):
        return None


class LaunchTmux:
    def __init__(self, pane: Pane):
        self.pane = pane
        self.tags: list[tuple[str, dict[str, object]]] = []
        self.attached: list[str] = []

    def list_panes(self) -> list[Pane]:
        return [self.pane]

    def get_pane(self, target: str) -> Pane | None:
        if target in {self.pane.pane_id, self.pane.session_name}:
            return self.pane
        return None

    def tag_pane(self, target: str, **values: object) -> None:
        self.tags.append((target, values))
        self.pane.pika_provider = str(values.get("provider") or "") or None
        self.pane.pika_session_id = str(values.get("session_id") or "") or None
        self.pane.pika_name = str(values.get("name") or "") or None
        self.pane.pika_launch_token = str(values.get("launch_token") or "") or None

    def attach(self, target: str, **_values: object) -> int:
        self.attached.append(target)
        return 0


class FailingTagTmux(LaunchTmux):
    def tag_pane(self, target: str, **values: object) -> None:
        raise OSError("tag failed")


class LifecycleTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.store = Store(self.root / "pika.db")
        self.token = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        self.session_id = "11111111-1111-4111-8111-111111111111"
        self.started = time.time()
        self.transcript = self.root / "rollout.jsonl"
        self.transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": self.session_id,
                        "cwd": str(self.root),
                        "timestamp": self.started,
                    },
                }
            )
            + "\n"
        )
        self.candidate = Candidate(
            "codex",
            self.session_id,
            None,
            cwd=str(self.root),
            transcript_path=str(self.transcript),
            created_at=self.started,
            updated_at=self.started,
            lifecycle_status=Status.WORKING.value,
        )
        self.pane = Pane(
            "pika-c-aaaaaaaa",
            "%1",
            123,
            str(self.root),
            "codex",
            False,
            False,
            None,
            self.started,
            self.started,
            pika_launch_token=self.token,
            pika_name="qis_dash",
        )

    def tearDown(self) -> None:
        self.temp.cleanup()

    def _pika(
        self,
        *,
        candidates: list[Candidate] | None = None,
        active: list[int] | None = None,
        saved_start: int | None = 99,
    ) -> tuple[Pika, LaunchTmux, LaunchProvider]:
        self.assertTrue(
            self.store.add_pending(
                self.token,
                "codex",
                "qis_dash",
                str(self.root),
                self.pane.session_name,
                self.pane.pane_id,
                preexisting_session_ids=[],
            )
        )
        self.store.finalize_pending_pane(
            self.token,
            self.pane.session_name,
            self.pane.pane_id,
            777 if saved_start is not None else None,
            saved_start,
        )
        provider = LaunchProvider(candidates or [self.candidate], active)
        tmux = LaunchTmux(self.pane)
        return Pika(self.store, tmux, {"codex": provider}), tmux, provider

    def _proof(self, *, token: str | None = None, pid_start: int | None = 99):
        return (
            patch(
                "pikamux.core.provider_process",
                side_effect=lambda _pid, provider: 777 if provider == "codex" else None,
            ),
            patch(
                "pikamux.core.process_environment",
                return_value={
                    "PIKA_LAUNCH_TOKEN": token or self.token,
                    "PIKA_PROVIDER": "codex",
                },
            ),
            patch("pikamux.core.process_start_time", return_value=pid_start),
            patch("pikamux.core.process_tree", return_value=[123, 777, 888]),
            patch("pikamux.store.process_start_time", return_value=pid_start),
        )

    def test_pending_codex_launch_recovers_without_hook(self) -> None:
        pika, tmux, provider = self._pika(active=[777])
        proofs = self._proof()
        with proofs[0], proofs[1], proofs[2], proofs[3], proofs[4]:
            self.assertEqual(pika.reconcile_pending_launches(), 0)
            with self.store.connect() as db:
                db.execute(
                    "UPDATE pending_launches SET candidate_session_id=?,"
                    "candidate_observed_at=? "
                    "WHERE launch_token=?",
                    (
                        self.session_id,
                        self.started + 30,
                        self.token,
                    ),
                )
            with patch("pikamux.core.time.time", return_value=self.started + 61):
                self.assertEqual(pika.reconcile_pending_launches(), 1)
        session = self.store.get_session("codex", self.session_id)
        self.assertIsNotNone(session)
        self.assertEqual(session.name, "qis_dash")
        self.assertEqual(session.tmux_pane, "%1")
        self.assertIsNone(self.store.get_pending(self.token))
        self.assertEqual(tmux.pane.pika_session_id, self.session_id)
        self.assertEqual(provider.renames, [(self.session_id, "qis_dash")])

    def test_ambiguous_launch_candidates_remain_pending(self) -> None:
        other_id = "22222222-2222-4222-8222-222222222222"
        other_transcript = self.root / "other.jsonl"
        other_transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {"id": other_id, "cwd": str(self.root)},
                }
            )
            + "\n"
        )
        other = Candidate(
            "codex",
            other_id,
            None,
            cwd=str(self.root),
            transcript_path=str(other_transcript),
            created_at=self.started,
        )
        pika, tmux, _provider = self._pika(candidates=[self.candidate, other])
        proofs = self._proof()
        with proofs[0], proofs[1], proofs[2], proofs[3], proofs[4]:
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        self.assertIsNotNone(self.store.get_pending(self.token))
        self.assertEqual(tmux.tags, [])

    def test_preexisting_same_cwd_candidate_cannot_claim_new_launch(self) -> None:
        pika, tmux, _provider = self._pika()
        with self.store.connect() as db:
            db.execute(
                "UPDATE pending_launches SET preexisting_session_ids_json=? "
                "WHERE launch_token=?",
                (json.dumps([self.session_id]), self.token),
            )
        proofs = self._proof()
        with proofs[0], proofs[1], proofs[2], proofs[3], proofs[4]:
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        self.assertIsNotNone(self.store.get_pending(self.token))
        self.assertEqual(tmux.tags, [])

    def test_missing_prelaunch_snapshot_disables_codex_inference(self) -> None:
        pika, tmux, _provider = self._pika()
        with self.store.connect() as db:
            db.execute(
                "UPDATE pending_launches SET preexisting_session_ids_json=NULL,"
                "created_at=? WHERE launch_token=?",
                (time.time() - 31, self.token),
            )
        proofs = self._proof()
        with proofs[0], proofs[1], proofs[2], proofs[3], proofs[4]:
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        self.assertIsNotNone(self.store.get_pending(self.token))
        self.assertEqual(tmux.tags, [])

    def test_opencode_launch_never_binds_from_time_and_cwd_inference(self) -> None:
        token = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
        session_id = "ses_concurrent123"
        pane = Pane(
            "pika-o-bbbbbbbbbb",
            "%2",
            124,
            str(self.root),
            "opencode",
            False,
            False,
            None,
            self.started,
            self.started,
            pika_launch_token=token,
            pika_name="oc-safe",
        )
        self.assertTrue(
            self.store.add_pending(
                token,
                "opencode",
                "oc-safe",
                str(self.root),
                pane.session_name,
                pane.pane_id,
                preexisting_session_ids=[],
            )
        )
        self.store.finalize_pending_pane(
            token, pane.session_name, pane.pane_id, 778, 100
        )
        candidate = Candidate(
            "opencode",
            session_id,
            "concurrent",
            cwd=str(self.root),
            created_at=self.started,
            updated_at=self.started,
        )
        provider = LaunchProvider([candidate], [778])
        provider.name = "opencode"
        tmux = LaunchTmux(pane)
        pika = Pika(self.store, tmux, {"opencode": provider})
        with (
            patch("pikamux.core.provider_process", return_value=778),
            patch(
                "pikamux.core.process_environment",
                return_value={
                    "PIKA_LAUNCH_TOKEN": token,
                    "PIKA_PROVIDER": "opencode",
                },
            ),
            patch("pikamux.core.process_start_time", return_value=100),
        ):
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        self.assertIsNotNone(self.store.get_pending(token))
        self.assertIsNone(self.store.get_session("opencode", session_id))
        self.assertEqual(tmux.tags, [])

    def test_pending_attach_requires_launch_environment_proof(self) -> None:
        pika, _tmux, _provider = self._pika()
        with patch(
            "pikamux.core.provider_process",
            side_effect=lambda _pid, provider: 777
            if provider == "codex"
            else None,
        ):
            pending = pika.pending_launches()[0]
        with (
            patch(
                "pikamux.core.provider_process",
                side_effect=lambda _pid, provider: 777
                if provider == "codex"
                else None,
            ),
            patch(
                "pikamux.core.process_environment",
                return_value={
                    "PIKA_LAUNCH_TOKEN": "wrong-token",
                    "PIKA_PROVIDER": "codex",
                },
            ),
            patch("pikamux.core.process_start_time", return_value=99),
            self.assertRaisesRegex(PikaError, "cannot be verified"),
        ):
            pika.open_pending(pending, attach=False)

    def test_wrong_launch_token_remains_pending(self) -> None:
        pika, tmux, _provider = self._pika()
        proofs = self._proof(token="wrong-token")
        with proofs[0], proofs[1], proofs[2], proofs[3], proofs[4]:
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        self.assertIsNotNone(self.store.get_pending(self.token))
        self.assertEqual(tmux.tags, [])

    def test_late_intended_candidate_prevents_early_wrong_binding(self) -> None:
        wrong_id = "22222222-2222-4222-8222-222222222222"
        wrong_transcript = self.root / "wrong.jsonl"
        wrong_transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {"id": wrong_id, "cwd": str(self.root)},
                }
            )
            + "\n"
        )
        wrong = Candidate(
            "codex",
            wrong_id,
            None,
            cwd=str(self.root),
            transcript_path=str(wrong_transcript),
            created_at=self.started + 1,
        )
        pika, tmux, provider = self._pika(candidates=[wrong])
        proofs = self._proof()
        with (
            proofs[0],
            proofs[1],
            proofs[2],
            proofs[3],
            proofs[4],
            patch("pikamux.core.time.time", return_value=self.started + 31),
        ):
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        provider.candidates.append(self.candidate)
        proofs = self._proof()
        with (
            proofs[0],
            proofs[1],
            proofs[2],
            proofs[3],
            proofs[4],
            patch("pikamux.core.time.time", return_value=self.started + 61),
        ):
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        self.assertIsNone(self.store.get_launch_binding(self.token))
        self.assertIsNotNone(self.store.get_pending(self.token))
        self.assertEqual(tmux.tags, [])

    def test_tag_failure_publishes_no_exact_session_or_recovery_owner(self) -> None:
        pika, _tmux, provider = self._pika(active=[777])
        pika.tmux = FailingTagTmux(self.pane)
        self.store.observe_pending_candidate(self.token, self.session_id)
        with self.store.connect() as db:
            db.execute(
                "UPDATE pending_launches SET candidate_observed_at=? "
                "WHERE launch_token=?",
                (self.started + 30, self.token),
            )
        proofs = self._proof()
        with (
            proofs[0],
            proofs[1],
            proofs[2],
            proofs[3],
            proofs[4],
            patch("pikamux.core.time.time", return_value=self.started + 61),
        ):
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        self.assertIsNotNone(self.store.get_pending(self.token))
        self.assertIsNone(self.store.get_session("codex", self.session_id))
        self.assertIsNone(
            self.store.get_recovery_owner("codex", self.session_id)
        )
        self.assertEqual(provider.renames, [])

    def test_pid_reuse_remains_pending(self) -> None:
        pika, tmux, _provider = self._pika(saved_start=98)
        proofs = self._proof(pid_start=99)
        with proofs[0], proofs[1], proofs[2], proofs[3], proofs[4]:
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        self.assertIsNotNone(self.store.get_pending(self.token))
        self.assertEqual(tmux.tags, [])

    def test_two_exact_uuid_processes_remain_fail_closed(self) -> None:
        pika, tmux, _provider = self._pika(active=[777, 888])
        proofs = self._proof()
        with (
            proofs[0],
            proofs[1],
            proofs[2],
            proofs[3],
            proofs[4],
            patch("pikamux.core.time.time", return_value=self.started + 61),
        ):
            self.assertEqual(pika.reconcile_pending_launches(), 0)
        self.assertIsNotNone(self.store.get_pending(self.token))
        self.assertEqual(tmux.tags, [])

    def test_two_uuid_clients_inside_one_tagged_pane_report_open_twice(self) -> None:
        self.pane.pika_provider = "codex"
        self.pane.pika_session_id = self.session_id
        self.pane.pika_launch_token = None
        session = Session(
            "codex",
            self.session_id,
            name="qis_dash",
            cwd=str(self.root),
            tmux_session=self.pane.session_name,
            tmux_pane=self.pane.pane_id,
        )
        self.store.upsert_session(session)
        pika = Pika(
            self.store,
            LaunchTmux(self.pane),
            {"codex": LaunchProvider([self.candidate], active=[777, 888])},
        )
        with (
            patch.object(pika, "refresh", return_value=[session]),
            patch(
                "pikamux.core.provider_process",
                side_effect=lambda _pid, provider: 777
                if provider == "codex"
                else None,
            ),
            patch("pikamux.core.process_tree", return_value=[123, 777, 888]),
            self.assertRaisesRegex(PikaError, "OPEN TWICE"),
        ):
            pika.open(session, attach=False)

    def test_pending_launch_is_visible_then_becomes_attention(self) -> None:
        pika, _tmux, _provider = self._pika()
        with patch("pikamux.core.provider_process", return_value=777):
            pending = pika.pending_launches()
        self.assertEqual(pending[0].status, Status.STARTING.value)
        self.assertFalse(pending[0].needs_attention)
        with self.store.connect() as db:
            db.execute(
                "UPDATE pending_launches SET created_at=? WHERE launch_token=?",
                (time.time() - 120, self.token),
            )
        with patch("pikamux.core.provider_process", return_value=777):
            pending = pika.pending_launches()
        self.assertEqual(pending[0].status, Status.ERROR.value)
        self.assertTrue(pending[0].needs_attention)

    def test_launch_binding_is_insert_or_confirm_never_overwrite(self) -> None:
        self.assertTrue(self.store.bind_launch(self.token, "codex", self.session_id))
        self.assertTrue(self.store.bind_launch(self.token, "codex", self.session_id))
        self.assertFalse(
            self.store.bind_launch(
                self.token,
                "codex",
                "22222222-2222-4222-8222-222222222222",
            )
        )
        self.assertEqual(
            self.store.get_launch_binding(self.token), ("codex", self.session_id)
        )

    def test_universal_entry_opens_exact_match(self) -> None:
        pika, _tmux, _provider = self._pika()
        session = Session("codex", self.session_id, name="qis_dash")
        with (
            patch.object(pika, "refresh", return_value=[session]),
            patch.object(pika, "pending_launches", return_value=[]),
            patch.object(pika.store, "list_untracked_sessions", return_value=[]),
            patch.object(
                pika, "discover_exact_candidates", return_value=[]
            ) as broad_discovery,
            patch.object(pika.fleet, "cached_sessions", return_value=[]),
            patch.object(pika, "open", return_value=17) as opened,
        ):
            self.assertEqual(pika.enter("qis_dash"), 17)
        opened.assert_called_once_with(session, attach=True)
        broad_discovery.assert_not_called()

    def test_provider_confirmed_deleted_opencode_row_leaves_board(self) -> None:
        provider = LaunchProvider([])
        provider.name = "opencode"
        provider.durable_state = Mock(return_value="deleted")
        session = Session(
            "opencode",
            "ses_deleted123",
            name="deleted-open-code",
            cwd=str(self.root),
            status=Status.READY.value,
            unread=True,
        )
        self.store.upsert_session(session)
        tmux = Mock()
        tmux.list_panes.return_value = []
        pika = Pika(self.store, tmux, {"opencode": provider})
        self.assertEqual(pika.refresh(), [])
        self.assertIsNone(self.store.get_session(*session.key))

    def test_unknown_opencode_store_state_never_deletes_row(self) -> None:
        provider = LaunchProvider([])
        provider.name = "opencode"
        provider.durable_state = Mock(return_value="unknown")
        session = Session(
            "opencode", "ses_unknown123", name="keep-me", cwd=str(self.root)
        )
        self.store.upsert_session(session)
        tmux = Mock()
        tmux.list_panes.return_value = []
        pika = Pika(self.store, tmux, {"opencode": provider})
        pika.refresh()
        self.assertIsNotNone(self.store.get_session(*session.key))

    def test_universal_entry_tags_live_untagged_codex_pane(self) -> None:
        self.pane.pika_launch_token = None
        candidate = self.candidate
        candidate.name = "native_codex"
        candidate.live = False
        candidate.pid = None
        provider = LaunchProvider([candidate], active=[777])
        tmux = LaunchTmux(self.pane)
        pika = Pika(self.store, tmux, {"codex": provider})
        with (
            patch(
                "pikamux.core.provider_process",
                side_effect=lambda _pid, provider_name: 777
                if provider_name == "codex"
                else None,
            ),
            patch("pikamux.core.process_tree", return_value=[123, 777]),
        ):
            self.assertEqual(pika.enter("native_codex"), 0)
        session = self.store.get_session("codex", self.session_id)
        self.assertIsNotNone(session)
        self.assertEqual(session.tmux_pane, self.pane.pane_id)
        self.assertEqual(tmux.pane.pika_provider, "codex")
        self.assertEqual(tmux.pane.pika_session_id, self.session_id)
        self.assertEqual(tmux.attached, [self.pane.session_name])

    def test_universal_entry_asks_on_live_cross_provider_name_collision(self) -> None:
        pika, _tmux, _provider = self._pika()
        codex = Session("codex", self.session_id, name="shared")
        claude = Candidate(
            "claude",
            "22222222-2222-4222-8222-222222222222",
            "shared",
            cwd=str(self.root),
            live=True,
        )
        pika._last_discovered_candidates = [claude]
        with (
            patch.object(pika, "refresh", return_value=[codex]),
            patch.object(pika, "pending_launches", return_value=[]),
            patch.object(pika.store, "list_untracked_sessions", return_value=[]),
            patch.object(pika.fleet, "cached_sessions", return_value=[]),
            patch("pikamux.core.choose_session", return_value=codex) as chooser,
            patch.object(pika, "open", return_value=0),
        ):
            self.assertEqual(pika.enter("shared"), 0)
        choices = chooser.call_args.args[0]
        self.assertEqual({item.provider for item in choices}, {"codex", "claude"})

    def test_universal_entry_creates_only_when_name_is_unambiguous(self) -> None:
        pika, _tmux, _provider = self._pika()
        existing = Session("codex", self.session_id, name="qis_dash")
        common = (
            patch.object(pika, "pending_launches", return_value=[]),
            patch.object(pika.store, "list_untracked_sessions", return_value=[]),
            patch.object(pika, "discover_exact_candidates", return_value=[]),
            patch.object(pika.fleet, "cached_sessions", return_value=[]),
        )
        with (
            patch.object(pika, "refresh", return_value=[existing]),
            common[0],
            common[1],
            common[2],
            common[3],
            patch("pikamux.core.sys.stdin.isatty", return_value=True),
            patch.object(pika, "new", return_value=23) as created,
        ):
            self.assertEqual(pika.enter("fresh_name"), 23)
            with self.assertRaisesRegex(PikaError, "Close matches"):
                pika.enter("qis-dash")
        created.assert_called_once_with("fresh_name", attach=True)

    def test_universal_entry_never_creates_in_noninteractive_context(self) -> None:
        pika, _tmux, _provider = self._pika()
        with (
            patch.object(pika, "refresh", return_value=[]),
            patch.object(pika, "pending_launches", return_value=[]),
            patch.object(pika.store, "list_untracked_sessions", return_value=[]),
            patch.object(pika, "discover_exact_candidates", return_value=[]),
            patch.object(pika.fleet, "cached_sessions", return_value=[]),
            patch("pikamux.core.sys.stdin.isatty", return_value=False),
            patch.object(pika, "new") as created,
            self.assertRaisesRegex(PikaError, "will not create one implicitly"),
        ):
            pika.enter("script_typo")
        created.assert_not_called()

    def test_universal_entry_blocks_creation_on_stale_remote_inventory(self) -> None:
        pika, _tmux, _provider = self._pika()
        self.store.upsert_fleet_node(
            FleetNode(
                "22222222-2222-4222-8222-222222222222",
                "atlas",
                "atlas",
                status="unreachable",
            )
        )
        with (
            patch.object(pika, "refresh", return_value=[]),
            patch.object(pika, "pending_launches", return_value=[]),
            patch.object(pika.store, "list_untracked_sessions", return_value=[]),
            patch.object(pika, "discover_exact_candidates", return_value=[]),
            patch.object(pika.fleet, "cached_sessions", return_value=[]),
            patch("pikamux.core.sys.stdin.isatty", return_value=True),
            patch.object(pika, "new") as created,
            self.assertRaisesRegex(PikaError, "remote inventory is stale.*atlas"),
        ):
            pika.enter("new_local_name")
        created.assert_not_called()

    def test_pinned_provider_executable_does_not_follow_later_path_changes(
        self,
    ) -> None:
        config = {"provider_executables": {"codex": "/opt/codex-a"}}
        with patch("pikamux.executables.shutil.which", return_value="/opt/codex-b"):
            selected = setup_executables(config)
            current = configured_executable("codex", config=config)
        self.assertEqual(selected["codex"], "/opt/codex-a")
        self.assertEqual(current, "/opt/codex-a")

    def test_runtime_path_adds_an_explicitly_replaced_provider_binary(self) -> None:
        value = setup_runtime_path(
            {"provider_runtime_path": "/old/bin:/usr/bin"},
            {"codex": "/new/codex/bin/codex"},
        )
        self.assertEqual(
            value.split(":"), ["/new/codex/bin", "/old/bin", "/usr/bin"]
        )


if __name__ == "__main__":
    unittest.main()
