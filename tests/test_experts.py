from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from pikamux.core import Pika, PikaError
from pikamux.consult import ConsultationError
from pikamux.experts import card_state, interview_profile, make_profile, rank_experts
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
                topics=["work"],
                transcript_mtime_ns=stat.st_mtime_ns,
                transcript_size=stat.st_size,
            )
            self.assertEqual(card_state(session, current).status, "CURRENT")
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
                        "summary": "Built and verified exact identity recovery.",
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
            self.assertIn("personally completed", consultation.prompt)
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
                name="master_attr",
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
            ),
            ExpertProfile(
                "claude",
                "two",
                "Studied factor definitions.",
                ("factor research",),
                (),
                200,
            ),
        ]
        matches = rank_experts(profiles, sessions, "factor attribution")
        self.assertEqual([item.session.session_id for item in matches], ["one", "two"])
        self.assertIn("topic", matches[0].matched_on)
        self.assertIn("summary", matches[0].matched_on)
        self.assertGreater(matches[0].score, matches[1].score)

    def test_profiles_are_cleaned_but_not_invented(self) -> None:
        profile = make_profile(
            Session("codex", "one"),
            summary="  Built   real work. ",
            topics=["attribution", "Attribution", " risk "],
            artifacts=[" reports/result.md "],
        )
        self.assertEqual(profile.summary, "Built real work.")
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
                summary="Owns this implementation.", topics=["identity"]
            )
        self.assertEqual(profile.session_id, "exact-id")

        with (
            patch.dict(os.environ, {"TMUX_PANE": "%1"}),
            patch.object(pika, "refresh", return_value=[session]),
            patch.object(pika, "exact_pane_pid", return_value=None),
            self.assertRaisesRegex(PikaError, "cannot prove"),
        ):
            pika.publish_expert(summary="Counterfeit", topics=["anything"])
        self.assertEqual(
            store.get_expert_profile("codex", "exact-id").summary,
            "Owns this implementation.",
        )
