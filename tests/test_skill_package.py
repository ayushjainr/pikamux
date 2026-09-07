import io
import json
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

from pikamux.cli import run, _parser
from pikamux.skill_package import install_skill, skill_text


class SkillPackageTests(unittest.TestCase):
    def test_install_backup_idempotence_and_preserve_resources(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "SKILL.md").write_text("user instructions")
            (root / "notes.md").write_text("user resource")
            result = install_skill(root)
            self.assertEqual(Path(result["backup"]).read_text(), "user instructions")
            self.assertEqual((root / "SKILL.md").read_text(), skill_text())
            self.assertEqual((root / "notes.md").read_text(), "user resource")
            self.assertEqual(list(root.rglob("SKILL.md")), [root / "SKILL.md"])
            self.assertEqual(Path(result["backup"]).suffix, ".pika-backup")
            self.assertFalse(install_skill(root)["changed"])

    def test_install_does_not_initialize_state_or_call_providers(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch("pikamux.cli.Pika", side_effect=AssertionError("model/runtime not needed")):
                with redirect_stdout(io.StringIO()) as output:
                    self.assertEqual(run(["skill", "install", directory, "--json"]), 0)
                self.assertTrue(json.loads(output.getvalue())["changed"])

    def test_documented_core_commands_parse(self):
        parser = _parser()
        for argv in [
            ["experts", "factor attribution", "--json"],
            ["expert", "status", "--json"],
            ["explain", "id@machine", "--json"],
            ["ask", "id@machine", "--jsonl"],
            ["expert", "update", "--now", "Waiting for a decision"],
            ["expert", "publish", "--scope", "Mandate", "--now", "Working", "--topic", "methodology"],
        ]:
            with self.subTest(argv=argv):
                self.assertIsNotNone(parser.parse_args(argv))
