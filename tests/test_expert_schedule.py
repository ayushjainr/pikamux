from __future__ import annotations

import tempfile
import unittest
from unittest.mock import patch

from pikamux.expert_schedule import TIMER_NAME, activate_timer, unit_contents


class ExpertScheduleTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
