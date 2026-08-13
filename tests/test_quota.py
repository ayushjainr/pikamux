from __future__ import annotations

import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

from pikamux.quota import (
    OBSERVATION_MAX_AGE_SECONDS,
    parse_codex_quota,
    read_claude_quota,
)


class QuotaTests(unittest.TestCase):
    def test_codex_selects_the_weekly_window(self) -> None:
        quota = parse_codex_quota(
            {
                "rateLimits": {
                    "primary": {
                        "usedPercent": 25,
                        "windowDurationMins": 300,
                        "resetsAt": 2_000,
                    },
                    "secondary": {
                        "usedPercent": 72,
                        "windowDurationMins": 10_080,
                        "resetsAt": 3_000,
                    },
                }
            },
            observed_at=1_000,
        )
        self.assertEqual(quota.used_percent if quota else None, 72)
        self.assertEqual(quota.remaining_percent if quota else None, 28)
        self.assertEqual(quota.reset_at if quota else None, 3_000)

    def test_codex_fails_closed_without_a_weekly_window(self) -> None:
        self.assertIsNone(
            parse_codex_quota(
                {
                    "rateLimits": {
                        "primary": {"usedPercent": 1, "resetsAt": 2_000}
                    }
                },
                observed_at=1_000,
            )
        )

    def test_claude_requires_a_fresh_weekly_observation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / ".claude.json"
            now = 2_000_000.0
            reset = datetime.fromtimestamp(now + 3_600, tz=timezone.utc).isoformat()
            payload = {
                "cachedUsageUtilization": {
                    "fetchedAtMs": int((now - 60) * 1000),
                    "utilization": {
                        "seven_day": {"utilization": 84, "resets_at": reset}
                    },
                }
            }
            path.write_text(json.dumps(payload))
            quota = read_claude_quota(path=path, now=now)
            self.assertEqual(quota.remaining_percent if quota else None, 16)

            payload["cachedUsageUtilization"]["fetchedAtMs"] = int(
                (now - OBSERVATION_MAX_AGE_SECONDS - 1) * 1000
            )
            path.write_text(json.dumps(payload))
            self.assertIsNone(read_claude_quota(path=path, now=now))


if __name__ == "__main__":
    unittest.main()
