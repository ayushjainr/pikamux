"""Daily name selection keeps durable identity and uncertainty separate."""
from dataclasses import replace
import os
import sqlite3
from unittest.mock import Mock

import pytest

from pikamux.core import Pika, PikaError
from pikamux.models import Candidate, FleetSession, Session
from pikamux.providers import CodexProvider
from pikamux.store import Store


@pytest.fixture
def inventory(tmp_path):
    home = tmp_path / "codex"
    home.mkdir()
    (home / "sessions").mkdir()
    cwd = tmp_path / "project"
    cwd.mkdir()
    sessions = []
    with sqlite3.connect(home / "state_5.sqlite") as db:
        db.execute("CREATE TABLE threads(id TEXT, name TEXT, cwd TEXT, rollout_path TEXT, updated_at REAL, archived INTEGER)")
        for number, updated in [(1, 100), (2, 200)]:
            identity = f"00000000-0000-4000-8000-{number:012d}"
            path = home / "sessions" / f"rollout-{identity}.jsonl"
            path.write_text("")
            db.execute("INSERT INTO threads VALUES (?,?,?,?,?,0)",
                       (identity, "master_hf", str(cwd), str(path), updated))
            sessions.append(Session("codex", identity, "master_hf", cwd=str(cwd),
                                    transcript_path=str(path), updated_at=10000 - updated))
    provider = CodexProvider(home)
    pika = Pika(Store(tmp_path / "pika.sqlite"), Mock(), {"codex": provider})
    return pika, provider, sessions


def test_provider_activity_wins_not_pika_or_index_refresh(inventory):
    pika, provider, sessions = inventory
    (provider.home / "session_index.jsonl").write_text(
        '{"id":"' + sessions[0].session_id + '","thread_name":"master_hf","updated_at":999999}\n')
    assert pika.resolve("master_hf", sessions) == sessions[1]


def test_exact_live_home_wins_over_newer_inactive(inventory):
    pika, _, sessions = inventory
    sessions[0].live = True
    sessions[0].home_state = "exact-live"
    assert pika._name_choices("master_hf", sessions) == [sessions[0]]


@pytest.mark.parametrize("outside", [False, True])
def test_genuine_concurrent_identities_remain_ambiguous(inventory, outside):
    pika, _, sessions = inventory
    sessions[0].live = sessions[1].live = True
    sessions[0].home_state = "exact-live"
    sessions[1].home_state = "outside-live" if outside else "exact-live"
    assert pika._name_choices("master_hf", sessions) == sessions


def test_live_outside_is_not_silently_replaced_by_latest_parked(inventory):
    pika, _, sessions = inventory
    sessions[0].live = True
    assert pika._name_choices("master_hf", sessions) == sessions


def test_other_machine_is_never_merged_by_local_directory(inventory):
    pika, _, sessions = inventory
    remote = FleetSession("rs6", "rs6", sessions[1])
    assert pika._name_choices("master_hf", [sessions[0], remote]) == [sessions[0], remote]


@pytest.mark.parametrize("change", ["provider", "unknown-cwd", "different-cwd", "tie"])
def test_unsafe_equivalence_stays_ambiguous(inventory, tmp_path, change):
    pika, provider, sessions = inventory
    if change == "provider":
        sessions[0].provider = "claude"
    else:
        with sqlite3.connect(provider.home / "state_5.sqlite") as db:
            if change == "tie":
                db.execute("UPDATE threads SET updated_at=200")
            else:
                cwd = None
                if change == "different-cwd":
                    directory = tmp_path / "another"
                    directory.mkdir()
                    cwd = str(directory)
                sessions[0].cwd = cwd
                db.execute("UPDATE threads SET cwd=? WHERE id=?", (cwd, sessions[0].session_id))
    assert pika._name_choices("master_hf", sessions) == sessions


def test_explicit_uuid_wins_even_when_name_matches_other_thread(inventory):
    pika, _, sessions = inventory
    sessions[1].name = sessions[0].session_id
    assert pika._name_choices(sessions[0].session_id, sessions) == [sessions[0]]


