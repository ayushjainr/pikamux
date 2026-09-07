"""First-use recovery precedes broad discovery and proves only observed facts."""
from __future__ import annotations

import argparse
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from pikamux import cli
from pikamux.models import Candidate, Session


@pytest.fixture
def setup_context(monkeypatch, tmp_path):
    events = []
    pika = Mock()
    pika.store.list_sessions.return_value = []
    pika.store.untracked_session_keys.return_value = set()
    pika.store.list_pending.return_value = []
    pika.store.get_meta.return_value = None
    pika.store.get_hook_observation.return_value = None
    pika.refresh.side_effect = lambda **_: events.append("refresh") or []
    pika.discover_import_candidates.side_effect = lambda: events.append("inventory") or []
    pika.discovery_errors = []
    pika.providers = {"codex": object()}
    monkeypatch.setattr(cli, "config_path", lambda: tmp_path / "config.json")
    monkeypatch.setattr(cli, "load_config", lambda: {"default_provider": "codex", "machine_alias": "local"})
    monkeypatch.setattr(cli, "setup_executables", lambda *_, **__: {"codex": "codex"})
    monkeypatch.setattr(cli, "setup_runtime_path", lambda *_, **__: "/usr/bin")
    monkeypatch.setattr(cli, "executable_version", lambda _: "1.0")
    monkeypatch.setattr(cli, "executable_available", lambda _: True)
    monkeypatch.setattr(cli, "provider_compatibility_error", lambda *_: None)
    monkeypatch.setattr(cli, "provider_version_supported", lambda *_: True)
    monkeypatch.setattr(cli, "hooks_installed", lambda _: True)
    monkeypatch.setattr(cli, "hook_spec_fingerprint", lambda _: "expected-hook")
    change = SimpleNamespace(changed=True, path=tmp_path / "config.json", diff=lambda: "CONFIG PREVIEW")
    monkeypatch.setattr(cli, "proposed_changes", lambda *_, **__: [change])
    monkeypatch.setattr(cli, "apply_changes", lambda _: events.append("apply") or [])
    monkeypatch.setattr(cli, "_first_conversation_recovery", lambda *_, **__: events.append("first-conversation"))
    monkeypatch.setattr(cli, "_setup_machine_candidates", lambda *_: events.append("machines") or [])
    monkeypatch.setattr(cli, "choose_fleet_candidates", lambda _: events.append("choose-more") or [])
    monkeypatch.setattr(cli, "_setup_coverage", lambda *_, **__: [])
    monkeypatch.setattr(cli.sys.stdin, "isatty", lambda: True)
    monkeypatch.setattr("builtins.input", lambda _: "y")
    args = cli._parser().parse_args(["setup", "--default-provider", "codex"])
    return pika, args, events


def test_first_recovery_precedes_broad_discovery_even_without_observed_hooks(setup_context, capsys):
    pika, args, events = setup_context
    assert cli._setup(pika, args) == 0
    assert events.index("apply") < events.index("first-conversation")
    assert events.index("first-conversation") < events.index("machines")
    assert events.index("first-conversation") < events.index("inventory")
    output = capsys.readouterr().out
    assert "CONFIG PREVIEW" in output
    assert "One required proof remains" in output
    assert "Pika commissioned ·" not in output
    pika.consultation.assert_not_called()


@pytest.mark.parametrize("skip", ["yes", "skip_walkthrough", "dry_run"])
def test_automatic_and_experienced_paths_skip_walkthrough(setup_context, skip):
    pika, args, events = setup_context
    setattr(args, skip, True)
    assert cli._setup(pika, args) == 0
    assert "first-conversation" not in events
    if skip == "dry_run":
        assert events == []


def test_explicit_bulk_import_survives_walkthrough_skip(setup_context):
    pika, args, events = setup_context
    args.yes = True
    args.import_all = True
    args.machine = ["explicit-host"]
    candidate = Candidate("codex", "thread", name="existing")
    pika.discover_import_candidates.side_effect = lambda: [candidate]
    assert cli._setup(pika, args) == 0
    assert "machines" in events
    pika.import_candidate.assert_called_once_with(candidate)


def test_partial_commissioning_can_prove_same_active_identity(monkeypatch, capsys):
    session = Session("codex", "logical", name="research", cwd="/tmp", status="PARKED", active_thread_id="active-id")
    pika = Mock()
    pika.providers = {"codex": object()}
    pika.open.return_value = 0
    pika.refresh.return_value = [replace(session, live=True)]
    pika.tmux.list_panes.return_value = [SimpleNamespace(pika_provider="codex", pika_session_id="logical")]
    pika.exact_pane_pid.return_value = 123
    monkeypatch.setattr(cli.sys.stdin, "isatty", lambda: True)
    monkeypatch.setenv("TMUX", "")
    monkeypatch.setattr("builtins.input", lambda _: "y")
    cli._offer_recovery_rehearsal(pika, [session], commissioned=False, automatic=False)
    output = capsys.readouterr().out
    assert "CONTINUITY PROVEN" in output
    assert "recovery only" in output
    assert "Pika commissioned" not in output
    pika.consultation.assert_not_called()


def test_recovery_cannot_certify_a_fork_or_reused_identity(monkeypatch, capsys):
    session = Session("codex", "logical", name="research", cwd="/tmp", status="PARKED", active_thread_id="original")
    pika = Mock()
    pika.providers = {"codex": object()}
    pika.open.return_value = 0
    pika.refresh.return_value = [replace(session, active_thread_id="fork")]
    pika.tmux.list_panes.return_value = [SimpleNamespace(pika_provider="codex", pika_session_id="logical")]
    pika.exact_pane_pid.return_value = 123
    monkeypatch.setattr(cli.sys.stdin, "isatty", lambda: True)
    monkeypatch.setenv("TMUX", "")
    monkeypatch.setattr("builtins.input", lambda _: "y")
    cli._offer_recovery_rehearsal(pika, [session], commissioned=True, automatic=False)
    output = capsys.readouterr().out
    assert "REHEARSAL INCOMPLETE" in output
    assert "CONTINUITY PROVEN" not in output


def test_first_conversation_does_not_guess_between_duplicate_names(monkeypatch, capsys):
    pika = Mock()
    pika.enter.return_value = 0
    pika.store.list_sessions.return_value = [Session("codex", "one", name="research"), Session("claude", "two", name="research")]
    monkeypatch.setattr(cli.sys.stdin, "isatty", lambda: True)
    monkeypatch.setenv("TMUX", "")
    monkeypatch.setattr("builtins.input", lambda _: "research")
    cli._first_conversation_recovery(pika, commissioned=False)
    assert "No identity was guessed" in capsys.readouterr().out
    pika.open.assert_not_called()
