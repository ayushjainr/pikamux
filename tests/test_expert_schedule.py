from __future__ import annotations

import tempfile
import os
import plistlib
import subprocess
import sys
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

from pikamux.expert_schedule import LAUNCHD_LABEL, LAUNCHD_NAME, TIMER_NAME, activate_timer, unit_contents


class ExpertScheduleTests(unittest.TestCase):
    def setUp(self) -> None:
        platform = patch("pikamux.expert_schedule._MACOS", False)
        platform.start()
        self.addCleanup(platform.stop)

    def test_timer_runs_a_finite_quota_aware_command(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with patch.dict("os.environ", {"XDG_CONFIG_HOME": directory}):
                units = unit_contents()
        service = next(
            value for path, value in units.items() if path.suffix == ".service"
        )
        timer = next(value for path, value in units.items() if path.suffix == ".timer")
        self.assertIn("expert refresh --due --json", service)
        self.assertIn("Type=oneshot", service)
        self.assertIn('Environment="PATH=', service)
        self.assertIn("OnUnitActiveSec=10min", timer)
        self.assertIn("Persistent=true", timer)

    @patch("pikamux.expert_schedule.subprocess.run")
    @patch("pikamux.expert_schedule.shutil.which", return_value="/usr/bin/systemctl")
    def test_activation_reloads_and_enables_user_timer(self, _which, run) -> None:
        run.return_value.returncode = 0
        active, _detail = activate_timer()
        self.assertTrue(active)
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args_list[-1].args[0][-1], TIMER_NAME)


class MacScheduleTests(unittest.TestCase):
    def setUp(self):
        platform = patch("pikamux.expert_schedule._MACOS", True)
        platform.start()
        self.addCleanup(platform.stop)

    def test_plist_keeps_argv_boundaries_and_does_not_interview_at_setup(self):
        with patch("pikamux.expert_schedule.sys.executable", "/path with spaces/python"):
            units = unit_contents(runtime_path="/path & tools:/usr/bin")
        path, content = next(iter(units.items()))
        self.assertEqual(path.name, LAUNCHD_NAME)
        payload = plistlib.loads(content.encode())
        self.assertEqual(payload["ProgramArguments"][0], "/path with spaces/python")
        self.assertEqual(payload["ProgramArguments"][-4:], ["expert", "refresh", "--due", "--json"])
        self.assertEqual(payload["EnvironmentVariables"]["PATH"], "/path & tools:/usr/bin")
        self.assertEqual(payload["StartInterval"], 600)
        self.assertNotIn("RunAtLoad", payload)
        self.assertNotIn("KeepAlive", payload)

    def test_activation_verifies_load_without_kickstarting(self):
        results = [subprocess.CompletedProcess([], code, "", "") for code in (113, 0, 0, 0)]
        with (
            patch("pikamux.expert_schedule.shutil.which", return_value="/bin/launchctl"),
            patch("pikamux.expert_schedule.subprocess.run", side_effect=results) as run,
        ):
            active, _ = activate_timer()
        self.assertTrue(active)
        self.assertEqual([call.args[0][1] for call in run.call_args_list], ["print", "enable", "bootstrap", "print"])

    def test_existing_job_is_never_terminated_to_reload_configuration(self):
        with (
            patch("pikamux.expert_schedule.shutil.which", return_value="/bin/launchctl"),
            patch("pikamux.expert_schedule.subprocess.run", return_value=subprocess.CompletedProcess([], 0, "", "")) as run,
        ):
            active, detail = activate_timer()
        self.assertTrue(active)
        self.assertIn("next desktop login", detail)
        self.assertEqual(run.call_count, 1)

    def test_failed_bootstrap_has_manual_recovery_and_no_success_claim(self):
        results = [subprocess.CompletedProcess([], code, "", "no GUI domain") for code in (113, 0, 5)]
        with (
            patch("pikamux.expert_schedule.shutil.which", return_value="/bin/launchctl"),
            patch("pikamux.expert_schedule.subprocess.run", side_effect=results),
        ):
            active, detail = activate_timer()
        self.assertFalse(active)
        self.assertIn("pika expert refresh --due", detail)

    @unittest.skipUnless(sys.platform == "darwin", "Apple plist validator")
    def test_generated_plist_passes_native_validator(self):
        content = next(iter(unit_contents().values()))
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / LAUNCHD_NAME
            target.write_text(content)
            result = subprocess.run(["plutil", "-lint", str(target)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    @unittest.skipUnless(sys.platform == "darwin", "native launchd")
    def test_native_launchd_load_and_verification_with_inert_test_job(self):
        domain = f"gui/{os.getuid()}"
        probe = subprocess.run(["launchctl", "print", domain], capture_output=True, timeout=5)
        if probe.returncode:
            self.skipTest("no desktop launchd domain on this runner")
        label = "io.pikamux.test-" + uuid.uuid4().hex
        target = f"{domain}/{label}"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / (label + ".plist")
            payload = plistlib.loads(next(iter(unit_contents().values())).encode())
            payload["Label"] = label
            payload["ProgramArguments"] = ["/usr/bin/true"]
            path.write_bytes(plistlib.dumps(payload))
            try:
                with (
                    patch("pikamux.expert_schedule.LAUNCHD_LABEL", label),
                    patch("pikamux.expert_schedule.LAUNCHD_NAME", path.name),
                    patch("pikamux.expert_schedule.unit_directory", return_value=Path(directory)),
                ):
                    active, detail = activate_timer()
                self.assertTrue(active, detail)
                result = subprocess.run(["launchctl", "print", target], capture_output=True, timeout=5)
                self.assertEqual(result.returncode, 0)
            finally:
                subprocess.run(["launchctl", "bootout", target], capture_output=True, timeout=5)


if __name__ == "__main__":
    unittest.main()
