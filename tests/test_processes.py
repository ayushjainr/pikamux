from __future__ import annotations

import unittest
from unittest.mock import patch

from pikamux.processes import (
    _process_kind,
    provider_ancestor,
    shared_provider_process,
)


class ProcessTests(unittest.TestCase):
    def test_agent_executables_are_classified_by_basename(self) -> None:
        self.assertEqual(_process_kind(["/usr/bin/claude", "--resume", "id"]), "claude")
        self.assertEqual(
            _process_kind(["node", "/opt/codex/bin/codex.js", "resume", "id"]),
            "codex",
        )

    def test_unrelated_paths_do_not_look_like_agent_processes(self) -> None:
        self.assertIsNone(
            _process_kind(
                ["bash", "-lc", "python hook.py", "/tmp/claude/settings.json"]
            )
        )
        self.assertIsNone(
            _process_kind(["python", "-m", "pikamux", "/tmp/codex/state.sqlite"])
        )

    def test_hook_process_walks_up_to_provider_ancestor(self) -> None:
        parents = {30: 20, 20: 10, 10: 1}
        commands = {30: ["python", "-m", "pikamux"], 20: ["sh", "-c"], 10: ["codex"]}
        with (
            patch(
                "pikamux.processes.parent_pid", side_effect=lambda pid: parents.get(pid)
            ),
            patch(
                "pikamux.processes.cmdline",
                side_effect=lambda pid: commands.get(pid, []),
            ),
        ):
            self.assertEqual(provider_ancestor(30, "codex"), 10)

    def test_codex_app_server_is_shared_infrastructure(self) -> None:
        with patch(
            "pikamux.processes.cmdline",
            return_value=["/opt/codex", "-c", "feature=true", "app-server"],
        ):
            self.assertTrue(shared_provider_process(123, "codex"))
            self.assertFalse(shared_provider_process(123, "claude"))

    def test_regular_codex_resume_is_not_shared_infrastructure(self) -> None:
        with patch(
            "pikamux.processes.cmdline",
            return_value=["/opt/codex", "resume", "exact-uuid"],
        ):
            self.assertFalse(shared_provider_process(123, "codex"))


if __name__ == "__main__":
    unittest.main()
