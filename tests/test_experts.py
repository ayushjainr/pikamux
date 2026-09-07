from __future__ import annotations

import json
import os
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from pikamux.core import Pika, PikaError
from pikamux.consult import ConsultationError
from pikamux.experts import (
    card_state,
    interview_profile,
    make_profile,
    rank_experts,
    transcript_fingerprint,
)
from pikamux.models import ExpertProfile, Pane, Session, Status
from pikamux.store import Store


class OnePaneTmux:
    def __init__(self, pane: Pane):
        self.pane = pane

    def get_pane(self, target: str):
        return (
            self.pane if target in {self.pane.pane_id, self.pane.session_name} else None
        )


class ExpertTests(unittest.TestCase):
    def test_opencode_card_fingerprint_is_session_tree_scoped(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            database = Path(directory) / "opencode.db"
            with sqlite3.connect(database) as db:
                db.execute(
                    "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, "
                    "time_updated INTEGER, time_archived INTEGER)"
                )
                db.execute(
                    "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, "
                    "time_created INTEGER, data TEXT)"
                )
                db.execute(
                    "CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, "
                    "message_id TEXT, time_created INTEGER, data TEXT)"
                )
                db.executemany(
                    "INSERT INTO session VALUES (?,?,?,NULL)",
                    (("ses_one", None, 100), ("ses_other", None, 100)),
                )
            session = Session("opencode", "ses_one", transcript_path=str(database))
            before = transcript_fingerprint(session)
            with sqlite3.connect(database) as db:
                db.execute("UPDATE session SET time_updated=200 WHERE id='ses_other'")
                db.execute(
                    "INSERT INTO message VALUES ('m_other','ses_other',200,'{}')"
                )
            self.assertEqual(transcript_fingerprint(session), before)
            with sqlite3.connect(database) as db:
                db.execute(
                    "INSERT INTO session VALUES ('ses_child','ses_one',300,NULL)"
                )
                db.execute(
                    "INSERT INTO message VALUES ('m_child','ses_child',300,'{}')"
                )
                db.execute(
                    "INSERT INTO part VALUES ('p_child','ses_child','m_child',300,'{}')"
                )
            self.assertNotEqual(transcript_fingerprint(session), before)

    def test_card_freshness_tracks_the_provider_transcript(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            transcript = Path(directory) / "thread.jsonl"
            transcript.write_text("one\n")
            session = Session("codex", "one", transcript_path=str(transcript))
            missing = card_state(session, None)
            self.assertEqual(missing.status, "MISSING")
            stat = transcript.stat()
            current = make_profile(
                session,
                summary="Did the work.",
                current_state="No active task; the verified work is complete.",
                topics=["work"],
                transcript_mtime_ns=stat.st_mtime_ns,
                transcript_size=stat.st_size,
            )
            self.assertEqual(card_state(session, current).status, "CURRENT")
            legacy = make_profile(
                session,
                summary="Recent work only.",
                topics=["work"],
                transcript_mtime_ns=stat.st_mtime_ns,
                transcript_size=stat.st_size,
            )
            self.assertEqual(card_state(session, legacy).status, "STALE")
            self.assertIn("current-state", card_state(session, legacy).detail)
            transcript.write_text("one\ntwo\n")
            self.assertEqual(card_state(session, current).status, "STALE")

    def test_interview_requires_strict_provider_authored_json(self) -> None:
        class Consultation:
            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return None

            def ask(self, prompt):
                self.prompt = prompt
                return json.dumps(
                    {
                        "scope": "Owns exact identity and recovery across Pika.",
                        "current_state": (
                            "Recovery is verified; lease handling is the current focus."
                        ),
                        "topics": ["session identity", "tmux recovery", "PID leases"],
                        "artifacts": ["src/pikamux/core.py"],
                    }
                )

        with tempfile.TemporaryDirectory() as directory:
            transcript = Path(directory) / "thread.jsonl"
            transcript.write_text("history\n")
            session = Session("codex", "one", transcript_path=str(transcript))
            consultation = Consultation()
            with patch(
                "pikamux.experts.consultation_for", return_value=consultation
            ):
                profile = interview_profile(session)
            self.assertEqual(profile.source, "interview")
            self.assertEqual(profile.scope, "Owns exact identity and recovery across Pika.")
            self.assertIn("lease handling", profile.current_state)
            self.assertIn("entire inherited conversation", consultation.prompt)
            self.assertIn("not a recap of the latest work", consultation.prompt)
            self.assertEqual(
                (profile.transcript_mtime_ns, profile.transcript_size),
                (transcript.stat().st_mtime_ns, transcript.stat().st_size),
            )

            consultation.ask = lambda _prompt: "not JSON"
            with (
                patch("pikamux.experts.consultation_for", return_value=consultation),
                self.assertRaisesRegex(ConsultationError, "valid JSON"),
            ):
                interview_profile(session)
    def test_rank_is_deterministic_and_exposes_why(self) -> None:
        sessions = [
            Session(
                "codex",
                "one",
                name="reporting",
                cwd="/work/attribution",
                status=Status.WORKING.value,
                live=True,
            ),
            Session(
                "claude",
                "two",
                name="research",
                cwd="/work/research",
                status=Status.PARKED.value,
            ),
        ]
        profiles = [
            ExpertProfile(
                "codex",
                "one",
                "Built the production factor attribution pipeline.",
                ("factor attribution", "portfolio analytics"),
                ("reports/attribution.md",),
                100,
                current_state="Investigating a live attribution mismatch.",
            ),
            ExpertProfile(
                "claude",
                "two",
                "Studied factor definitions.",
                ("factor research",),
                (),
                200,
                current_state="No active task.",
            ),
        ]
        matches = rank_experts(profiles, sessions, "factor attribution")
        self.assertEqual([item.session.session_id for item in matches], ["one"])
        self.assertIn("topic", matches[0].matched_on)
        self.assertIn("scope", matches[0].matched_on)
        current = rank_experts(profiles, sessions, "live mismatch")
        self.assertEqual(current[0].session.session_id, "one")
        self.assertIn("now", current[0].matched_on)
        payload = current[0].to_dict()
        self.assertEqual(payload["scope"], profiles[0].scope)
        self.assertEqual(payload["current_state"], profiles[0].current_state)

    def test_rank_matches_tokens_not_incidental_substrings(self) -> None:
        sessions = [
            Session("codex", "ai", name="ai-components"),
            Session("codex", "daily", name="returns"),
        ]
        profiles = [
            ExpertProfile("codex", "ai", "Built AI component screens.", ("AI",)),
            ExpertProfile(
                "codex",
                "daily",
                "Operates the daily returns pipeline.",
                ("daily returns",),
            ),
        ]

        self.assertEqual(
            [
                match.session.session_id
                for match in rank_experts(profiles, sessions, "ai")
            ],
            ["ai"],
        )
        self.assertEqual(rank_experts(profiles, sessions, "the"), [])
        self.assertEqual(rank_experts(profiles, sessions, "ai unrelated"), [])

    def test_rank_normalizes_hyphens_underscores_and_paths(self) -> None:
        session = Session("codex", "one", name="factor_weights")
        profile = ExpertProfile(
            "codex",
            "one",
            "Built the factor-weight publication.",
            ("publication",),
            ("reports/factor_weights.parquet",),
        )

        matches = rank_experts([profile], [session], "factor weights")
        self.assertEqual([match.session.session_id for match in matches], ["one"])
        self.assertIn("scope", matches[0].matched_on)

    def test_profiles_are_cleaned_but_not_invented(self) -> None:
        profile = make_profile(
            Session("codex", "one"),
            summary="  Built   real work. ",
            current_state="  Validating   the next release. ",
            topics=["attribution", "Attribution", " risk "],
            artifacts=[" reports/result.md "],
        )
        self.assertEqual(profile.summary, "Built real work.")
        self.assertEqual(profile.current_state, "Validating the next release.")
        self.assertEqual(profile.topics, ("attribution", "risk"))
        self.assertEqual(profile.artifacts, ("reports/result.md",))
        with self.assertRaisesRegex(ValueError, "at least one"):
            make_profile(Session("codex", "one"), summary="work", topics=[])

    def test_only_the_exact_calling_pane_can_publish(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        store = Store(Path(temporary.name) / "pika.db")
        session = Session(
            "codex",
            "exact-id",
            name="expert",
            tmux_session="home",
            tmux_pane="%1",
        )
        store.upsert_session(session)
        pane = Pane(
            "home",
            "%1",
            123,
            "/tmp",
            "codex",
            True,
            False,
            None,
            1,
            1,
            pika_provider="codex",
            pika_session_id="exact-id",
        )
        pika = Pika(store, OnePaneTmux(pane), {})
        with (
            patch.dict(os.environ, {"TMUX_PANE": "%1"}),
            patch.object(pika, "refresh", return_value=[session]),
            patch.object(pika, "exact_pane_pid", return_value=999),
        ):
            profile = pika.publish_expert(
                summary="Owns this implementation.",
                current_state="Testing exact recovery now.",
                topics=["identity"],
            )
        self.assertEqual(profile.session_id, "exact-id")

        with (
            patch.dict(os.environ, {"TMUX_PANE": "%1"}),
            patch.object(pika, "refresh", return_value=[session]),
            patch.object(pika, "exact_pane_pid", return_value=None),
            self.assertRaisesRegex(PikaError, "cannot prove"),
        ):
            pika.publish_expert(
                summary="Counterfeit",
                current_state="Pretending to work.",
                topics=["anything"],
            )
        self.assertEqual(
            store.get_expert_profile("codex", "exact-id").summary,
            "Owns this implementation.",
        )
