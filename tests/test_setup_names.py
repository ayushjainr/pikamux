"""Setup naming evidence is stricter than lookup, on local and remote nodes."""
import io
import json
import sqlite3
from unittest.mock import Mock

import pytest

from pikamux.core import Pika
from pikamux.fleet import FleetError, FleetManager, PROTOCOL_NAME, PROTOCOL_VERSION, handle_fleet_stdio
from pikamux.models import Candidate, FleetNode, Session
from pikamux.providers import ClaudeProvider, CodexProvider, OpenCodeProvider
from pikamux.store import Store
from pikamux.ui import choose_fleet_candidates


@pytest.fixture
def inventory(tmp_path):
    home = tmp_path / "codex"
    home.mkdir()
    names = {
        "11111111-1111-4111-8111-111111111111": ("chosen", False, "Codex Desktop"),
        "22222222-2222-4222-8222-222222222222": (None, False, "Codex Desktop"),
        "33333333-3333-4333-8333-333333333333": ("archived", True, "Codex Desktop"),
        "44444444-4444-4444-8444-444444444444": ("worker", False, "codex_exec"),
    }
    with sqlite3.connect(home / "state_current.sqlite") as db:
        db.execute("CREATE TABLE threads(id TEXT, name TEXT, rollout_path TEXT, archived INTEGER)")
        for identity, (name, archived, originator) in names.items():
            path = home / (identity + ".jsonl")
            path.write_text(json.dumps({"type": "session_meta", "payload": {
                "id": identity, "originator": originator,
            }}) + "\n")
            db.execute("INSERT INTO threads VALUES (?,?,?,?)", (identity, name, str(path), archived))
    (home / "session_index.jsonl").write_text("".join(
        json.dumps({"id": identity, "thread_name": "old display title" if name == "chosen" else name or "generated title"}) + "\n"
        for identity, (name, _, _) in names.items()
    ))
    store = Store(tmp_path / "pika.db")
    provider = CodexProvider(home)
    return Pika(store, Mock(), {"codex": provider}), provider


def test_setup_does_not_guess_name_authorship_and_browse_preserves_lookup(inventory):
    pika, provider = inventory
    before = {p: p.read_bytes() for p in provider.home.iterdir() if p.is_file()}
    assert pika.discover_import_candidates() == []
    assert {item.name for item in pika.discover_import_candidates(include_unconfirmed=True)} == {
        "old display title", "generated title",
    }
    assert provider.find_candidates("generated title")[0].session_id.startswith("22222222")
    assert provider.is_resumable("22222222-2222-4222-8222-222222222222")
    assert {p: p.read_bytes() for p in before} == before
    assert pika.store.list_sessions() == []


def test_remote_default_and_browse_use_same_filter_and_never_reimport_tracked(inventory):
    pika, _ = inventory
    tracked = Session("codex", "22222222-2222-4222-8222-222222222222", name="already watched")
    pika.store.upsert_session(tracked)

    def candidates(**extra):
        request = {"op": "candidates", "protocol": PROTOCOL_NAME, "version": PROTOCOL_VERSION, **extra}
        output = io.StringIO()
        assert handle_fleet_stdio(pika, io.StringIO(json.dumps(request) + "\n"), output) == 0
        return json.loads(output.getvalue())

    assert candidates()["candidates"] == []
    assert [item["session_id"] for item in candidates(include_unconfirmed=True)["candidates"]] == [
        "11111111-1111-4111-8111-111111111111",
    ]
    assert candidates(include_unconfirmed="false")["kind"] == "invalid_request"
    assert pika.store.get_session(*tracked.key).name == "already watched"


def test_claude_unknown_live_name_is_not_rename_evidence(tmp_path, monkeypatch):
    registry = tmp_path / "sessions"
    registry.mkdir()
    for identity, source in [("unknown", None), ("derived", "derived"), ("explicit", "custom")]:
        (registry / (identity + ".json")).write_text(json.dumps({
            "sessionId": identity, "kind": "interactive", "name": identity,
            "nameSource": source, "pid": 123,
        }))
    monkeypatch.setattr("pikamux.providers.provider_process", lambda *_: 123)
    provider = ClaudeProvider(tmp_path)
    assert [item.name for item in provider.import_candidates()] == ["explicit"]
    assert {item.session_id for item in provider.browse_candidates()} == {"unknown", "derived", "explicit"}
    assert {item.session_id for item in provider.launch_candidates()} == {"unknown", "derived", "explicit"}


def test_opencode_does_not_infer_rename_from_a_human_looking_title(tmp_path, monkeypatch):
    provider = OpenCodeProvider(tmp_path)
    row = Candidate("opencode", "ses_example123", "my-chosen-looking-name")
    monkeypatch.setattr(provider, "_records", lambda **_: [row])
    assert provider.import_candidates() == []
    assert provider.browse_candidates() == [row]
    assert provider.find_candidates(row.name) == [row]


def test_untitled_opencode_is_browsable_but_not_a_default_suggestion(tmp_path, monkeypatch):
    provider = OpenCodeProvider(tmp_path)
    row = Candidate("opencode", "ses_example123", None)
    monkeypatch.setattr(provider, "_records", lambda **kwargs: [row] if kwargs.get("named_only") is False else [])
    assert provider.discover() == []
    assert provider.import_candidates() == []
    assert provider.browse_candidates() == [row]


