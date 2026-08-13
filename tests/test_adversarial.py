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

from pikamux.core import Pika, PikaError
from pikamux.doctor import repair_stale_state, run_doctor
from pikamux.models import Candidate, Pane, Session, Status
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
    ):
        self.name = name
        self.candidates = candidates or []
        self.active = active or []
        self.resumable = resumable
        self.hidden = hidden or set()

    def discover(self) -> list[Candidate]:
        return self.candidates

    def import_candidates(self) -> list[Candidate]:
        return self.candidates

    def hidden_session_ids(self) -> set[str]:
        return self.hidden

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
        self.temp = tempfile.TemporaryDirectory(dir="/mnt/ebs1/ajain")
        self.root = Path(self.temp.name)
        self.store = Store(self.root / "pika.db")

    def tearDown(self) -> None:
        self.temp.cleanup()

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

    def test_provider_hidden_session_stays_out_of_refresh_and_pane_recovery(
        self,
    ) -> None:
        archived_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session("codex", archived_id, name="master_quant", cwd="/tmp")
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
            name="owned-elsewhere",
            cwd="/tmp",
        )
        self.store.upsert_session(session)
        pika = Pika(
            self.store,
            StaticTmux([pane()]),
            {"codex": FakeProvider(active=[999])},
        )
        with (
            patch("pikamux.core.provider_process", return_value=999),
            self.assertRaisesRegex(PikaError, "already running outside"),
        ):
            pika.open(session, attach=False)

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

    def test_stale_shared_app_server_lease_auto_recovers_exact_pane(self) -> None:
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
            blocked = pika.refresh()[0]
            self.assertEqual(blocked.status, Status.ERROR.value)
            self.assertFalse(blocked.exact_home)

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

    def test_fresh_shared_app_server_lease_remains_fail_closed(self) -> None:
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
            self.assertIsNone(pika.exact_pane_pid(session, tmux.panes[0]))
        self.assertEqual(
            self.store.get_live_owners("codex", session_id)[0][0], owner_root
        )

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
        with patch("pikamux.core.provider_process", return_value=999):
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
        with patch("pikamux.core.provider_process", return_value=999):
            imported = pika.import_candidate(candidate)
        self.assertEqual(imported.status, Status.READY.value)
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

    def test_new_codex_requires_observed_current_hook_definition(self) -> None:
        pika = Pika(self.store, StaticTmux(), {"codex": FakeProvider()})
        with (
            patch("pikamux.core.hooks_installed", return_value=True),
            self.assertRaisesRegex(PikaError, "current Codex hook definition"),
        ):
            pika.new("guarded", "codex", "/tmp", attach=False)

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
        self.store.set_meta("hook_seen:codex", hook_spec_fingerprint("codex"))
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
        with (
            patch("pikamux.core.provider_process", return_value=999),
            redirect_stdout(io.StringIO()),
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
        receipt_core = tmux.receipts[0].rsplit(" · ready", 1)[0]
        self.assertLessEqual(len(receipt_core), 58)
        tmux.receipts.clear()
        with patch("pikamux.core.provider_process", return_value=999):
            self.assertEqual(pika.open(self.store.get_session(*session.key)), 0)
        self.assertNotIn("RESULT COLLECTED", tmux.receipts[0])
        self.assertIn("ATTACHED EXACT", tmux.receipts[0])

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

    def test_resume_does_not_manufacture_a_ready_result(self) -> None:
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
        self.assertEqual(current.status if current else None, Status.WORKING.value)
        self.assertFalse(current.unread if current else True)
        counts = self.store.attention_event_counts(since=0.0, until=time.time())
        self.assertEqual(counts.get(Status.ERROR.value), 1)
        self.assertNotIn(Status.READY.value, counts)

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


if __name__ == "__main__":
    unittest.main()
