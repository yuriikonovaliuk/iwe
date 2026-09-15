#!/usr/bin/env python3
"""Isolated regression test for register_claude_shared_daemon.py."""

from __future__ import annotations

import copy
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("register_claude_shared_daemon.py")


class RegistrationTest(unittest.TestCase):
    def test_updates_only_iwe_and_mind_and_is_idempotent(self) -> None:
        initial = {
            "mcpServers": {
                "iwe": {"command": "/home/example/bin/iwe-memory-mcp", "args": []},
                "mind": {"command": "/home/example/bin/mind-memory-mcp", "args": []},
                "unrelated": {"command": "other-mcp", "args": ["--keep"]},
            },
            "permissions": {"allow": ["Read", "Write"]},
            "unrelatedTopLevel": {"unchanged": True},
        }
        expected = copy.deepcopy(initial)
        expected["mcpServers"]["iwe"] = {
            "type": "http",
            "url": "http://127.0.0.1:8765/mcp",
        }
        expected["mcpServers"]["mind"] = {
            "type": "http",
            "url": "http://127.0.0.1:8766/mcp",
        }

        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "claude.json"
            original_bytes = json.dumps(initial, separators=(",", ":")).encode("utf-8")
            config_path.write_bytes(original_bytes)

            first = subprocess.run(
                [sys.executable, str(SCRIPT), "--config", str(config_path)],
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertIn("Updated", first.stdout)
            self.assertEqual(
                (Path(directory) / "claude.json.bak").read_bytes(), original_bytes
            )
            self.assertEqual(json.loads(config_path.read_text()), expected)

            after_first_run = config_path.read_bytes()
            second = subprocess.run(
                [sys.executable, str(SCRIPT), "--config", str(config_path)],
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertIn("Already registered", second.stdout)
            self.assertEqual(config_path.read_bytes(), after_first_run)


if __name__ == "__main__":
    unittest.main()
