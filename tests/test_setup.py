from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from pikamux.setup_hooks import (
    FileChange,
    apply_changes,
    claude_settings_change,
    codex_config_change,
    codex_hooks_change,
    codex_hooks_enabled,
    hooks_installed,
)


class SetupTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_json_hook_merges_preserve_existing_entries_and_are_idempotent(
        self,
    ) -> None:
        codex = self.root / "codex"
        claude = self.root / "claude"
        codex.mkdir()
        claude.mkdir()
        (codex / "hooks.json").write_text(
            json.dumps(
                {
                    "hooks": {
                        "Stop": [
                            {"hooks": [{"type": "command", "command": "existing"}]}
                        ]
                    }
                }
            )
        )
        (claude / "settings.json").write_text(
            json.dumps({"model": "opus", "hooks": {}})
        )
        first = [codex_hooks_change(codex), claude_settings_change(claude)]
        apply_changes(first)
        codex_data = json.loads((codex / "hooks.json").read_text())
        claude_data = json.loads((claude / "settings.json").read_text())
        self.assertEqual(
            codex_data["hooks"]["Stop"][0]["hooks"][0]["command"], "existing"
        )
        self.assertEqual(claude_data["model"], "opus")
        claude_handler = claude_data["hooks"]["SessionStart"][0]["hooks"][0]
        self.assertIn("-m pikamux hook --provider claude", claude_handler["command"])
        self.assertNotIn("args", claude_handler)
        self.assertFalse(codex_hooks_change(codex).changed)
        self.assertFalse(claude_settings_change(claude).changed)

    def test_codex_features_section_is_updated_without_duplicate(self) -> None:
        codex = self.root / "codex"
        codex.mkdir()
        (codex / "config.toml").write_text(
            "[features]\nhooks = false\napps = true\n\n[other]\nx = 1\n"
        )
        change = codex_config_change(codex)
        self.assertIn("hooks = true", change.after)
        self.assertEqual(change.after.count("hooks ="), 1)
        self.assertIn("[other]", change.after)

    def test_commented_features_header_and_similar_key_are_preserved(self) -> None:
        codex = self.root / "codex"
        codex.mkdir()
        (codex / "config.toml").write_text(
            "[features] # lifecycle\nhooks_extra = false\nhooks = false # pika\n"
        )
        change = codex_config_change(codex)
        self.assertEqual(change.after.count("[features]"), 1)
        self.assertIn("hooks_extra = false", change.after)
        self.assertIn("hooks = true # pika", change.after)
        (codex / "config.toml").write_text(change.after)
        self.assertTrue(codex_hooks_enabled(codex))

    def test_backups_are_collision_safe_and_restore_original_bytes(self) -> None:
        target = self.root / "settings.json"
        target.write_text('{"version":1}\n')
        first = apply_changes(
            [FileChange(target, target.read_text(), '{"version":2}\n')]
        )
        second = apply_changes(
            [FileChange(target, target.read_text(), '{"version":3}\n')]
        )
        self.assertEqual(len(first), 1)
        self.assertEqual(len(second), 1)
        self.assertNotEqual(first[0], second[0])
        self.assertEqual(first[0].read_text(), '{"version":1}\n')
        self.assertEqual(second[0].read_text(), '{"version":2}\n')
        shutil.copy2(first[0], target)
        self.assertEqual(target.read_text(), '{"version":1}\n')

    def test_stale_hook_interpreter_is_replaced_and_not_reported_installed(
        self,
    ) -> None:
        codex = self.root / "codex"
        codex.mkdir()
        stale = {
            "hooks": {
                event: [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": (
                                    "/missing/pika-python -m pikamux hook "
                                    "--provider codex"
                                ),
                            }
                        ]
                    }
                ]
                for event in (
                    "SessionStart",
                    "UserPromptSubmit",
                    "PermissionRequest",
                    "PostToolUse",
                    "Stop",
                    "SessionEnd",
                )
            }
        }
        (codex / "hooks.json").write_text(json.dumps(stale))
        with patch("pikamux.setup_hooks.codex_home", return_value=codex):
            self.assertFalse(hooks_installed("codex"))
        change = codex_hooks_change(codex)
        data = json.loads(change.after)
        self.assertEqual(len(data["hooks"]["SessionStart"]), 1)
        command = data["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        self.assertNotIn("/missing/pika-python", command)

    def test_invalid_codex_toml_fails_closed_when_cli_is_available(self) -> None:
        codex = self.root / "codex"
        codex.mkdir()
        (codex / "config.toml").write_text('model = "unterminated\n')
        with self.assertRaisesRegex(ValueError, "invalid Codex config"):
            codex_config_change(codex)

    def test_setup_explicitly_reenables_disabled_claude_hooks(self) -> None:
        claude = self.root / "claude"
        claude.mkdir()
        (claude / "settings.json").write_text('{"disableAllHooks":true}\n')
        change = claude_settings_change(claude)
        self.assertFalse(json.loads(change.after)["disableAllHooks"])

    def test_json_merge_preserves_existing_top_level_key_order(self) -> None:
        claude = self.root / "claude-order"
        claude.mkdir()
        (claude / "settings.json").write_text(
            '{\n  "zeta": 1,\n  "model": "opus",\n  "hooks": {}\n}\n'
        )
        after = claude_settings_change(claude).after
        self.assertLess(after.index('"zeta"'), after.index('"model"'))
        self.assertLess(after.index('"model"'), after.index('"hooks"'))


if __name__ == "__main__":
    unittest.main()