def test_browse_is_lazy_and_does_not_adopt_until_a_number_is_chosen(monkeypatch, capsys):
    row = (None, Candidate("codex", "exact", "generated"))
    browse = Mock(return_value=[row])
    monkeypatch.setattr("pikamux.ui.sys.stdin.isatty", lambda: True)
    answers = iter(["b", "1"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert choose_fleet_candidates([], browse=browse) == [row]
    browse.assert_called_once_with()
    assert "nothing added yet" in capsys.readouterr().out

    browse.reset_mock()
    monkeypatch.setattr("builtins.input", lambda _: "q")
    assert choose_fleet_candidates([], browse=browse) == []
    browse.assert_not_called()


def test_named_then_recent_keeps_choices_and_bounds_recent_inventory(monkeypatch, capsys):
    now = 2_000_000_000.0
    monkeypatch.setattr("pikamux.ui.time.time", lambda: now)
    monkeypatch.setattr("pikamux.ui.sys.stdin.isatty", lambda: True)
    named = (None, Candidate("claude", "named", "my-project", updated_at=now - 60))
    recent = [
        (None, Candidate("codex", f"recent-{i}", f"automatic-{i}", updated_at=now - i * 60))
        for i in range(12)
    ]
    stale = (None, Candidate("codex", "stale", "too-old", updated_at=now - 15 * 86400))
    unknown = (None, Candidate("codex", "unknown", "unknown-age"))
    browse = Mock(return_value=[stale, named, unknown, *reversed(recent)])
    answers = iter(["1", "all"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert choose_fleet_candidates([named], browse=browse) == [named, *recent[:10]]
    output = capsys.readouterr().out
    first, second = output.split("2/2 · RECENT")
    assert "my-project" in first and "automatic-0" not in first
    assert "my-project" not in second
    assert "too-old" not in second and "unknown-age" not in second
    assert "automatic-10" not in second
    assert second.index("automatic-0") < second.index("automatic-1")
    browse.assert_called_once_with()


def test_recent_browse_preserves_first_choice_and_machine_identity(monkeypatch):
    monkeypatch.setattr("pikamux.ui.sys.stdin.isatty", lambda: True)
    named = (None, Candidate("claude", "same-id", "chosen"))
    remote = (FleetNode("other-node", "remote", "remote"), Candidate("claude", "same-id", "remote title"))
    browse = Mock(return_value=[named, remote])
    answers = iter(["1", "b", "all"])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert choose_fleet_candidates([named], browse=browse) == [named, remote]
    browse.assert_called_once_with()


def test_skipping_recent_keeps_named_selection(monkeypatch):
    monkeypatch.setattr("pikamux.ui.sys.stdin.isatty", lambda: True)
    named = (None, Candidate("claude", "named", "chosen"))
    answers = iter(["1", ""])
    monkeypatch.setattr("builtins.input", lambda _: next(answers))
    assert choose_fleet_candidates([named], browse=lambda: []) == [named]


def test_noninteractive_chooser_never_scans_recent(monkeypatch):
    monkeypatch.setattr("pikamux.ui.sys.stdin.isatty", lambda: False)
    browse = Mock()
    assert choose_fleet_candidates([], browse=browse) == []
    browse.assert_not_called()


def test_untitled_codex_is_browsable_locally_and_remotely(inventory):
    pika, provider = inventory
    identity = "55555555-5555-4555-8555-555555555555"
    path = provider.home / (identity + ".jsonl")
    path.write_text(json.dumps({"type": "session_meta", "payload": {
        "id": identity, "originator": "Codex Desktop",
    }}) + "\n")
    with sqlite3.connect(provider.home / "state_current.sqlite") as db:
        db.execute("INSERT INTO threads VALUES (?,?,?,?)", (identity, None, str(path), False))
    assert identity not in {item.session_id for item in provider.discover()}
    assert identity in {item.session_id for item in pika.discover_import_candidates(include_unconfirmed=True)}
    request = {"op": "candidates", "protocol": PROTOCOL_NAME, "version": PROTOCOL_VERSION, "include_unconfirmed": True}
    output = io.StringIO()
    handle_fleet_stdio(pika, io.StringIO(json.dumps(request) + "\n"), output)
    assert identity in {item["session_id"] for item in json.loads(output.getvalue())["candidates"]}


def test_untitled_claude_is_browsable_without_becoming_a_named_suggestion(tmp_path):
    project = tmp_path / "projects" / "project"
    project.mkdir(parents=True)
    (project / "untitled.jsonl").write_text(json.dumps({"type": "user", "message": {"content": "hello"}}) + "\n")
    provider = ClaudeProvider(tmp_path)
    assert provider.import_candidates() == []
    rows = provider.browse_candidates()
    assert [(item.session_id, item.name) for item in rows] == [("untitled", None)]


def test_old_remote_inventory_is_never_presented_as_confirmed_names(tmp_path):
    transport = Mock()
    manager = FleetManager(Store(tmp_path / "pika.db"), transport)
    node = FleetNode("remote-id", "older-node", "older-node")
    with pytest.raises(FleetError, match="explicit-name setup filtering"):
        manager.remote_candidates(node)
    transport.request.assert_not_called()
    transport.request.return_value = {"type": "candidates", "node_id": node.node_id, "candidates": []}
    assert manager.remote_candidates(node, include_unconfirmed=True) == []
    assert transport.request.call_args.args[1]["include_unconfirmed"] is True


def test_remote_explicit_selection_can_add_a_browsed_unconfirmed_title(inventory):
    pika, _ = inventory
    request = {
        "op": "adopt", "protocol": PROTOCOL_NAME, "version": PROTOCOL_VERSION,
        "provider": "codex", "session_id": "22222222-2222-4222-8222-222222222222",
        "request_id": "55555555-5555-4555-8555-555555555555",
    }
    output = io.StringIO()
    assert handle_fleet_stdio(pika, io.StringIO(json.dumps(request) + "\n"), output) == 0
    response = json.loads(output.getvalue())
    assert response["type"] == "adopted", response
    assert response["session"]["session_id"] == request["session_id"]
    assert pika.store.get_session("codex", request["session_id"]).name == "generated title"
