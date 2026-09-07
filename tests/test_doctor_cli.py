from __future__ import annotations

import io
import json
import tempfile
import time
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

from pikamux.cli import run
from pikamux.store import Store


class DoctorCliTests(unittest.TestCase):
    def test_corrupt_database_still_returns_machine_readable_unsafe_receipt(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            database = Path(directory) / "pika.db"
            database.write_text("not a sqlite database")
            output = io.StringIO()
            with (
                patch.dict("os.environ", {"PIKA_DB_PATH": str(database)}, clear=False),
                redirect_stdout(output),
            ):
                self.assertEqual(run(["doctor", "--json"]), 1)
        receipt = json.loads(output.getvalue())
        self.assertFalse(receipt["safe_to_disconnect"])
        self.assertEqual(receipt["checks"][0]["level"], "error")
        self.assertIn("unreadable", receipt["checks"][0]["message"])

    def test_malformed_pika_config_is_an_explicit_doctor_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_home = root / "config"
            config_home.mkdir()
            (config_home / "config.json").write_text("{")
            output = io.StringIO()
            with (
                patch.dict(
                    "os.environ",
                    {
                        "PIKA_CONFIG_HOME": str(config_home),
                        "PIKA_STATE_HOME": str(root / "state"),
                        "CODEX_HOME": str(root / "codex"),
                        "CLAUDE_CONFIG_DIR": str(root / "claude"),
                    },
                    clear=False,
                ),
                redirect_stdout(output),
            ):
                self.assertEqual(run(["doctor", "--json"]), 1)
        receipt = json.loads(output.getvalue())
        matching = [
            check
            for check in receipt["checks"]
            if check["name"] == "configuration format"
        ]
        self.assertEqual(len(matching), 1)
        self.assertEqual(matching[0]["level"], "error")

    def test_repair_stale_json_is_one_receipt_with_exact_repairs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / "state"
            store = Store(state / "pika.db")
            store.add_pending("stale-token", "codex", "lost", "/tmp")
            with store.connect() as db:
                db.execute(
                    "UPDATE pending_launches SET created_at=?",
                    (time.time() - 600,),
                )
            output = io.StringIO()
            with (
                patch.dict(
                    "os.environ",
                    {
                        "PIKA_CONFIG_HOME": str(root / "config"),
                        "PIKA_STATE_HOME": str(state),
                        "CODEX_HOME": str(root / "codex"),
                        "CLAUDE_CONFIG_DIR": str(root / "claude"),
                    },
                    clear=False,
                ),
                redirect_stdout(output),
            ):
                self.assertEqual(run(["doctor", "--repair-stale", "--json"]), 1)
            receipt = json.loads(output.getvalue())
            self.assertEqual(len(receipt["repairs"]), 1)
            self.assertIn("stale-token", receipt["repairs"][0])
            self.assertIsNone(store.get_pending("stale-token"))


if __name__ == "__main__":
    unittest.main()