def test_active_thread_alias_is_one_choice(inventory):
    pika, _, sessions = inventory
    home = replace(sessions[0], active_thread_id=sessions[1].session_id)
    assert pika._name_choices("master_hf", [home, sessions[1]]) == [home]
    assert pika._name_choices(sessions[1].session_id, [home, sessions[1]]) == [home]


def test_exact_home_alias_is_not_discarded_for_parked_logical_alias(inventory):
    pika, _, sessions = inventory
    parked = replace(sessions[0], active_thread_id=sessions[1].session_id)
    live = replace(sessions[1], live=True, home_state="exact-live")
    assert pika._name_choices("master_hf", [live, parked]) == [live]


def test_confirmed_index_only_missing_is_not_a_competing_choice(inventory):
    pika, _, sessions = inventory
    stale = Session("codex", "019d2cff-0000-4000-8000-000000000000", "master_hf")
    pika.store.upsert_session(stale)
    assert pika._name_choices("master_hf", [stale, sessions[1]]) == [sessions[1]]
    assert pika.store.get_session(*stale.key) is not None
    assert pika._name_choices(stale.session_id, [stale]) == [stale]
    with pytest.raises(PikaError, match="No new conversation was created"):
        pika._name_choices("master_hf", [stale])


def test_legacy_rollout_without_database_row_is_not_confirmed_missing(inventory):
    _, provider, sessions = inventory
    identity = sessions[0].session_id
    with sqlite3.connect(provider.home / "state_5.sqlite") as db:
        db.execute("DELETE FROM threads WHERE id=?", (identity,))
    assert provider.selection_evidence(identity)[0] == "unknown"


def test_archived_is_excluded_from_name_but_not_exact_id(inventory):
    pika, provider, sessions = inventory
    with sqlite3.connect(provider.home / "state_5.sqlite") as db:
        db.execute("UPDATE threads SET archived=1 WHERE id=?", (sessions[1].session_id,))
    assert pika._name_choices("master_hf", sessions) == [sessions[0]]
    assert pika._name_choices(sessions[1].session_id, sessions) == [sessions[1]]


def test_unreadable_newest_database_never_falls_back_to_missing(inventory):
    pika, provider, sessions = inventory
    stale = Session("codex", "unavailable-id", "master_hf", cwd=sessions[0].cwd)
    corrupt = provider.home / "state_6.sqlite"
    corrupt.write_bytes(b"not sqlite")
    os.utime(corrupt, (2_000_000_000, 2_000_000_000))
    assert provider.selection_evidence(stale.session_id) == ("unknown", None)
    assert pika._name_choices("master_hf", [stale, sessions[1]]) == [stale, sessions[1]]


def test_unavailable_transcript_storage_does_not_prove_missing(inventory, monkeypatch):
    _, provider, _ = inventory
    def denied(*args, **kwargs):
        raise PermissionError("storage unavailable")
    monkeypatch.setattr("pikamux.providers.os.walk", denied)
    assert provider.selection_evidence("stale-id") == ("unknown", None)


def test_enter_reduces_native_alias_and_stale_rows_without_adoption(inventory, monkeypatch):
    pika, _, sessions = inventory
    home = replace(sessions[0], active_thread_id=sessions[1].session_id,
                   live=True, home_state="exact-live")
    stale = Session("codex", "gone-id", "master_hf")
    pika._last_discovered_candidates = [Candidate("codex", sessions[1].session_id, "master_hf")]
    monkeypatch.setattr(pika, "refresh", lambda: [home, stale])
    monkeypatch.setattr(pika, "pending_launches", lambda: [])
    monkeypatch.setattr(pika.fleet, "cached_sessions", lambda: [])
    opened = Mock(return_value=0)
    monkeypatch.setattr(pika, "open", opened)
    assert pika.enter("master_hf") == 0
    opened.assert_called_once_with(home, attach=True)
    assert pika.store.list_sessions() == []
