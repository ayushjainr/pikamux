from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from pikamux.processes import (
    _canonical_identity_pids,
    _process_kind,
    opencode_session_processes,
    process_tty,
    provider_ancestor,
    shared_provider_process,
)


class ProcessTests(unittest.TestCase):
    def test_opencode_session_processes_batches_one_proc_scan(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = {
                "101": ["/opt/opencode", "--session", "ses_first123"],
                "202": ["opencode", "-s", "ses_second123"],
                "303": ["opencode", "--session=ses_first123"],
                "404": ["codex", "--session", "ses_ignore123"],
            }
            for pid, argv in commands.items():
                target = root / pid
                target.mkdir()
                (target / "cmdline").write_bytes(
                    b"\0".join(value.encode() for value in argv) + b"\0"
                )
            with patch("pikamux.processes.parent_pid", return_value=None):
                observed = opencode_session_processes(root)
        self.assertEqual(observed["ses_first123"], [101, 303])
        self.assertEqual(observed["ses_second123"], [202])
        self.assertNotIn("ses_ignore123", observed)

    def test_process_tty_reports_only_terminal_devices(self) -> None:
        with patch(
            "pikamux.processes.os.readlink",
            side_effect=[OSError(), "/dev/pts/39"],
        ):
            self.assertEqual(process_tty(2997494), "/dev/pts/39")
        with patch("pikamux.processes.os.readlink", return_value="/tmp/output"):
            self.assertIsNone(process_tty(2997494))

    def test_uuid_process_aliases_collapse_only_within_one_direct_tree(self) -> None:
        parents = {200: 100, 300: 1}
        with patch(
            "pikamux.processes.parent_pid", side_effect=lambda pid: parents.get(pid)
        ):
            self.assertEqual(_canonical_identity_pids([100, 200, 300]), [200, 300])

    def test_agent_executables_are_classified_by_basename(self) -> None:
        self.assertEqual(_process_kind(["/usr/bin/claude", "--resume", "id"]), "claude")
        self.assertEqual(
            _process_kind(["node", "/opt/codex/bin/codex.js", "resume", "id"]),
            "codex",
        )
        self.assertEqual(
            _process_kind(["/home/user/.opencode/bin/opencode", "--session", "ses_x"]),
            "opencode",
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

    def test_opencode_process_is_a_renewable_multi_root_lease(self) -> None:
        with patch(
            "pikamux.processes.cmdline",
            return_value=["/home/user/.opencode/bin/opencode", "--session", "ses_one"],
        ):
            self.assertTrue(shared_provider_process(123, "opencode"))


if __name__ == "__main__":
    unittest.main()
