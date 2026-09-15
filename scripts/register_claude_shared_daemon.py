#!/usr/bin/env python3
"""Register the local shared IWE and Mind MCP daemons with Claude.

This changes only ``mcpServers.iwe`` and ``mcpServers.mind`` in a Claude
configuration. Run it only after both shared daemons are confirmed live: the
later gated rollout task performs the live ``~/.claude.json`` flip. This
script deliberately does not start daemons.
"""

from __future__ import annotations

import argparse
import json
import os
import stat
import sys
import tempfile
from pathlib import Path
from typing import Any


SERVERS = {
    "iwe": {"type": "http", "url": "http://127.0.0.1:8765/mcp"},
    "mind": {"type": "http", "url": "http://127.0.0.1:8766/mcp"},
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Rewrite only mcpServers.iwe and mcpServers.mind to shared-daemon "
            "HTTP URLs. Run the live flip only after daemon liveness is confirmed."
        )
    )
    parser.add_argument(
        "--config",
        type=Path,
        default=Path("~/.claude.json").expanduser(),
        help="Claude configuration path (default: ~/.claude.json)",
    )
    return parser.parse_args()


def load_config(config_path: Path) -> tuple[dict[str, Any], bytes]:
    try:
        original = config_path.read_bytes()
    except FileNotFoundError:
        raise ValueError(f"Claude config does not exist: {config_path}") from None

    try:
        config = json.loads(original)
    except json.JSONDecodeError as error:
        raise ValueError(f"Claude config is not valid JSON: {error}") from error

    if not isinstance(config, dict):
        raise ValueError("Claude config root must be a JSON object")
    servers = config.get("mcpServers")
    if not isinstance(servers, dict):
        raise ValueError("Claude config must contain an mcpServers object")
    missing = [name for name in SERVERS if name not in servers]
    if missing:
        raise ValueError(f"Claude config mcpServers is missing: {', '.join(missing)}")
    return config, original


def write_atomically(config_path: Path, content: bytes) -> None:
    mode = stat.S_IMODE(config_path.stat().st_mode)
    with tempfile.NamedTemporaryFile(
        dir=config_path.parent, prefix=f".{config_path.name}.", delete=False
    ) as temporary:
        temporary.write(content)
        temporary.flush()
        os.fsync(temporary.fileno())
        temporary_path = Path(temporary.name)
    try:
        os.chmod(temporary_path, mode)
        os.replace(temporary_path, config_path)
    except BaseException:
        temporary_path.unlink(missing_ok=True)
        raise


def register(config_path: Path) -> bool:
    """Apply the two registration entries, returning whether the file changed."""
    config_path = config_path.expanduser()
    config, original = load_config(config_path)
    servers = config["mcpServers"]
    if all(servers[name] == registration for name, registration in SERVERS.items()):
        return False

    backup_path = config_path.with_name(f"{config_path.name}.bak")
    if not backup_path.exists():
        backup_path.write_bytes(original)

    servers.update(SERVERS)
    rendered = (json.dumps(config, indent=2, ensure_ascii=False) + "\n").encode("utf-8")
    write_atomically(config_path, rendered)
    return True


def main() -> int:
    args = parse_args()
    try:
        changed = register(args.config)
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"{'Updated' if changed else 'Already registered'}: {args.config.expanduser()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
