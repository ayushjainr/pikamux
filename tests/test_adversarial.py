from __future__ import annotations

import io
import json
import os
import tempfile
import time
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

from pikamux import __version__
from pikamux.core import OPEN_TWICE_ERROR, OutsideLiveConflict, Pika, PikaError
from pikamux.doctor import repair_stale_state, run_doctor
from pikamux.models import Candidate, Pane, Session, Status
from pikamux.providers import ClaudeProvider, CodexProvider
from pikamux.setup_hooks import hook_spec_fingerprint
from pikamux.store import Store


def pane(
    *,
    pane_id: str = "%1",
    provider: str | None = None,
    session_id: str | None = None,
) -> Pane:
    return Pane(
        session_name="manual",
        pane_id=pane_id,
        pane_pid=123,
        cwd="/tmp",
        current_command="bash",
        attached=False,
        dead=False,
        dead_status=None,
        activity=time.time(),
        created=time.time(),
        pika_provider=provider,
        pika_session_id=session_id,
    )


class StaticTmux:
    def __init__(
        self,
        panes: list[Pane] | None = None,
        *,
        attach_result: int = 0,
    ):
        self.panes = panes or []
        self.attach_result = attach_result
        self.tags: list[tuple[str, dict]] = []
        self.receipts: list[str] = []
        self.cleared: list[str] = []

    def list_panes(self) -> list[Pane]:
        return self.panes

    def get_pane(self, target: str) -> Pane | None:
        return next(
            (
                item
                for item in self.panes
                if target in {item.pane_id, item.session_name}
            ),
            None,
        )

    def available(self) -> bool:
        return True

    @staticmethod
    def internal_name(provider, session_id=None, token=None):
        return f"pika-{provider[0]}-{(session_id or token or 'session')[:8]}"

    def attach(
        self,
        _target: str,
        *,
        target_pane=None,
        on_attached=None,
        receipt=None,
    ) -> int:
        if self.attach_result == 0 and on_attached:
            on_attached()
        if self.attach_result == 0 and receipt:
            self.receipts.append(receipt() if callable(receipt) else receipt)
        return self.attach_result

    def tag_pane(self, target: str, **values) -> None:
        self.tags.append((target, values))

    def clear_pika_tags(self, target: str) -> None:
        self.cleared.append(target)
        for item in self.panes:
            if item.pane_id == target:
                item.pika_provider = None
                item.pika_session_id = None
                item.pika_name = None

    def create_agent_session(
        self,
        *,
        tmux_name,
        cwd,
        provider,
        session_id,
        display_name,
        **_values,
    ) -> Pane:
        created = Pane(
            tmux_name,
            "%2",
            456,
            cwd,
            provider,
            False,
            False,
            None,
            time.time(),
            time.time(),
            provider,
            session_id,
            display_name,
        )
        self.panes.append(created)
        return created


class FakeProvider:
    def __init__(
        self,
        name: str = "codex",
        candidates: list[Candidate] | None = None,
        active: list[int] | None = None,
        resumable: bool = True,
        hidden: set[str] | None = None,
        workers: dict[str, str] | None = None,
    ):
        self.name = name
        self.candidates = candidates or []
        self.active = active or []
        self.resumable = resumable
        self.hidden = hidden or set()
        self.workers = workers or {}

    def discover(self) -> list[Candidate]:
        return self.candidates

    def import_candidates(self) -> list[Candidate]:
        return self.candidates

    def hidden_session_ids(self) -> set[str]:
        return self.hidden

    def worker_originator(
        self, session_id: str, _transcript_path: str | None
    ) -> str | None:
        return self.workers.get(session_id)

    def active_pids(self, _session_id: str) -> list[int]:
        return self.active

    def is_resumable(self, _session_id: str) -> bool:
        return self.resumable

    def usage(self, _session, _store):
        return None

    def installed(self) -> bool:
        return True

    def version(self) -> str:
        return "fake 1.0"

    def resume_argv(self, session_id: str) -> list[str]:
        return [self.name, "resume", session_id]


class AdversarialTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.store = Store(self.root / "pika.db")

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_idle_pane_accepts_tmux_fallback_shell_chain_but_not_live_job(
        self,
    ) -> None:
        item = pane()
        with (
            patch("pikamux.core.process_tree", return_value=[123, 456]),
            patch(
                "pikamux.core.cmdline",
                side_effect=lambda pid: ["/bin/bash", "-l"],
            ),
        ):
            self.assertTrue(Pika._pane_is_idle(item))
        with (
            patch("pikamux.core.process_tree", return_value=[123, 456, 789]),
            patch(
                "pikamux.core.cmdline",
                side_effect=lambda pid: (
                    ["/bin/bash", "-l"] if pid != 789 else ["sleep", "30"]
                ),
            ),
        ):
            self.assertFalse(Pika._pane_is_idle(item))
        with (
            patch("pikamux.core.process_tree", return_value=[123, 456, 789]),
            patch(
                "pikamux.core.cmdline",
                side_effect=lambda pid: ["/bin/bash", "-l"] if pid != 789 else [],
            ),
            patch("pikamux.core.process_state", return_value="Z"),
        ):
            self.assertTrue(Pika._pane_is_idle(item))
        with (
            patch("pikamux.core.process_tree", return_value=[123, 456, 789]),
            patch(
                "pikamux.core.cmdline",
                side_effect=lambda pid: ["/bin/bash", "-l"] if pid != 789 else [],
            ),
            patch("pikamux.core.process_state", return_value="S"),
        ):
            self.assertFalse(Pika._pane_is_idle(item))

    def test_refresh_prunes_proven_worker_but_preserves_parent_run(self) -> None:
        worker_id = "11111111-1111-4111-8111-111111111111"
        parent_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session(
                "codex",
                worker_id,
                name="codex-01a00072",
                status=Status.READY.value,
                unread=True,
            )
        )
        self.store.upsert_session(Session("codex", parent_id, name="learning-study-v3"))
        pika = Pika(
            self.store,
            StaticTmux(),
            {"codex": FakeProvider(workers={worker_id: "agentic_fund"})},
        )

        refreshed = pika.refresh()

        self.assertEqual([item.session_id for item in refreshed], [parent_id])
        self.assertIsNone(self.store.get_session("codex", worker_id))
        self.assertIsNotNone(self.store.get_session("codex", parent_id))

    def test_codex_exec_worker_cannot_shadow_same_named_opencode_workstream(
        self,
    ) -> None:
        codex_home = self.root / "codex"
        codex_home.mkdir()
        worker_id = "11111111-1111-4111-8111-111111111111"
        opencode_id = "ses_real_qes_style"
        transcript = codex_home / "codex-exec-worker.jsonl"
        transcript_bytes = (
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": worker_id,
                        "originator": "codex_exec",
                        "source": "exec",
                    },
                }
            )
            + "\n"
        ).encode()
        transcript.write_bytes(transcript_bytes)
        self.store.upsert_session(
            Session(
                "codex",
                worker_id,
                name="oc_qes_style",
                transcript_path=str(transcript),
                status=Status.READY.value,
                unread=True,
            )
        )
        self.store.upsert_session(
            Session(
                "opencode",
                opencode_id,
                name="oc_qes_style",
                cwd="/tmp",
                status=Status.WORKING.value,
            )
        )
        opencode_candidate = Candidate(
            "opencode",
            opencode_id,
            name="oc_qes_style",
            cwd="/tmp",
        )
        pika = Pika(
            self.store,
            StaticTmux(),
            {
                "codex": CodexProvider(codex_home),
                "opencode": FakeProvider(
                    name="opencode", candidates=[opencode_candidate]
                ),
            },
        )

        refreshed = pika.refresh()

        self.assertIsNone(self.store.get_session("codex", worker_id))
        self.assertEqual(
            [(item.provider, item.session_id, item.name) for item in refreshed],
            [("opencode", opencode_id, "oc_qes_style")],
        )
        self.assertEqual(transcript.read_bytes(), transcript_bytes)

    def test_refresh_prunes_claude_sdk_worker_without_touching_same_name_thread(
        self,
    ) -> None:
        claude_home = self.root / "claude"
        project = claude_home / "projects" / "repo"
        project.mkdir(parents=True)
        worker_id = "11111111-1111-4111-8111-111111111111"
        interactive_id = "22222222-2222-4222-8222-222222222222"
        worker_path = project / f"{worker_id}.jsonl"
        interactive_path = project / f"{interactive_id}.jsonl"
        worker_bytes = (
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
        ).encode()
        interactive_bytes = (
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
        ).encode()
        worker_path.write_bytes(worker_bytes)
        interactive_path.write_bytes(interactive_bytes)
        self.store.upsert_session(
            Session(
                "claude",
                worker_id,
                name="sample_plugin",
                transcript_path=str(worker_path),
                status=Status.READY.value,
                unread=True,
            )
        )
        self.store.upsert_session(
            Session(
                "claude",
                interactive_id,
                name="sample_plugin",
                transcript_path=str(interactive_path),
                status=Status.WORKING.value,
            )
        )
        pika = Pika(
            self.store,
            StaticTmux(),
            {"claude": ClaudeProvider(claude_home)},
        )

        refreshed = pika.refresh()

        self.assertEqual([item.session_id for item in refreshed], [interactive_id])
        self.assertIsNone(self.store.get_session("claude", worker_id))
        self.assertIsNotNone(self.store.get_session("claude", interactive_id))
        self.assertEqual(worker_path.read_bytes(), worker_bytes)
        self.assertEqual(interactive_path.read_bytes(), interactive_bytes)

    def test_resolve_promotes_native_name_from_empty_ledger(self) -> None:
        candidate = Candidate(
            provider="codex",
            session_id="11111111-1111-4111-8111-111111111111",
            name="native-name",
            cwd="/tmp",
        )
        pika = Pika(
            self.store,
            StaticTmux(),
            {"codex": FakeProvider(candidates=[candidate])},
        )
        resolved = pika.resolve("native-name")
        self.assertEqual(resolved.session_id, candidate.session_id)
        self.assertIsNotNone(self.store.get_session("codex", candidate.session_id))

    @patch("pikamux.core.process_tree", side_effect=lambda pid: {pid})
    @patch("pikamux.core.provider_process", side_effect=lambda pid, _provider: pid)
    def test_provider_rename_updates_ledger_and_recovery_tag(
        self, _provider_process, _process_tree
    ) -> None:
        session_id = "12121212-1212-4212-8212-121212121212"
        self.store.upsert_session(
            Session("codex", session_id, name="old-name", tmux_pane="%1")
        )
        old_pane = pane(provider="codex", session_id=session_id)
        old_pane.pika_name = "old-name"
        tmux = StaticTmux([old_pane])
        candidate = Candidate(
            "codex",
            session_id,
            name="new-name",
            live=True,
            pid=123,
            updated_at=time.time(),
        )
        pika = Pika(
            self.store,
            tmux,
            {"codex": FakeProvider(candidates=[candidate], active=[123])},
        )

        refreshed = pika.refresh()

        self.assertEqual(refreshed[0].name, "new-name")
        self.assertIn(("%1", {"name": "new-name"}), tmux.tags)

    def test_untrack_clears_tags_and_stays_hidden_until_explicit_reopen(self) -> None:
        session_id = "13131313-1313-4313-8313-131313131313"
        tracked = Session(
            "codex", session_id, name="quiet-work", tmux_pane="%1", managed=True
        )
        self.store.upsert_session(tracked)
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(
            self.store,
            tmux,
            {"codex": FakeProvider(candidates=[])},
        )

        self.assertEqual(pika.untrack(tracked), 1)
        self.assertEqual(tmux.cleared, ["%1"])
        self.assertEqual(pika.refresh(), [])
        self.assertTrue(self.store.is_untracked("codex", session_id))

        restored = pika.resolve(session_id)
        self.assertEqual(restored.session_id, session_id)
        self.assertFalse(self.store.is_untracked("codex", session_id))

    def test_untrack_refuses_placeholder_without_exact_uuid(self) -> None:
        placeholder = Session(
            "codex", "unbound:%9", name="unknown", tmux_pane="%9"
        )
        self.store.upsert_session(placeholder)
        pika = Pika(
            self.store,
            StaticTmux([pane(pane_id="%9")]),
            {"codex": FakeProvider()},
        )
        with self.assertRaisesRegex(PikaError, "provider identity is not known"):
            pika.untrack(placeholder)
        self.assertIsNotNone(self.store.get_session(*placeholder.key))

    def test_provider_hidden_session_stays_out_of_refresh_and_pane_recovery(
        self,
    ) -> None:
        archived_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session("codex", archived_id, name="research-notes", cwd="/tmp")
        )
        tmux = StaticTmux([pane(provider="codex", session_id=archived_id)])
        pika = Pika(
            self.store,
            tmux,
            {"codex": FakeProvider(hidden={archived_id})},
        )

        self.assertEqual(pika.refresh(), [])
        self.assertIsNotNone(self.store.get_session("codex", archived_id))

    def test_setup_import_excludes_unresumable_history_but_keeps_live_work(
        self,
    ) -> None:
        stale = Candidate(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="stale",
        )
        live = Candidate(
            "claude",
            "22222222-2222-4222-8222-222222222222",
            name=None,
            live=True,
        )
        pika = Pika(
            self.store,
            StaticTmux(),
            {
                "codex": FakeProvider("codex", [stale], resumable=False),
                "claude": FakeProvider("claude", [live], resumable=False),
            },
        )

        self.assertEqual(pika.discover_import_candidates(), [live])

    def test_cross_provider_native_collision_uses_chooser(self) -> None:
        candidates = [
            Candidate(
                provider=name,
                session_id=session_id,
                name="same-name",
                cwd="/tmp",
            )
            for name, session_id in (
                ("codex", "11111111-1111-4111-8111-111111111111"),
                ("claude", "22222222-2222-4222-8222-222222222222"),
            )
        ]
        pika = Pika(
            self.store,
            StaticTmux(),
            {
                "codex": FakeProvider("codex", [candidates[0]]),
                "claude": FakeProvider("claude", [candidates[1]]),
            },
        )
        with patch(
            "pikamux.core.choose_session", side_effect=lambda items, _prompt: items[1]
        ) as chooser:
            resolved = pika.resolve("same-name")
        self.assertEqual(resolved.provider, "claude")
        self.assertEqual(len(chooser.call_args.args[0]), 2)

    def test_untagged_tmux_owner_is_not_excluded_from_duplicate_check(self) -> None:
        session = Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="owned elsewhere",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        pika = Pika(
            self.store,
            StaticTmux([pane()]),
            {"codex": FakeProvider(active=[999])},
        )
        with patch("pikamux.core.provider_process", return_value=999), self.assertRaises(
            PikaError
        ) as raised:
            pika.open(session, attach=False)
        message = str(raised.exception)
        self.assertIn("already running outside", message)
        self.assertIn("run exactly: `pika 'owned elsewhere'`", message)

    def test_single_exact_process_outside_tmux_offers_clean_and_attach(self) -> None:
        session = Session(
            "claude",
            "11111111-1111-4111-8111-111111111111",
            name="sample_plugin",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        pika = Pika(
            self.store,
            StaticTmux(),
            {"claude": FakeProvider("claude", active=[999])},
        )
        with (
            patch("pikamux.core.process_start_time", return_value=4242),
            patch("pikamux.core.shared_provider_process", return_value=False),
            self.assertRaises(OutsideLiveConflict) as raised,
        ):
            pika.open(session, attach=False)

        self.assertEqual(raised.exception.session.key, session.key)
        self.assertEqual(raised.exception.process_identities, ((999, 4242),))
        self.assertIn("same UUID", str(raised.exception))

    def test_clean_and_attach_pins_generation_stops_once_and_recovers(self) -> None:
        session = Session(
            "claude",
            "22222222-2222-4222-8222-222222222222",
            name="sample_plugin",
            cwd="/tmp",
        )
        conflict = OutsideLiveConflict("outside", session, ((999, 4242),))
        pika = Pika(self.store, StaticTmux(), {"claude": FakeProvider("claude")})

        with (
            patch.object(pika, "refresh", return_value=[session]),
            patch.object(pika, "uuid_identity_pids", side_effect=[{999}, set()]),
            patch("pikamux.core.process_start_time", return_value=4242),
            patch("pikamux.core.shared_provider_process", return_value=False),
            patch("pikamux.core.os.pidfd_open", return_value=55) as pidfd_open,
            patch("pikamux.core.signal.pidfd_send_signal") as send_signal,
            patch("pikamux.core.select.select", return_value=([55], [], [])),
            patch("pikamux.core.os.close") as close,
            patch.object(
                pika, "recover_after_closed_confirmation", return_value=17
            ) as recover,
        ):
            self.assertEqual(pika.clean_and_attach(conflict, attach=False), 17)

        pidfd_open.assert_called_once_with(999, 0)
        send_signal.assert_called_once()
        self.assertEqual(send_signal.call_args.args[:2], (55, 15))
        close.assert_called_once_with(55)
        recover.assert_called_once_with(session, attach=False)

    def test_clean_and_attach_rejects_pid_reuse_before_signal(self) -> None:
        session = Session("claude", "uuid", name="changed")
        conflict = OutsideLiveConflict("outside", session, ((999, 4242),))
        pika = Pika(self.store, StaticTmux(), {"claude": FakeProvider("claude")})

        with (
            patch.object(pika, "refresh", return_value=[session]),
            patch.object(pika, "uuid_identity_pids", return_value={999}),
            patch("pikamux.core.process_start_time", return_value=4343),
            patch("pikamux.core.os.pidfd_open") as pidfd_open,
            self.assertRaisesRegex(PikaError, "PID generation changed"),
        ):
            pika.clean_and_attach(conflict, attach=False)
        pidfd_open.assert_not_called()

    def test_clean_and_attach_never_targets_shared_infrastructure(self) -> None:
        session = Session("codex", "uuid", name="shared")
        conflict = OutsideLiveConflict("outside", session, ((999, 4242),))
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})

        with (
            patch.object(pika, "refresh", return_value=[session]),
            patch.object(pika, "uuid_identity_pids", return_value={999}),
            patch("pikamux.core.process_start_time", return_value=4242),
            patch("pikamux.core.shared_provider_process", return_value=True),
            patch("pikamux.core.os.pidfd_open") as pidfd_open,
            self.assertRaisesRegex(PikaError, "shared provider infrastructure"),
        ):
            pika.clean_and_attach(conflict, attach=False)
        pidfd_open.assert_not_called()

    def test_clean_and_attach_rechecks_that_process_is_still_outside_tmux(self) -> None:
        session = Session("claude", "uuid", name="moved")
        conflict = OutsideLiveConflict("outside", session, ((999, 4242),))
        tmux = StaticTmux([pane()])
        pika = Pika(self.store, tmux, {"claude": FakeProvider("claude")})

        with (
            patch.object(pika, "refresh", return_value=[session]),
            patch.object(pika, "uuid_identity_pids", return_value={999}),
            patch("pikamux.core.process_start_time", return_value=4242),
            patch("pikamux.core.shared_provider_process", return_value=False),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
            patch("pikamux.core.os.pidfd_open") as pidfd_open,
            self.assertRaisesRegex(PikaError, "now inside tmux"),
        ):
            pika.clean_and_attach(conflict, attach=False)
        pidfd_open.assert_not_called()

    def test_clean_and_attach_never_escalates_after_graceful_timeout(self) -> None:
        session = Session("claude", "uuid", name="busy")
        conflict = OutsideLiveConflict("outside", session, ((999, 4242),))
        pika = Pika(self.store, StaticTmux(), {"claude": FakeProvider("claude")})

        with (
            patch.object(pika, "refresh", return_value=[session]),
            patch.object(pika, "uuid_identity_pids", return_value={999}),
            patch("pikamux.core.process_start_time", return_value=4242),
            patch("pikamux.core.shared_provider_process", return_value=False),
            patch("pikamux.core.os.pidfd_open", return_value=55),
            patch("pikamux.core.signal.pidfd_send_signal") as send_signal,
            patch("pikamux.core.select.select", return_value=([], [], [])),
            patch("pikamux.core.os.close"),
            patch.object(pika, "recover_after_closed_confirmation") as recover,
            self.assertRaisesRegex(PikaError, "did not force-kill"),
        ):
            pika.clean_and_attach(conflict, attach=False, timeout=0.1)
        send_signal.assert_called_once()
        recover.assert_not_called()

    def test_hook_live_owner_blocks_duplicate_even_when_process_argv_is_opaque(
        self,
    ) -> None:
        session = Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="renamed-later",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        owner_pid = os.getpid()
        self.store.set_live_owner("codex", session.session_id, owner_pid)
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})

        def provider_at_root(pid, provider=None):
            return 999 if pid == owner_pid and provider == "codex" else None

        with (
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
            self.assertRaisesRegex(PikaError, "already running outside"),
        ):
            pika.open(session, attach=False)

    def test_shared_app_lease_names_the_real_blocker_and_recovery(self) -> None:
        session = Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="recover me",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        owner_pid = os.getpid()
        self.store.set_live_owner("codex", session.session_id, owner_pid)
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})

        def provider_at_root(pid, provider=None):
            return 999 if pid == owner_pid and provider == "codex" else None

        with (
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
            patch("pikamux.core.shared_provider_process", return_value=True),
            self.assertRaises(PikaError) as raised,
        ):
            pika.open(session, attach=False)

        message = str(raised.exception)
        self.assertIn("CODEX CLIENT STATE AMBIGUOUS", message)
        self.assertIn("needs one confirmation", message)
        self.assertIn("Run exactly: `pika 'recover me'`", message)
        self.assertIn("Do not kill PID", message)
        self.assertNotIn("Exit that copy normally", message)

    def test_confirmed_recovery_revokes_only_shared_lease_and_resumes(self) -> None:
        session = Session(
            "codex",
            "15151515-1515-4515-8515-151515151515",
            name="closed client",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        owner_pid = os.getpid()
        self.assertTrue(
            self.store.set_live_owner("codex", session.session_id, owner_pid)
        )
        tmux = StaticTmux()
        pika = Pika(self.store, tmux, {"codex": FakeProvider()})

        def provider_at_root(pid, provider=None):
            if provider != "codex":
                return None
            if pid == owner_pid:
                return 777
            if pid == 456:
                return 888
            return None

        with (
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
            patch(
                "pikamux.core.shared_provider_process",
                side_effect=lambda pid, provider: provider == "codex" and pid == 777,
            ),
        ):
            self.assertEqual(
                pika.recover_after_closed_confirmation(session, attach=False), 0
            )

        self.assertEqual(self.store.get_live_owners(*session.key), [])
        self.assertEqual(len(tmux.panes), 1)
        self.assertEqual(tmux.panes[0].pika_session_id, session.session_id)

    def test_named_cli_client_gets_exact_exit_then_recovery_steps(self) -> None:
        session = Session(
            "codex",
            "17171717-1717-4717-8717-171717171717",
            name="named live",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        owner_pid = os.getpid()
        self.assertTrue(
            self.store.set_live_owner("codex", session.session_id, owner_pid)
        )
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})

        def provider_at_root(pid, provider=None):
            return 777 if pid == owner_pid and provider == "codex" else None

        with (
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
            patch("pikamux.core.shared_provider_process", return_value=True),
            patch("pikamux.core.find_processes_with_session_id", return_value=[654]),
        ):
            with self.assertRaises(PikaError) as open_error:
                pika.open(session, attach=False)
            with self.assertRaisesRegex(PikaError, "RECOVERY REFUSED"):
                pika.recover_after_closed_confirmation(session, attach=False)

        message = str(open_error.exception)
        self.assertIn("ACTIVE IN CODEX CLI", message)
        self.assertIn("run `/exit` and wait for the shell prompt", message)
        self.assertIn("run exactly: `pika 'named live'`", message)

    def test_confirmed_recovery_remains_fail_closed_for_dedicated_process(self) -> None:
        session = Session(
            "codex",
            "16161616-1616-4616-8616-161616161616",
            name="still open",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        pika = Pika(
            self.store,
            StaticTmux(),
            {"codex": FakeProvider(active=[999])},
        )
        with self.assertRaisesRegex(PikaError, "RECOVERY REFUSED"):
            pika.recover_after_closed_confirmation(session, attach=False)

    def test_confirmed_recovery_rechecks_exact_uuid_before_launch(self) -> None:
        session = Session(
            "codex",
            "18181818-1818-4818-8818-181818181818",
            name="late exact",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        owner_pid = os.getpid()
        self.assertTrue(self.store.set_live_owner(*session.key, owner_pid))
        provider = FakeProvider()
        tmux = StaticTmux()
        pika = Pika(self.store, tmux, {"codex": provider})
        original_outside = pika._outside_processes
        outside_calls = 0

        def staged_outside(current, panes):
            nonlocal outside_calls
            outside_calls += 1
            result = original_outside(current, panes)
            if outside_calls == 2:
                provider.active = [888]
            return result

        with (
            patch(
                "pikamux.core.provider_process",
                side_effect=lambda pid, provider=None: (
                    777 if pid == owner_pid and provider == "codex" else None
                ),
            ),
            patch(
                "pikamux.core.shared_provider_process",
                side_effect=lambda pid, provider: provider == "codex" and pid == 777,
            ),
            patch.object(pika, "_outside_processes", side_effect=staged_outside),
            self.assertRaisesRegex(PikaError, "already running outside"),
        ):
            pika.recover_after_closed_confirmation(session, attach=False)

        self.assertEqual(tmux.panes, [])

    def test_confirmed_recovery_reports_late_genuine_duplicate_as_open_twice(
        self,
    ) -> None:
        session = Session(
            "codex",
            "19191919-1919-4919-8919-191919191919",
            name="late duplicate",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        owner_pid = os.getpid()
        self.assertTrue(self.store.set_live_owner(*session.key, owner_pid))
        provider = FakeProvider()
        tmux = StaticTmux()
        pika = Pika(self.store, tmux, {"codex": provider})
        original_outside = pika._outside_processes
        outside_calls = 0

        def staged_outside(current, panes):
            nonlocal outside_calls
            outside_calls += 1
            result = original_outside(current, panes)
            # The third check is open()'s pre-reservation check. Introduce the
            # duplicates immediately after it so only the reserved recheck can
            # catch and classify the race.
            if outside_calls == 3:
                provider.active = [888, 999]
            return result

        with (
            patch(
                "pikamux.core.provider_process",
                side_effect=lambda pid, provider=None: (
                    777 if pid == owner_pid and provider == "codex" else None
                ),
            ),
            patch(
                "pikamux.core.shared_provider_process",
                side_effect=lambda pid, provider: provider == "codex" and pid == 777,
            ),
            patch.object(pika, "_outside_processes", side_effect=staged_outside),
            self.assertRaisesRegex(PikaError, "OPEN TWICE"),
        ):
            pika.recover_after_closed_confirmation(session, attach=False)

        self.assertEqual(tmux.panes, [])

    def test_confirmed_recovery_refuses_a_renewed_shared_lease(self) -> None:
        session = Session(
            "codex",
            "20202020-2020-4020-8020-202020202020",
            name="renewed lease",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        owner_pid = os.getpid()
        self.assertTrue(
            self.store.set_live_owner(*session.key, owner_pid, owner_token="initial")
        )
        tmux = StaticTmux()
        pika = Pika(self.store, tmux, {"codex": FakeProvider()})
        original_outside = pika._outside_processes
        outside_calls = 0

        def renew_before_recheck(current, panes):
            nonlocal outside_calls
            outside_calls += 1
            if outside_calls == 2:
                self.store.set_live_owner(
                    *session.key, owner_pid, owner_token="renewed"
                )
            return original_outside(current, panes)

        with (
            patch(
                "pikamux.core.provider_process",
                side_effect=lambda pid, provider=None: (
                    777 if pid == owner_pid and provider == "codex" else None
                ),
            ),
            patch("pikamux.core.shared_provider_process", return_value=True),
            patch.object(
                pika, "_outside_processes", side_effect=renew_before_recheck
            ),
            self.assertRaisesRegex(PikaError, "live ownership evidence"),
        ):
            pika.recover_after_closed_confirmation(session, attach=False)

        self.assertEqual(tmux.panes, [])
        self.assertEqual(len(self.store.get_live_owner_leases(*session.key)), 1)

    def test_stale_opencode_root_lease_expires_while_process_survives(self) -> None:
        session = Session(
            "opencode", "ses_stale123", name="old root", cwd="/tmp"
        )
        self.store.upsert_session(session)
        owner_pid = os.getpid()
        self.assertTrue(self.store.set_live_owner(*session.key, owner_pid))
        with self.store.connect() as db:
            db.execute(
                "UPDATE live_owners SET last_seen=? WHERE provider=? AND session_id=?",
                (time.time() - 301, *session.key),
            )
        pika = Pika(
            self.store,
            StaticTmux(),
            {"opencode": FakeProvider()},
        )
        with (
            patch("pikamux.core.provider_process", return_value=owner_pid),
            patch("pikamux.core.shared_provider_process", return_value=True),
        ):
            self.assertEqual(pika._live_owner_pids(session), set())
        self.assertEqual(self.store.get_live_owner_leases(*session.key), [])

    def test_fresh_opencode_root_claim_supersedes_stale_launch_argv(self) -> None:
        old = Session("opencode", "ses_oldroot123", name="old", cwd="/tmp")
        new = Session("opencode", "ses_newroot123", name="new", cwd="/tmp")
        self.store.upsert_session(old)
        self.store.upsert_session(new)
        with patch("pikamux.store.process_start_time", return_value=10):
            self.assertTrue(self.store.set_live_owner(*new.key, 4321))
        pika = Pika(
            self.store,
            StaticTmux(),
            {"opencode": FakeProvider(name="opencode")},
        )
        with (
            patch(
                "pikamux.core.opencode_session_processes",
                return_value={old.session_id: [4321]},
            ),
            patch("pikamux.core.process_start_time", return_value=10),
        ):
            self.assertEqual(pika._outside_processes(old, []), [])
            self.store.delete_live_owner(*new.key)
            self.assertEqual(pika._outside_processes(old, []), [4321])

    def test_opencode_failed_native_rename_keeps_requested_pika_alias(self) -> None:
        session_id = "ses_alias123"
        session = Session("opencode", session_id, name="qes_style", cwd="/tmp")
        self.store.upsert_session(session)
        self.store.set_meta(
            f"native_name_error:opencode:{session_id}", "qes_style"
        )
        candidate = Candidate(
            "opencode",
            session_id,
            name="New session - provider placeholder",
            cwd="/tmp",
            updated_at=time.time(),
        )
        pika = Pika(
            self.store,
            StaticTmux(),
            {"opencode": FakeProvider(name="opencode", candidates=[candidate])},
        )

        refreshed = pika.refresh()[0]

        self.assertEqual(refreshed.name, "qes_style")
        self.assertEqual(
            self.store.get_meta(f"native_name_error:opencode:{session_id}"),
            "qes_style",
        )

    def test_refresh_removes_unmanaged_opencode_placeholder_but_keeps_lease(
        self,
    ) -> None:
        session_id = "ses_placeholder123"
        self.store.upsert_session(
            Session(
                "opencode",
                session_id,
                name="New session - 2026-08-26T00:00:00Z",
                cwd="/tmp",
                status=Status.UNBOUND.value,
                source="external",
                managed=False,
            )
        )
        with patch("pikamux.store.process_start_time", return_value=12345):
            self.store.set_live_owner("opencode", session_id, 4321)
        provider = FakeProvider(
            name="opencode",
            candidates=[
                Candidate(
                    "opencode",
                    session_id,
                    name="New session - 2026-08-26T00:00:00Z",
                    cwd="/tmp",
                )
            ],
        )
        provider.native_placeholder_title = lambda value: str(value or "").startswith(
            "New session - "
        )
        pika = Pika(self.store, StaticTmux(), {"opencode": provider})

        with patch("pikamux.core.opencode_session_processes", return_value={}):
            refreshed = pika.refresh()

        self.assertEqual(refreshed, [])
        self.assertIsNone(self.store.get_session("opencode", session_id))
        self.assertEqual(
            self.store.get_live_owners("opencode", session_id),
            [(4321, 12345)],
        )

    def test_refresh_keeps_managed_opencode_placeholder(self) -> None:
        session_id = "ses_managedplaceholder123"
        self.store.upsert_session(
            Session(
                "opencode",
                session_id,
                name="New session - provider placeholder",
                cwd="/tmp",
                status=Status.STARTING.value,
                source="managed",
                managed=True,
            )
        )
        provider = FakeProvider(name="opencode")
        provider.native_placeholder_title = lambda value: str(value or "").startswith(
            "New session - "
        )
        pika = Pika(self.store, StaticTmux(), {"opencode": provider})

        with patch("pikamux.core.opencode_session_processes", return_value={}):
            refreshed = pika.refresh()

        self.assertEqual([item.session_id for item in refreshed], [session_id])

    def test_opencode_second_live_client_is_open_twice_not_generic_error(self) -> None:
        session_id = "ses_duplicate123"
        token = "opencode-launch"
        session = Session(
            "opencode",
            session_id,
            name="duplicate",
            cwd="/tmp",
            tmux_session="manual",
            tmux_pane="%1",
        )
        self.store.upsert_session(session)
        self.assertTrue(self.store.bind_launch(token, "opencode", session_id))
        self.store.set_recovery_owner("opencode", session_id, 999, 42, token)
        with patch(
            "pikamux.store.process_start_time",
            side_effect=lambda pid: {999: 42, 888: 84}.get(pid),
        ):
            self.assertTrue(self.store.set_live_owner(*session.key, 999))
            self.assertTrue(self.store.set_live_owner(*session.key, 888))
        tmux = StaticTmux([pane(provider="opencode", session_id=session_id)])
        pika = Pika(
            self.store,
            tmux,
            {"opencode": FakeProvider(name="opencode")},
        )

        def provider_at_root(pid, provider=None):
            return pid if provider == "opencode" and pid in {888, 999} else (
                999 if provider == "opencode" and pid == 123 else None
            )

        with (
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
            patch(
                "pikamux.core.process_start_time",
                side_effect=lambda pid: {999: 42, 888: 84}.get(pid),
            ),
            patch(
                "pikamux.core.process_environment",
                return_value={
                    "PIKA_PROVIDER": "opencode",
                    "PIKA_LAUNCH_TOKEN": token,
                },
            ),
            patch("pikamux.core.shared_provider_process", return_value=True),
        ):
            self.assertIsNone(pika.exact_pane_pid(session, tmux.panes[0]))
            refreshed = pika.refresh()[0]
        self.assertEqual(refreshed.status, Status.OPEN_TWICE.value)
        self.assertIn("multiple process trees", refreshed.error or "")

    def test_opencode_external_idle_argv_blocks_an_exact_managed_home(self) -> None:
        session_id = "ses_idleduplicate123"
        token = "opencode-launch"
        session = Session(
            "opencode",
            session_id,
            name="idle duplicate",
            cwd="/tmp",
            tmux_session="manual",
            tmux_pane="%1",
        )
        self.store.upsert_session(session)
        self.assertTrue(self.store.bind_launch(token, "opencode", session_id))
        self.store.set_recovery_owner("opencode", session_id, 999, 42, token)
        tmux = StaticTmux([pane(provider="opencode", session_id=session_id)])
        pika = Pika(
            self.store,
            tmux,
            {"opencode": FakeProvider(name="opencode")},
        )

        def provider_at_root(pid, provider=None):
            return 999 if provider == "opencode" and pid in {123, 999} else None

        with (
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
            patch("pikamux.core.process_start_time", return_value=42),
            patch(
                "pikamux.core.process_environment",
                return_value={
                    "PIKA_PROVIDER": "opencode",
                    "PIKA_LAUNCH_TOKEN": token,
                },
            ),
            patch(
                "pikamux.core.opencode_session_processes",
                return_value={session_id: [888]},
            ) as process_snapshot,
        ):
            refreshed = pika.refresh()[0]
            self.assertEqual(process_snapshot.call_count, 1)
            self.assertEqual(refreshed.status, Status.OPEN_TWICE.value)
            self.assertIn("outside PID 888", refreshed.error or "")
            process_snapshot.reset_mock()
            with self.assertRaisesRegex(PikaError, "OPEN TWICE"):
                pika.open(session, attach=False)
            self.assertEqual(process_snapshot.call_count, 2)

    def test_confirmed_recovery_rejects_exact_client_after_pid_reuse(self) -> None:
        session = Session(
            "codex",
            "21212121-2121-4121-8121-212121212121",
            name="reused pid",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        with patch("pikamux.store.process_start_time", return_value=100):
            self.assertTrue(self.store.set_live_owner(*session.key, 456))
        tmux = StaticTmux()
        pika = Pika(
            self.store,
            tmux,
            {"codex": FakeProvider(active=[456])},
        )

        with (
            patch("pikamux.core.process_start_time", return_value=200),
            patch("pikamux.core.provider_process", return_value=456),
            self.assertRaisesRegex(PikaError, "bearing the exact UUID"),
        ):
            pika.recover_after_closed_confirmation(session, attach=False)

        self.assertEqual(tmux.panes, [])

    def test_reused_live_owner_pid_cannot_prove_exact_identity(self) -> None:
        session = Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="exact-only",
            cwd="/tmp",
        )
        with patch("pikamux.store.process_start_time", return_value=100):
            self.store.set_live_owner("codex", session.session_id, 456)
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})
        with (
            patch("pikamux.core.process_start_time", return_value=200),
            patch("pikamux.core.provider_process", return_value=456),
        ):
            self.assertEqual(pika.identity_pids(session), set())
        self.assertEqual(self.store.get_live_owners(*session.key), [])

    def test_shared_app_server_lease_never_blocks_and_expires_normally(self) -> None:
        session_id = "12121212-1212-4212-8212-121212121212"
        session = Session(
            "codex",
            session_id,
            name="lease-recovery",
            cwd="/tmp",
            status=Status.READY.value,
            unread=True,
            last_event_at=10.0,
        )
        self.store.upsert_session(session)
        owner_root = os.getpid()
        self.assertTrue(self.store.set_live_owner("codex", session_id, owner_root))
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(
            self.store,
            tmux,
            {"codex": FakeProvider(active=[999])},
        )

        def provider_at_root(pid, provider=None):
            if provider != "codex":
                return None
            return 999 if pid == 123 else 777 if pid == owner_root else None

        def tree(pid):
            return [123, 999] if pid == 123 else [pid]

        with (
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
            patch("pikamux.core.process_tree", side_effect=tree),
            patch("pikamux.core.shared_provider_process", return_value=True),
        ):
            exact = pika.refresh()[0]
            self.assertEqual(exact.status, Status.READY.value)
            self.assertTrue(exact.exact_home)

            with self.store.connect() as db:
                db.execute(
                    "UPDATE live_owners SET last_seen=? "
                    "WHERE provider=? AND session_id=? AND pid=?",
                    (time.time() - 301, "codex", session_id, owner_root),
                )

            recovered = pika.refresh()[0]

        self.assertTrue(recovered.exact_home)
        self.assertEqual(recovered.status, Status.READY.value)
        self.assertTrue(recovered.unread)
        self.assertIsNone(recovered.error)
        self.assertEqual(self.store.get_live_owners("codex", session_id), [])

    def test_exact_uuid_process_outranks_fresh_shared_app_server_lease(self) -> None:
        session_id = "13131313-1313-4313-8313-131313131313"
        session = Session("codex", session_id, cwd="/tmp")
        self.store.upsert_session(session)
        owner_root = os.getpid()
        self.assertTrue(self.store.set_live_owner("codex", session_id, owner_root))
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(
            self.store,
            tmux,
            {"codex": FakeProvider(active=[999])},
        )

        def provider_at_root(pid, provider=None):
            if provider != "codex":
                return None
            return 999 if pid == 123 else 777 if pid == owner_root else None

        with (
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
            patch("pikamux.core.shared_provider_process", return_value=True),
        ):
            self.assertEqual(pika.exact_pane_pid(session, tmux.panes[0]), 999)
        self.assertEqual(
            self.store.get_live_owners("codex", session_id)[0][0], owner_root
        )

    def test_bound_launch_process_outranks_shared_app_server_lease(self) -> None:
        session_id = "15151515-1515-4515-8515-151515151515"
        token = "launch-token"
        session = Session(
            "codex",
            session_id,
            cwd="/tmp",
            status=Status.WORKING.value,
        )
        self.store.upsert_session(session)
        self.store.capture_identity_interruption(*session.key)
        self.store.update_session(
            *session.key,
            status=Status.OPEN_TWICE.value,
            unread=True,
            attention_reason="identity",
            error=f"{OPEN_TWICE_ERROR}: PIDs 777, 999",
        )
        self.assertTrue(self.store.bind_launch(token, "codex", session_id))
        self.store.set_recovery_owner("codex", session_id, 999, 4242, token)
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[])})

        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
            patch(
                "pikamux.core.process_environment",
                return_value={
                    "PIKA_PROVIDER": "codex",
                    "PIKA_LAUNCH_TOKEN": token,
                },
            ),
            patch("pikamux.core.process_start_time", return_value=4242),
            patch.object(pika, "_live_owner_pids", return_value={999, 777}),
            patch(
                "pikamux.core.shared_provider_process",
                side_effect=lambda pid, _provider: pid == 777,
            ),
        ):
            self.assertEqual(pika.exact_pane_pid(session, tmux.panes[0]), 999)
            self.assertEqual(pika.identity_pids(session), {999})
            refreshed = pika.refresh()[0]

        self.assertTrue(refreshed.exact_home)
        self.assertEqual(refreshed.status, Status.WORKING.value)
        self.assertFalse(refreshed.unread)
        self.assertIsNone(refreshed.error)

    def test_bound_launch_still_fails_closed_for_second_uuid_process(self) -> None:
        session_id = "16161616-1616-4616-8616-161616161616"
        token = "launch-token"
        session = Session("codex", session_id, cwd="/tmp")
        self.store.upsert_session(session)
        self.assertTrue(self.store.bind_launch(token, "codex", session_id))
        self.store.set_recovery_owner("codex", session_id, 999, 4242, token)
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[888])})

        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
            patch(
                "pikamux.core.process_environment",
                return_value={
                    "PIKA_PROVIDER": "codex",
                    "PIKA_LAUNCH_TOKEN": token,
                },
            ),
            patch("pikamux.core.process_start_time", return_value=4242),
        ):
            self.assertIsNone(pika.exact_pane_pid(session, tmux.panes[0]))
            refreshed = pika.refresh()[0]
            self.assertEqual(refreshed.status, Status.OPEN_TWICE.value)
            self.assertIn("multiple process trees", refreshed.error or "")

    def test_inherited_launch_token_does_not_prove_a_descendant(self) -> None:
        session_id = "17171717-1717-4717-8717-171717171717"
        token = "launch-token"
        session = Session("codex", session_id, cwd="/tmp")
        self.store.upsert_session(session)
        self.assertTrue(self.store.bind_launch(token, "codex", session_id))
        self.store.set_recovery_owner("codex", session_id, 555, 4141, token)
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider(active=[])})

        with (
            patch(
                "pikamux.core.process_environment",
                return_value={
                    "PIKA_PROVIDER": "codex",
                    "PIKA_LAUNCH_TOKEN": token,
                },
            ),
            patch("pikamux.core.process_start_time", return_value=4242),
        ):
            self.assertFalse(pika._bound_launch_identity(session, 999))

    def test_bound_launch_rejects_pid_reuse(self) -> None:
        session_id = "18181818-1818-4818-8818-181818181818"
        token = "launch-token"
        session = Session("codex", session_id, cwd="/tmp")
        self.store.upsert_session(session)
        self.assertTrue(self.store.bind_launch(token, "codex", session_id))
        self.store.set_recovery_owner("codex", session_id, 999, 4141, token)
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider(active=[])})

        with (
            patch(
                "pikamux.core.process_environment",
                return_value={
                    "PIKA_PROVIDER": "codex",
                    "PIKA_LAUNCH_TOKEN": token,
                },
            ),
            patch("pikamux.core.process_start_time", return_value=4242),
        ):
            self.assertFalse(pika._bound_launch_identity(session, 999))

    def test_fresh_dedicated_owner_lease_still_remains_fail_closed(self) -> None:
        session_id = "14141414-1414-4414-8414-141414141414"
        session = Session("codex", session_id, cwd="/tmp")
        self.store.upsert_session(session)
        owner_root = os.getpid()
        self.assertTrue(self.store.set_live_owner("codex", session_id, owner_root))
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(
            self.store,
            tmux,
            {"codex": FakeProvider(active=[999])},
        )

        def provider_at_root(pid, provider=None):
            if provider != "codex":
                return None
            return 999 if pid == 123 else 777 if pid == owner_root else None

        with (
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
            patch("pikamux.core.shared_provider_process", return_value=False),
        ):
            self.assertIsNone(pika.exact_pane_pid(session, tmux.panes[0]))

    def test_open_refuses_to_replace_other_agent_in_saved_pane(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        session = Session(
            "codex",
            session_id,
            name="wrong-agent",
            cwd="/tmp",
            tmux_session="manual",
            tmux_pane="%1",
        )
        self.store.upsert_session(session)
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=session_id)]),
            {"codex": FakeProvider()},
        )

        def process_for_provider(_pid, provider=None):
            return 777 if provider == "claude" else None

        with (
            patch("pikamux.core.provider_process", side_effect=process_for_provider),
            self.assertRaisesRegex(PikaError, "refuses to replace"),
        ):
            pika.open(session, attach=False)

    def test_exact_receipt_refuses_same_provider_with_unverified_uuid(self) -> None:
        session_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        session = Session(
            "codex",
            session_id,
            name="claimed-old-thread",
            cwd="/tmp",
            tmux_session="manual",
            tmux_pane="%1",
        )
        self.store.upsert_session(session)
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=session_id)]),
            {"codex": FakeProvider(active=[])},
        )
        with (
            patch("pikamux.core.provider_process", return_value=999),
            self.assertRaisesRegex(PikaError, "cannot be tied to exact UUID"),
        ):
            pika.open(session)
        refreshed = self.store.get_session("codex", session_id)
        self.assertEqual(refreshed.status if refreshed else None, Status.ERROR.value)
        self.assertEqual(refreshed.attention_reason if refreshed else None, "identity")

    def test_refresh_exposes_exact_home_only_with_independent_uuid_proof(self) -> None:
        session_id = "abababab-abab-4bab-8bab-abababababab"
        tracked = Session(
            "codex",
            session_id,
            cwd="/tmp",
            tmux_session="manual",
            tmux_pane="%1",
        )
        self.store.upsert_session(tracked)
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[999])})
        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
        ):
            exact = pika.refresh()[0]
        self.assertTrue(exact.exact_home)

        pika.providers["codex"].active = []
        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
        ):
            unverified = pika.refresh()[0]
        self.assertFalse(unverified.exact_home)
        self.assertEqual(unverified.status, Status.ERROR.value)

    def test_exact_home_fails_closed_for_uuid_process_outside_pane(self) -> None:
        session_id = "66666666-6666-4666-8666-666666666666"
        session = Session(
            "codex",
            session_id,
            name="one-home",
            cwd="/tmp",
            unread=True,
            status=Status.READY.value,
            last_event_at=10.0,
        )
        self.store.upsert_session(session)
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(
            self.store,
            tmux,
            {"codex": FakeProvider(active=[999, 888])},
        )

        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
        ):
            refreshed = pika.refresh()[0]
            self.assertFalse(refreshed.exact_home)
            self.assertEqual(refreshed.status, Status.OPEN_TWICE.value)
            with self.assertRaisesRegex(PikaError, "OPEN TWICE"):
                pika.open(session)
        self.assertTrue(self.store.get_session("codex", session_id).unread)

        pika.providers["codex"].active = [999]
        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
        ):
            healed = pika.refresh()[0]
        self.assertTrue(healed.exact_home)
        self.assertEqual(healed.status, Status.READY.value)
        self.assertTrue(healed.unread)
        self.assertIsNone(healed.error)

    def test_duplicate_home_repair_cannot_manufacture_a_result(self) -> None:
        session_id = "77777777-7777-4777-8777-777777777777"
        self.store.upsert_session(
            Session(
                "codex",
                session_id,
                name="duplicate",
                cwd="/tmp",
                status=Status.WORKING.value,
            )
        )
        first = pane(pane_id="%1", provider="codex", session_id=session_id)
        second = pane(pane_id="%2", provider="codex", session_id=session_id)
        tmux = StaticTmux([first, second])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[999])})
        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
        ):
            broken = pika.refresh()[0]
            self.assertEqual(broken.status, Status.ERROR.value)
            tmux.panes = [first]
            healed = pika.refresh()[0]
        self.assertEqual(healed.status, Status.WORKING.value)
        self.assertFalse(healed.unread)
        counts = self.store.attention_event_counts(since=0.0)
        self.assertEqual(counts.get(Status.ERROR.value), 1)
        self.assertNotIn(Status.READY.value, counts)

    def test_provider_error_does_not_erase_exact_home_proof(self) -> None:
        session_id = "12121212-1212-4212-8212-121212121212"
        self.store.upsert_session(
            Session(
                "codex",
                session_id,
                status=Status.ERROR.value,
                unread=True,
                error="provider failed",
                attention_reason="failed",
            )
        )
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=session_id)]),
            {"codex": FakeProvider(active=[999])},
        )
        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
        ):
            failed = pika.refresh()[0]
        self.assertEqual(failed.status, Status.ERROR.value)
        self.assertTrue(failed.exact_home)

    def test_dead_unbound_placeholder_no_longer_demands_adoption(self) -> None:
        placeholder = Session(
            "codex",
            "unbound:%1",
            name="orphan-shell",
            cwd="/tmp",
            tmux_session="manual",
            tmux_pane="%1",
            status=Status.UNBOUND.value,
            unread=True,
            attention_reason="adopt",
        )
        self.store.upsert_session(placeholder)
        pika = Pika(self.store, StaticTmux([pane()]), {"codex": FakeProvider()})
        with patch("pikamux.core.provider_process", return_value=None):
            healed = pika.refresh()[0]
        self.assertEqual(healed.status, Status.PARKED.value)
        self.assertFalse(healed.unread)
        self.assertFalse(healed.live)

    def test_busy_pane_receipt_makes_preserved_work_and_new_home_visible(self) -> None:
        session_id = "55555555-5555-4555-8555-555555555555"
        old = pane(provider="codex", session_id=session_id)
        old.current_command = "nvim"
        session = Session(
            "codex",
            session_id,
            name="careful-resume",
            cwd="/tmp",
            tmux_session=old.session_name,
            tmux_pane=old.pane_id,
        )
        self.store.upsert_session(session)
        tmux = StaticTmux([old])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[])})

        def provider_in_new_home(pid, provider=None):
            return 888 if pid == 456 and provider == "codex" else None

        with (
            patch("pikamux.core.provider_process", side_effect=provider_in_new_home),
            patch("pikamux.core.process_tree", return_value=[123, 777]),
            patch.object(
                pika,
                "identity_pids",
                side_effect=lambda _session: {888} if len(tmux.panes) > 1 else set(),
            ),
            redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(pika.open(session), 0)
        self.assertEqual(tmux.cleared, ["%1"])
        self.assertEqual(len(tmux.receipts), 1)
        self.assertIn("NEW HOME", tmux.receipts[0])
        self.assertIn("preserved manual:%1 running nvim", tmux.receipts[0])

    def test_conflicting_reused_pane_id_is_cleared(self) -> None:
        old_id = "11111111-1111-4111-8111-111111111111"
        other_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session(
                "codex",
                old_id,
                name="old",
                cwd="/tmp",
                tmux_session="old-home",
                tmux_pane="%1",
                root_pid=123,
            )
        )
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=other_id)]),
            {"codex": FakeProvider()},
        )
        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
        ):
            refreshed = pika.refresh()
        old = next(item for item in refreshed if item.session_id == old_id)
        persisted = self.store.get_session("codex", old_id)
        self.assertFalse(old.live)
        self.assertIsNone(persisted.tmux_pane if persisted else "missing")
        self.assertIsNone(persisted.root_pid if persisted else "missing")

    def test_same_uuid_from_other_provider_is_still_a_pane_conflict(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        self.store.upsert_session(
            Session(
                "codex",
                session_id,
                name="codex-home",
                cwd="/tmp",
                tmux_session="old-home",
                tmux_pane="%1",
                root_pid=123,
            )
        )
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="claude", session_id=session_id)]),
            {"codex": FakeProvider()},
        )
        with patch("pikamux.core.provider_process", return_value=None):
            pika.refresh()
        persisted = self.store.get_session("codex", session_id)
        self.assertIsNone(persisted.tmux_pane if persisted else "missing")

    def test_live_import_adopts_exact_tmux_process_or_marks_unbound(self) -> None:
        candidate = Candidate(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="running",
            cwd="/tmp",
            live=True,
            pid=999,
        )
        tmux = StaticTmux([pane()])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[999])})
        with patch(
            "pikamux.core.provider_process",
            side_effect=lambda _pid, provider=None: 999
            if provider == "codex"
            else None,
        ):
            imported = pika.import_candidate(candidate)
        self.assertEqual(imported.status, Status.WORKING.value)
        self.assertTrue(imported.managed)
        self.assertEqual(imported.tmux_pane, "%1")
        self.assertEqual(tmux.tags[0][1]["session_id"], candidate.session_id)

        second = Candidate(
            "codex",
            "22222222-2222-4222-8222-222222222222",
            name="outside",
            cwd="/tmp",
            live=True,
            pid=888,
        )
        outside = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})
        imported = outside.import_candidate(second)
        self.assertEqual(imported.status, Status.UNBOUND.value)
        self.assertFalse(imported.managed)

    def test_live_import_rejects_mixed_provider_pane(self) -> None:
        candidate = Candidate(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="mixed",
            cwd="/tmp",
            live=True,
            pid=999,
        )
        tmux = StaticTmux([pane()])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[999])})

        def mixed_provider(_pid, provider=None):
            return 999 if provider == "codex" else 888

        with (
            patch("pikamux.core.provider_process", side_effect=mixed_provider),
            patch("pikamux.core.process_tree", return_value=[123, 999, 888]),
        ):
            imported = pika.import_candidate(candidate)
        self.assertEqual(imported.status, Status.UNBOUND.value)
        self.assertEqual(tmux.tags, [])
        self.assertFalse(imported.managed)
        self.assertIsNone(imported.tmux_pane)

    def test_adopt_name_finds_the_untagged_tmux_pane(self) -> None:
        session_id = "33333333-3333-4333-8333-333333333333"
        session = Session("claude", session_id, name="named-agent", cwd="/tmp")
        candidate = Candidate(
            "claude",
            session_id,
            name="named-agent",
            cwd="/tmp",
            live=True,
            pid=999,
        )
        tmux = StaticTmux([pane()])
        pika = Pika(
            self.store,
            tmux,
            {"claude": FakeProvider("claude", [candidate], active=[999])},
        )

        def provider_at_root(_pid, provider=None):
            return 999 if provider == "claude" else None

        with (
            patch.object(pika, "resolve", return_value=session),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
            patch("pikamux.core.provider_process", side_effect=provider_at_root),
        ):
            adopted = pika.adopt("named-agent")

        self.assertEqual(adopted.session_id, session_id)
        self.assertEqual(tmux.tags[0][0], "%1")

    def test_adopt_name_explains_safe_transition_from_outside_tmux(self) -> None:
        session_id = "44444444-4444-4444-8444-444444444444"
        session = Session("claude", session_id, name="outside-agent", cwd="/tmp")
        pika = Pika(
            self.store,
            StaticTmux(),
            {"claude": FakeProvider("claude", active=[999])},
        )
        with (
            patch.object(pika, "resolve", return_value=session),
            self.assertRaisesRegex(
                PikaError,
                "running outside tmux.*cannot be moved safely.*pika outside-agent",
            ),
        ):
            pika.adopt("outside-agent")

    def test_stale_pending_identity_is_not_pruned_by_refresh(self) -> None:
        self.store.add_pending("old-token", "codex", "unbound", "/tmp")
        with self.store.connect() as db:
            db.execute(
                "UPDATE pending_launches SET created_at=? WHERE launch_token=?",
                (time.time() - 86400, "old-token"),
            )
        Pika(self.store, StaticTmux(), {"codex": FakeProvider()}).refresh()
        self.assertIsNotNone(self.store.get_pending("old-token"))

    def test_doctor_repair_removes_only_confirmed_stale_launch_state(self) -> None:
        self.store.add_pending("missing-pane", "codex", "lost", "/tmp")
        self.store.add_pending("active-pane", "codex", "active", "/tmp", "home", "%1")
        self.store.add_pending(
            "session-only", "codex", "binding", "/tmp", "manual", None
        )
        self.assertTrue(
            self.store.reserve_resume(
                "codex", "11111111-1111-4111-8111-111111111111", "stale-lock"
            )
        )
        self.store.initialize()
        with self.store.connect() as db:
            db.execute(
                """
                INSERT INTO launch_reservations(
                    provider,session_id,token,owner_pid,owner_start_time,created_at
                ) VALUES (?,?,?,?,?,?)
                """,
                (
                    "claude",
                    "22222222-2222-4222-8222-222222222222",
                    "dead-lock",
                    99999999,
                    1,
                    time.time() - 600,
                ),
            )
            db.execute("UPDATE pending_launches SET created_at=?", (time.time() - 600,))
            db.execute(
                "UPDATE launch_reservations SET created_at=?", (time.time() - 600,)
            )
        pika = Pika(
            self.store,
            StaticTmux([pane(pane_id="%1")]),
            {"codex": FakeProvider()},
        )
        with patch(
            "pikamux.doctor.provider_process",
            side_effect=lambda pid, provider=None: 777 if pid == 123 else None,
        ):
            repairs = repair_stale_state(pika)
        self.assertEqual(len(repairs), 2)
        self.assertIsNone(self.store.get_pending("missing-pane"))
        self.assertIsNotNone(self.store.get_pending("active-pane"))
        self.assertIsNotNone(self.store.get_pending("session-only"))
        with self.store.connect() as db:
            self.assertEqual(
                db.execute("SELECT COUNT(*) FROM launch_reservations").fetchone()[0],
                1,
            )

    def test_new_codex_can_start_before_first_hook_observation(self) -> None:
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})
        provider = pika.providers["codex"]
        provider.new_argv = lambda name, session_id=None: ["codex"]
        with patch("pikamux.core.hooks_installed", return_value=True):
            self.assertEqual(pika.new("guarded", "codex", "/tmp", attach=False), 0)
        pending = self.store.list_pending()
        self.assertEqual(len(pending), 1)
        self.assertEqual(pending[0]["name"], "guarded")

    def test_doctor_rejects_untracked_live_owner(self) -> None:
        config = self.root / "config.json"
        config.write_text('{"default_provider":"codex"}\n')
        os.chmod(config, 0o600)
        self.store.set_meta("hook_seen:codex", hook_spec_fingerprint("codex"))
        owner_pid = os.getpid()
        self.store.set_live_owner(
            "codex", "11111111-1111-4111-8111-111111111111", owner_pid
        )
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})
        output = io.StringIO()
        with (
            patch("pikamux.doctor.config_path", return_value=config),
            patch("pikamux.doctor.database_path", return_value=self.store.path),
            patch("pikamux.doctor.hooks_installed", return_value=True),
            patch("pikamux.doctor.codex_hooks_enabled", return_value=True),
            patch("pikamux.doctor.provider_process", return_value=owner_pid),
            patch("pikamux.core.provider_process", return_value=owner_pid),
            redirect_stdout(output),
        ):
            self.assertFalse(
                run_doctor(pika, as_json=True, repairs=["removed stale lock"])
            )
        receipt = json.loads(output.getvalue())
        self.assertEqual(receipt["repairs"], ["removed stale lock"])
        self.assertIn("outside Pika tracking", output.getvalue())

    def test_doctor_expires_stale_untracked_shared_app_server_lease(self) -> None:
        config = self.root / "config.json"
        config.write_text('{"default_provider":"codex"}\n')
        os.chmod(config, 0o600)
        session_id = "14141414-1414-4414-8414-141414141414"
        owner_pid = os.getpid()
        self.assertTrue(self.store.set_live_owner("codex", session_id, owner_pid))
        with self.store.connect() as db:
            db.execute(
                "UPDATE live_owners SET last_seen=? "
                "WHERE provider=? AND session_id=? AND pid=?",
                (time.time() - 301, "codex", session_id, owner_pid),
            )
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})
        with (
            patch("pikamux.doctor.config_path", return_value=config),
            patch("pikamux.doctor.database_path", return_value=self.store.path),
            patch("pikamux.doctor.hooks_installed", return_value=True),
            patch("pikamux.doctor.codex_hooks_enabled", return_value=True),
            patch("pikamux.doctor.provider_process", return_value=owner_pid),
            patch("pikamux.doctor.shared_provider_process", return_value=True),
            redirect_stdout(io.StringIO()),
        ):
            run_doctor(pika, as_json=True)
        self.assertEqual(self.store.get_live_owners("codex", session_id), [])

    def test_doctor_rejects_tracked_live_owner_outside_pika_tmux(self) -> None:
        session_id = "77777777-7777-4777-8777-777777777777"
        config = self.root / "config.json"
        config.write_text('{"default_provider":"codex"}\n')
        os.chmod(config, 0o600)
        self.store.upsert_session(
            Session("codex", session_id, name="external", cwd="/tmp")
        )
        self.store.set_meta("hook_seen:codex", hook_spec_fingerprint("codex"))
        owner_pid = os.getpid()
        self.store.set_live_owner("codex", session_id, owner_pid)
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})
        output = io.StringIO()
        with (
            patch("pikamux.doctor.config_path", return_value=config),
            patch("pikamux.doctor.database_path", return_value=self.store.path),
            patch("pikamux.doctor.hooks_installed", return_value=True),
            patch("pikamux.doctor.codex_hooks_enabled", return_value=True),
            patch("pikamux.doctor.provider_process", return_value=owner_pid),
            patch("pikamux.core.provider_process", return_value=owner_pid),
            redirect_stdout(output),
        ):
            self.assertFalse(run_doctor(pika, as_json=True))
        receipt = json.loads(output.getvalue())
        self.assertFalse(receipt["safe_to_disconnect"])
        self.assertIn("outside exact Pika tmux homes", output.getvalue())

    def test_doctor_rejects_invalid_and_non_resumable_identities(self) -> None:
        config = self.root / "config.json"
        config.write_text('{"default_provider":"codex"}\n')
        os.chmod(config, 0o600)
        for session_id, resumable in (
            ("not-a-provider-uuid", True),
            ("11111111-1111-4111-8111-111111111111", False),
        ):
            with self.subTest(session_id=session_id):
                db = self.root / (session_id[:6] + ".db")
                store = Store(db)
                store.upsert_session(
                    Session("codex", session_id, name="risk", cwd="/tmp")
                )
                store.set_meta("hook_seen:codex", "1")
                pika = Pika(
                    store,
                    StaticTmux(),
                    {"codex": FakeProvider(resumable=resumable)},
                )
                with (
                    patch("pikamux.doctor.config_path", return_value=config),
                    patch("pikamux.doctor.database_path", return_value=db),
                    patch("pikamux.doctor.hooks_installed", return_value=True),
                    redirect_stdout(io.StringIO()),
                ):
                    self.assertFalse(run_doctor(pika, as_json=True))

    def test_doctor_surfaces_failed_native_naming(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        config = self.root / "config.json"
        config.write_text('{"default_provider":"codex"}\n')
        os.chmod(config, 0o600)
        self.store.upsert_session(
            Session("codex", session_id, name="pending-name", cwd="/tmp")
        )
        self.store.set_meta("hook_seen:codex", "1")
        self.store.set_meta(f"native_name_error:codex:{session_id}", "pending-name")
        pika = Pika(
            self.store,
            StaticTmux(),
            {"codex": FakeProvider(resumable=True)},
        )
        output = io.StringIO()
        with (
            patch("pikamux.doctor.config_path", return_value=config),
            patch("pikamux.doctor.database_path", return_value=self.store.path),
            patch("pikamux.doctor.hooks_installed", return_value=True),
            redirect_stdout(output),
        ):
            self.assertFalse(run_doctor(pika, as_json=True))
        self.assertIn("provider naming is still pending", output.getvalue())

    def test_safe_doctor_is_a_scoped_timestamped_recovery_certificate(self) -> None:
        config = self.root / "config.json"
        config.write_text('{"default_provider":"codex"}\n')
        os.chmod(config, 0o600)
        self.store.initialize()
        self.store.record_hook_observation(
            "codex",
            hook_spec_fingerprint("codex"),
            "session_start",
            "11111111-1111-4111-8111-111111111111",
        )
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})
        output = io.StringIO()
        with (
            patch("pikamux.doctor.config_path", return_value=config),
            patch("pikamux.doctor.database_path", return_value=self.store.path),
            patch("pikamux.doctor.hooks_installed", return_value=True),
            patch("pikamux.doctor.codex_hooks_enabled", return_value=True),
            patch.dict(os.environ, {"TMUX": "socket,1,0"}, clear=False),
            redirect_stdout(output),
        ):
            self.assertTrue(run_doctor(pika))
        receipt = output.getvalue()
        self.assertIn("Pika setup verified", receipt)
        self.assertIn("UTC", receipt)
        self.assertIn("Safe to disconnect this terminal", receipt)
        self.assertIn("Keep the tmux server running", receipt)
        self.assertIn("Detach with Ctrl-b d", receipt)
        self.assertIn("Copy-safe recovery passport", receipt)
        self.assertIn("PIKA VERIFIED · 1 provider · 0 recoverable · 0 ambiguous", receipt)
        self.assertIn(f"pikamux {__version__}", receipt)
        self.assertNotIn("11111111-1111-4111-8111-111111111111", receipt)

    def test_safe_doctor_does_not_require_optional_missing_providers(self) -> None:
        config = self.root / "config.json"
        config.write_text('{"default_provider":"codex"}\n')
        os.chmod(config, 0o600)
        self.store.initialize()
        self.store.record_hook_observation(
            "codex",
            hook_spec_fingerprint("codex"),
            "session_start",
            "11111111-1111-4111-8111-111111111111",
        )
        claude = FakeProvider(name="claude")
        opencode = FakeProvider(name="opencode")
        claude.version = lambda: None
        opencode.version = lambda: None
        pika = Pika(
            self.store,
            StaticTmux(),
            {"codex": FakeProvider(), "claude": claude, "opencode": opencode},
        )
        output = io.StringIO()
        with (
            patch("pikamux.doctor.config_path", return_value=config),
            patch("pikamux.doctor.database_path", return_value=self.store.path),
            patch("pikamux.doctor.hooks_installed", return_value=True),
            patch("pikamux.doctor.codex_hooks_enabled", return_value=True),
            redirect_stdout(output),
        ):
            self.assertTrue(run_doctor(pika, as_json=True))
        receipt = json.loads(output.getvalue())
        optional = {
            item["name"]: item for item in receipt["checks"]
            if item["name"] in {"claude", "opencode"}
        }
        self.assertEqual(optional["claude"]["level"], "ok")
        self.assertEqual(optional["opencode"]["level"], "ok")
        self.assertTrue(receipt["safe_to_disconnect"])

    def test_failed_attach_does_not_acknowledge_ready_session(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        session = Session(
            "codex",
            session_id,
            name="unread",
            cwd="/tmp",
            status=Status.READY.value,
            unread=True,
        )
        self.store.upsert_session(session)
        tmux = StaticTmux(
            [pane(provider="codex", session_id=session_id)], attach_result=1
        )
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[999])})
        with patch("pikamux.core.provider_process", return_value=999):
            self.assertEqual(pika.open(session), 1)
        self.assertTrue(self.store.get_session(*session.key).unread)
        self.assertIsNone(self.store.get_meta("last_attached"))

    def test_successful_attach_acknowledges_and_records_immediately(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        session = Session(
            "codex",
            session_id,
            name="ready",
            cwd="/tmp",
            status=Status.READY.value,
            unread=True,
        )
        self.store.upsert_session(session)
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[999])})
        output = io.StringIO()
        with (
            patch("pikamux.core.provider_process", return_value=999),
            redirect_stdout(output),
        ):
            self.assertEqual(pika.open(session), 0)
        self.assertFalse(self.store.get_session(*session.key).unread)
        self.assertEqual(
            self.store.get_meta("last_attached"),
            '["codex", "11111111-1111-4111-8111-111111111111"]',
        )
        self.assertEqual(len(tmux.receipts), 1)
        self.assertIn("RESULT COLLECTED", tmux.receipts[0])
        self.assertIn("EXACT 11111111", tmux.receipts[0])
        self.assertIn("INBOX CLEAR", tmux.receipts[0])
        self.assertIn("return with: pika ready", output.getvalue())
        self.assertNotIn("pika open", output.getvalue())
        receipt_core = tmux.receipts[0].rsplit(" · ready", 1)[0]
        self.assertLessEqual(len(receipt_core), 58)
        tmux.receipts.clear()
        with patch("pikamux.core.provider_process", return_value=999):
            self.assertEqual(pika.open(self.store.get_session(*session.key)), 0)
        self.assertNotIn("RESULT COLLECTED", tmux.receipts[0])
        self.assertIn("CONTINUITY PROVEN", tmux.receipts[0])
        self.assertIn("same live home", tmux.receipts[0])

    def test_result_receipt_counts_remaining_unread_results(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        session = Session(
            "codex",
            session_id,
            name="first-result",
            cwd="/tmp",
            status=Status.READY.value,
            unread=True,
            last_event_at=100.0,
        )
        self.store.upsert_session(session)
        self.store.upsert_session(
            Session(
                "claude",
                "22222222-2222-4222-8222-222222222222",
                name="second-result",
                cwd="/tmp",
                status=Status.READY.value,
                unread=True,
                last_event_at=101.0,
            )
        )
        current = self.store.get_session(*session.key)
        assert current is not None
        tmux = StaticTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[999])})
        with patch("pikamux.core.provider_process", return_value=999):
            self.assertEqual(pika.open(current), 0)
        self.assertIn("1 unread remains", tmux.receipts[0])

    def test_result_receipt_fails_closed_when_a_new_event_wins_the_race(self) -> None:
        session_id = "77777777-7777-4777-8777-777777777777"
        session = Session(
            "codex",
            session_id,
            name="racing-result",
            cwd="/tmp",
            status=Status.READY.value,
            unread=True,
            last_event_at=100.0,
        )
        self.store.upsert_session(session)
        current = self.store.get_session(*session.key)
        assert current is not None

        class RacingTmux(StaticTmux):
            def attach(inner_self, *args, on_attached=None, receipt=None, **kwargs):
                self.store.update_session(
                    *session.key,
                    status=Status.NEEDS_YOU.value,
                    unread=True,
                    attention_reason="question",
                    last_event_at=current.last_event_at + 1,
                )
                return super().attach(
                    *args, on_attached=on_attached, receipt=receipt, **kwargs
                )

        tmux = RacingTmux([pane(provider="codex", session_id=session_id)])
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[999])})
        with patch("pikamux.core.provider_process", return_value=999):
            self.assertEqual(pika.open(current), 0)
        self.assertNotIn("RESULT COLLECTED", tmux.receipts[0])
        self.assertTrue(self.store.get_session(*session.key).unread)

    def test_result_stays_unread_while_resumed_identity_is_pending(self) -> None:
        session_id = "88888888-8888-4888-8888-888888888888"
        session = Session(
            "codex",
            session_id,
            name="pending-proof",
            cwd="/tmp",
            status=Status.READY.value,
            unread=True,
            last_event_at=100.0,
        )
        self.store.upsert_session(session)
        current = self.store.get_session(*session.key)
        assert current is not None
        tmux = StaticTmux()
        pika = Pika(self.store, tmux, {"codex": FakeProvider(active=[])})

        def provider_in_new_home(pid, provider=None):
            return 888 if pid == 456 and provider == "codex" else None

        with patch("pikamux.core.provider_process", side_effect=provider_in_new_home):
            self.assertEqual(pika.open(current), 0)
        self.assertIn("IDENTITY PENDING", tmux.receipts[0])
        self.assertNotIn("RESULT COLLECTED", tmux.receipts[0])
        self.assertTrue(self.store.get_session(*session.key).unread)

    def test_resume_clears_old_failure_to_idle_without_unread_result(self) -> None:
        session_id = "99999999-9999-4999-8999-999999999999"
        session = Session(
            "codex",
            session_id,
            name="failed-before-resume",
            cwd="/tmp",
            status=Status.ERROR.value,
            unread=True,
            error="boom",
            attention_reason="failed",
            last_event_at=100.0,
        )
        self.store.upsert_session(session)
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})

        def provider_in_new_home(pid, provider=None):
            return 888 if pid == 456 and provider == "codex" else None

        with patch("pikamux.core.provider_process", side_effect=provider_in_new_home):
            self.assertEqual(pika.open(session, attach=False), 0)
        current = self.store.get_session(*session.key)
        self.assertEqual(current.status if current else None, Status.READY.value)
        self.assertFalse(current.unread if current else True)
        counts = self.store.attention_event_counts(since=0.0, until=time.time())
        self.assertEqual(counts.get(Status.ERROR.value), 1)
        self.assertNotIn(Status.READY.value, counts)

    def test_resume_uses_provider_evidence_for_genuine_work(self) -> None:
        now = time.time()
        session_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
        session = Session(
            "codex",
            session_id,
            name="active-turn",
            cwd="/tmp",
            status=Status.PARKED.value,
            last_event_at=now,
        )
        self.store.upsert_session(session)
        provider = FakeProvider(
            candidates=[
                Candidate(
                    "codex",
                    session_id,
                    name="active-turn",
                    cwd="/tmp",
                    updated_at=now - 10,
                    lifecycle_status=Status.WORKING.value,
                )
            ]
        )
        pika = Pika(self.store, StaticTmux(), {"codex": provider})

        def provider_in_new_home(pid, provider=None):
            return 888 if pid == 456 and provider == "codex" else None

        with patch("pikamux.core.provider_process", side_effect=provider_in_new_home):
            self.assertEqual(pika.open(session, attach=False), 0)
        current = self.store.get_session(*session.key)
        self.assertEqual(current.status if current else None, Status.WORKING.value)
        self.assertFalse(current.unread if current else True)

    def test_refresh_heals_legacy_process_launch_marked_as_work(self) -> None:
        now = time.time()
        session_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
        self.store.upsert_session(
            Session(
                "codex",
                session_id,
                name="idle-resume",
                cwd="/tmp",
                tmux_session="manual",
                tmux_pane="%1",
                status=Status.WORKING.value,
                unread=False,
                last_event_at=now - 120,
                last_activity_at=now - 30,
            )
        )
        provider = FakeProvider(
            candidates=[
                Candidate(
                    "codex",
                    session_id,
                    name="idle-resume",
                    cwd="/tmp",
                    updated_at=now - 300,
                    lifecycle_status=Status.READY.value,
                )
            ],
            active=[999],
        )
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=session_id)]),
            {"codex": provider},
        )

        with patch("pikamux.core.provider_process", return_value=999):
            refreshed = pika.refresh()

        self.assertEqual(refreshed[0].status, Status.READY.value)
        self.assertFalse(refreshed[0].unread)
        counts = self.store.attention_event_counts(since=0.0, until=time.time())
        self.assertNotIn(Status.READY.value, counts)

    def test_refresh_does_not_override_fresh_work_hook_with_old_ready(self) -> None:
        now = time.time()
        session_id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc"
        self.store.upsert_session(
            Session(
                "codex",
                session_id,
                name="fresh-turn",
                cwd="/tmp",
                tmux_session="manual",
                tmux_pane="%1",
                status=Status.WORKING.value,
                unread=False,
                last_event_at=now - 1,
                last_activity_at=now,
            )
        )
        provider = FakeProvider(
            candidates=[
                Candidate(
                    "codex",
                    session_id,
                    name="fresh-turn",
                    cwd="/tmp",
                    updated_at=now - 300,
                    lifecycle_status=Status.READY.value,
                )
            ],
            active=[999],
        )
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=session_id)]),
            {"codex": provider},
        )

        with patch("pikamux.core.provider_process", return_value=999):
            refreshed = pika.refresh()

        self.assertEqual(refreshed[0].status, Status.WORKING.value)

    def test_stale_ready_ack_cannot_hide_new_permission(self) -> None:
        session_id = "66666666-6666-4666-8666-666666666666"
        stale = Session(
            "codex",
            session_id,
            name="racing",
            status=Status.READY.value,
            unread=True,
            last_event_at=100,
        )
        self.store.upsert_session(stale)
        persisted = self.store.get_session("codex", session_id)
        assert persisted is not None
        self.store.update_session(
            "codex",
            session_id,
            status=Status.NEEDS_YOU.value,
            unread=True,
            attention_reason="permission",
            last_event_at=persisted.last_event_at + 1,
        )
        Pika(self.store, StaticTmux(), {"codex": FakeProvider()}).acknowledge(
            persisted, attaching=True
        )
        current = self.store.get_session("codex", session_id)
        self.assertTrue(current.unread if current else False)
        self.assertEqual(current.status if current else None, Status.NEEDS_YOU.value)
        self.assertEqual(current.attention_reason if current else None, "permission")

    def test_refresh_picks_up_provider_native_rename(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        self.store.upsert_session(
            Session("codex", session_id, name="before", cwd="/tmp")
        )
        provider = FakeProvider(
            candidates=[
                Candidate("codex", session_id, name="after", cwd="/tmp", updated_at=2)
            ]
        )
        pika = Pika(self.store, StaticTmux(), {"codex": provider})
        refreshed = pika.refresh()
        self.assertEqual(refreshed[0].name, "after")

    def test_refresh_follows_one_live_codex_continuation_in_same_home(self) -> None:
        now = time.time()
        parent_id = "11111111-1111-4111-8111-111111111111"
        child_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                name="research-notes",
                cwd="/repo",
                tmux_session="manual",
                tmux_pane="%1",
                status=Status.READY.value,
                last_event_at=now - 120,
            )
        )
        provider = FakeProvider(
            candidates=[
                Candidate(
                    "codex",
                    parent_id,
                    "research-notes",
                    cwd="/repo",
                    updated_at=now - 100,
                    lifecycle_status=Status.READY.value,
                ),
                Candidate(
                    "codex",
                    child_id,
                    "research-notes",
                    cwd="/repo",
                    transcript_path="/repo/child.jsonl",
                    updated_at=now,
                    parent_session_id=parent_id,
                    lifecycle_status=Status.WORKING.value,
                ),
            ],
            active=[999],
        )
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=parent_id)]),
            {"codex": provider},
        )
        with patch("pikamux.core.provider_process", return_value=999):
            refreshed = pika.refresh()

        self.assertEqual(len(refreshed), 1)
        self.assertEqual(refreshed[0].session_id, parent_id)
        self.assertEqual(refreshed[0].active_thread_id, child_id)
        self.assertEqual(refreshed[0].status, Status.WORKING.value)
        self.assertEqual(refreshed[0].transcript_path, "/repo/child.jsonl")
        self.assertTrue(refreshed[0].exact_home)
        resolved = self.store.get_session_by_thread("codex", child_id)
        self.assertEqual(resolved.session_id if resolved else None, parent_id)

    def test_refresh_tracks_renamed_fork_separately_from_parent(self) -> None:
        parent_id = "11111111-1111-4111-8111-111111111111"
        child_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                name="returns_tracker",
                cwd="/repo",
                status=Status.READY.value,
            )
        )
        provider = FakeProvider(
            candidates=[
                Candidate(
                    "codex",
                    parent_id,
                    "returns_tracker",
                    cwd="/repo",
                    lifecycle_status=Status.READY.value,
                ),
                Candidate(
                    "codex",
                    child_id,
                    "cf_perf",
                    cwd="/repo",
                    parent_session_id=parent_id,
                    lifecycle_status=Status.READY.value,
                ),
            ]
        )

        refreshed = Pika(
            self.store,
            StaticTmux(),
            {"codex": provider},
        ).refresh()

        self.assertEqual(
            {(item.session_id, item.name) for item in refreshed},
            {
                (parent_id, "returns_tracker"),
                (child_id, "cf_perf"),
            },
        )
        child = self.store.get_session("codex", child_id)
        self.assertEqual(child.status if child else None, Status.PARKED.value)
        self.assertIsNone(
            self.store.get_session("codex", parent_id).active_thread_id
        )

    def test_refresh_does_not_split_same_name_same_pane_continuation(self) -> None:
        now = time.time()
        parent_id = "11111111-1111-4111-8111-111111111111"
        child_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                name="returns_tracker",
                cwd="/repo",
                tmux_session="manual",
                tmux_pane="%1",
                status=Status.READY.value,
            )
        )
        provider = FakeProvider(
            candidates=[
                Candidate(
                    "codex",
                    child_id,
                    "returns_tracker",
                    cwd="/repo",
                    parent_session_id=parent_id,
                    updated_at=now,
                    lifecycle_status=Status.WORKING.value,
                )
            ],
            active=[],
        )
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=parent_id)]),
            {"codex": provider},
        )
        with patch("pikamux.core.provider_process", return_value=999):
            refreshed = pika.refresh()

        self.assertEqual(len(refreshed), 1)
        self.assertEqual(refreshed[0].session_id, parent_id)
        self.assertEqual(refreshed[0].active_thread_id, child_id)
        self.assertIsNone(self.store.get_session("codex", child_id))

    def test_same_pane_active_fork_adopts_its_new_native_name(self) -> None:
        now = time.time()
        parent_id = "11111111-1111-4111-8111-111111111111"
        child_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                active_thread_id=child_id,
                name="returns_tracker",
                cwd="/repo",
                tmux_session="manual",
                tmux_pane="%1",
                status=Status.WORKING.value,
            )
        )
        provider = FakeProvider(
            candidates=[
                Candidate(
                    "codex",
                    child_id,
                    "cf_perf",
                    cwd="/repo",
                    parent_session_id=parent_id,
                    updated_at=now,
                    lifecycle_status=Status.WORKING.value,
                )
            ],
            active=[],
        )
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=parent_id)]),
            {"codex": provider},
        )
        with (
            patch("pikamux.core.provider_process", return_value=999),
            patch("pikamux.core.process_tree", return_value=[123, 999]),
        ):
            refreshed = pika.refresh()

        self.assertEqual(len(refreshed), 1)
        self.assertEqual(refreshed[0].session_id, parent_id)
        self.assertEqual(refreshed[0].active_thread_id, child_id)
        self.assertEqual(refreshed[0].name, "cf_perf")
        self.assertIsNone(self.store.get_session("codex", child_id))

    def test_two_live_codex_continuations_fail_closed_as_open_twice(self) -> None:
        now = time.time()
        parent_id = "11111111-1111-4111-8111-111111111111"
        first_child_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                active_thread_id=first_child_id,
                name="research-notes",
                cwd="/repo",
                tmux_session="manual",
                tmux_pane="%1",
                status=Status.READY.value,
                last_event_at=now - 120,
            )
        )
        children = [
            Candidate(
                provider="codex",
                session_id=(
                    first_child_id
                    if number == 2
                    else "33333333-3333-4333-8333-333333333333"
                ),
                name="research-notes",
                cwd="/repo",
                updated_at=now - number,
                parent_session_id=parent_id,
                lifecycle_status=Status.WORKING.value,
            )
            for number in (2, 3)
        ]
        # Both are sibling forks of the stable home. The first is already the
        # active leaf; discovering the second must still fail closed.
        self.assertEqual(children[0].session_id, first_child_id)
        pika = Pika(
            self.store,
            StaticTmux([pane(provider="codex", session_id=parent_id)]),
            {"codex": FakeProvider(candidates=children, active=[999])},
        )
        with patch("pikamux.core.provider_process", return_value=999):
            refreshed = pika.refresh()

        self.assertEqual(refreshed[0].status, Status.OPEN_TWICE.value)
        self.assertTrue(refreshed[0].unread)
        self.assertIn("multiple active Codex continuation", refreshed[0].error)
        self.assertEqual(refreshed[0].active_thread_id, first_child_id)


if __name__ == "__main__":
    unittest.main()
