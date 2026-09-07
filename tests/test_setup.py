from __future__ import annotations

import json
import os
import shutil
import subprocess
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
    opencode_plugin_change,
    pika_config_change,
)


class SetupTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_machine_alias_is_persisted_as_deployment_configuration(self) -> None:
        target = self.root / "config.json"
        with patch("pikamux.setup_hooks.config_path", return_value=target):
            change = pika_config_change("codex", "devbox")
        self.assertEqual(json.loads(change.after)["machine_alias"], "devbox")

    def test_provider_executables_and_runtime_path_are_persisted(self) -> None:
        target = self.root / "config.json"
        with patch("pikamux.setup_hooks.config_path", return_value=target):
            change = pika_config_change(
                "codex",
                provider_executables={
                    "codex": "/opt/codex/bin/codex",
                    "claude": "/opt/claude/bin/claude",
                },
                provider_runtime_path="/opt/codex/bin:/opt/claude/bin:/usr/bin",
            )
        data = json.loads(change.after)
        self.assertEqual(data["provider_executables"]["codex"], "/opt/codex/bin/codex")
        self.assertEqual(
            data["provider_runtime_path"],
            "/opt/codex/bin:/opt/claude/bin:/usr/bin",
        )

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
        self.assertIn("PreToolUse", codex_data["hooks"])
        self.assertIn("PreToolUse", claude_data["hooks"])
        self.assertEqual(
            codex_data["hooks"]["PreToolUse"][0]["matcher"],
            "^request_user_input$",
        )
        self.assertEqual(
            claude_data["hooks"]["PreToolUse"][0]["matcher"],
            "^AskUserQuestion$",
        )
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
                    "PreToolUse",
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

    def test_opencode_plugin_is_idempotent_and_maps_attention_events(self) -> None:
        home = self.root / "opencode"
        change = opencode_plugin_change(home)
        self.assertIn('event.type === "question.asked"', change.after)
        self.assertIn('send("QuestionRequest"', change.after)
        self.assertIn("client.session.update", change.after)
        self.assertIn("path: { id }", change.after)
        self.assertIn("body: { title: desired }", change.after)
        self.assertIn("p.info.parentID", change.after)
        self.assertLess(
            change.after.index('event.type === "session.deleted"'),
            change.after.index("client.session.get"),
        )
        self.assertIn("updated && updated.error", change.after)
        self.assertIn("observedTitle !== desired", change.after)
        self.assertIn("deleted: true", change.after)
        self.assertIn('send("SessionHeartbeat", currentRootId)', change.after)
        self.assertIn("}, 60000)", change.after)
        self.assertIn("heartbeat.unref()", change.after)
        self.assertIn("expectedRootId", change.after)
        self.assertEqual(
            change.after.count('send("SessionHeartbeat", currentRootId)'), 2
        )
        self.assertIn("process.env.PIKA_SESSION_ID || null", change.after)
        self.assertIn("let currentRootId = expectedRootId", change.after)
        self.assertIn("initialNameClaimed", change.after)
        apply_changes([change])
        self.assertFalse(opencode_plugin_change(home).changed)
        with patch("pikamux.setup_hooks.opencode_config_home", return_value=home):
            self.assertTrue(hooks_installed("opencode"))

    @unittest.skipUnless(shutil.which("node"), "node is required for plugin execution")
    def test_opencode_plugin_executes_delete_and_name_verification_paths(self) -> None:
        home = self.root / "plugin-runtime"
        home.mkdir()
        (home / "pika.mjs").write_text(opencode_plugin_change(home).after)
        (home / "run.mjs").write_text(
            '''import { Pika } from "./pika.mjs";
const payloads = [];
let gets = 0;
globalThis.Bun = { spawn: () => {
  let value = "";
  return {
    stdin: {
      write: (part) => { value += part; },
      end: () => { payloads.push(JSON.parse(value)); },
    },
    exited: Promise.resolve(0),
  };
}};
const client = { session: {
  get: async () => { gets += 1; return { data: { id: "ses_created123", parentID: null, title: "default" } }; },
  update: async () => ({ error: "rename denied" }),
}};
const plugin = await Pika({ client, directory: "/repo" });
await plugin.event({ event: { type: "session.deleted", properties: { info: { id: "ses_deleted123", parentID: null, title: "gone" } } } });
const getsAfterDelete = gets;
await plugin.event({ event: { type: "session.created", properties: { info: { id: "ses_created123", parentID: null, title: "default" } } } });
console.log(JSON.stringify({ payloads, getsAfterDelete, gets }));
'''
        )
        environment = os.environ.copy()
        environment.pop("PIKA_EPHEMERAL", None)
        environment["PIKA_NAME"] = "wanted"
        result = subprocess.run(
            [shutil.which("node") or "node", "run.mjs"],
            cwd=home,
            env=environment,
            capture_output=True,
            text=True,
            timeout=10,
            check=True,
        )
        observed = json.loads(result.stdout)
        self.assertEqual(observed["getsAfterDelete"], 0)
        self.assertEqual(observed["payloads"][0]["hook_event_name"], "SessionEnd")
        self.assertTrue(observed["payloads"][0]["deleted"])
        self.assertEqual(observed["payloads"][1]["hook_event_name"], "SessionStart")
        self.assertEqual(observed["payloads"][1]["session_title"], "default")
        self.assertEqual(observed["payloads"][1]["desired_name"], "wanted")
        self.assertIn("rename denied", observed["payloads"][1]["native_name_error"])


if __name__ == "__main__":
    unittest.main()
