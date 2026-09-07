from __future__ import annotations

import unittest

from pikamux.executables import (
    provider_compatibility_error,
    provider_version_supported,
)


class ExecutableCompatibilityTests(unittest.TestCase):
    def test_opencode_requires_certified_minimum_version(self) -> None:
        self.assertIn(
            "requires opencode >= 1.18.21",
            provider_compatibility_error("opencode", "opencode 1.18.20") or "",
        )
        self.assertFalse(provider_version_supported("opencode", "1.18.20"))
        self.assertFalse(provider_version_supported("opencode", "development"))

    def test_opencode_accepts_minimum_and_future_versions(self) -> None:
        self.assertTrue(provider_version_supported("opencode", "opencode 1.18.21"))
        self.assertTrue(provider_version_supported("opencode", "1.19.0-alpha.1"))
        self.assertIsNone(provider_compatibility_error("codex", "codex-cli 0.1"))


if __name__ == "__main__":
    unittest.main()
