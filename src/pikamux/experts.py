from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable

from .models import ExpertProfile, Session

_TERM = re.compile(r"[\w.-]+", re.UNICODE)


@dataclass(slots=True)
class ExpertMatch:
    profile: ExpertProfile
    session: Session
    score: int
    matched_on: tuple[str, ...]

    def to_dict(self) -> dict[str, Any]:
        return {
            "provider": self.session.provider,
            "session_id": self.session.session_id,
            "name": self.session.name,
            "project": self.session.cwd,
            "branch": self.session.branch,
            "status": self.session.status,
            "live": self.session.live,
            "summary": self.profile.summary,
            "topics": list(self.profile.topics),
            "artifacts": list(self.profile.artifacts),
            "profile_updated_at": self.profile.updated_at,
            "score": self.score,
            "matched_on": list(self.matched_on),
        }


def make_profile(
    session: Session,
    *,
    summary: str,
    topics: Iterable[str],
    artifacts: Iterable[str] = (),
) -> ExpertProfile:
    clean_summary = _clean(summary, label="summary", limit=600)
    clean_topics = _clean_many(topics, label="topic", limit=80, maximum=12)
    clean_artifacts = _clean_many(
        artifacts, label="artifact", limit=500, maximum=12, required=False
    )
    if not clean_topics:
        raise ValueError("Publish at least one expert topic")
    return ExpertProfile(
        provider=session.provider,
        session_id=session.session_id,
        summary=clean_summary,
        topics=clean_topics,
        artifacts=clean_artifacts,
    )


def rank_experts(
    profiles: Iterable[ExpertProfile],
    sessions: Iterable[Session],
    query: str = "",
) -> list[ExpertMatch]:
    session_by_key = {item.key: item for item in sessions}
    phrase = " ".join(query.casefold().split())
    terms = tuple(dict.fromkeys(_TERM.findall(phrase)))
    matches: list[ExpertMatch] = []
    for profile in profiles:
        session = session_by_key.get(profile.key)
        if session is None:
            continue
        fields = (
            ("topic", " ".join(profile.topics), 10),
            ("summary", profile.summary, 6),
            ("name", session.name or "", 4),
            ("project", " ".join(filter(None, (session.cwd, session.branch))), 3),
            ("artifact", " ".join(profile.artifacts), 2),
        )
        score = 0
        matched_on: list[str] = []
        for label, value, weight in fields:
            folded = value.casefold()
            hits = sum(term in folded for term in terms)
            if hits:
                score += hits * weight
                matched_on.append(label)
            if phrase and phrase in folded:
                score += weight * 2
        if phrase and not score:
            continue
        matches.append(
            ExpertMatch(profile, session, score, tuple(dict.fromkeys(matched_on)))
        )
    return sorted(
        matches,
        key=lambda item: (
            item.score,
            item.session.live,
            item.profile.updated_at,
            item.session.session_id,
        ),
        reverse=True,
    )


def project_label(cwd: str | None) -> str:
    if not cwd:
        return "—"
    return Path(cwd).name or cwd


def _clean(value: str, *, label: str, limit: int) -> str:
    clean = " ".join(value.split())
    if not clean:
        raise ValueError(f"Expert {label} cannot be empty")
    if len(clean) > limit:
        raise ValueError(f"Expert {label} must be {limit} characters or fewer")
    if not all(character.isprintable() for character in clean):
        raise ValueError(f"Expert {label} contains control characters")
    return clean


def _clean_many(
    values: Iterable[str],
    *,
    label: str,
    limit: int,
    maximum: int,
    required: bool = True,
) -> tuple[str, ...]:
    result: list[str] = []
    seen: set[str] = set()
    for value in values:
        clean = _clean(value, label=label, limit=limit)
        folded = clean.casefold()
        if folded in seen:
            continue
        seen.add(folded)
        result.append(clean)
    if required and not result:
        raise ValueError(f"Publish at least one expert {label}")
    if len(result) > maximum:
        raise ValueError(f"Publish at most {maximum} expert {label}s")
    return tuple(result)
