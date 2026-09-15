#!/bin/sh
# Install the shared-daemon units into the invoking user's manager.  This is
# intentionally safe before the privileged iwec binary refresh: it never
# enables or starts a service.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
unit_source="$repo_root/systemd/user"
unit_target="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

mkdir -p "$HOME/.local/state/iwec"
install -d -m 0755 "$unit_target"
install -m 0644 "$unit_source/iwec-iwe-memory.service" "$unit_target/iwec-iwe-memory.service"
install -m 0644 "$unit_source/iwec-mind.service" "$unit_target/iwec-mind.service"
if systemctl --user daemon-reload; then
  reload_result="reloaded this user's systemd manager"
else
  # Unit files are still completely installed.  A missing user bus is common
  # during provisioning and must not make a pre-binary-refresh install fail.
  reload_result="could not reach this user's systemd manager; reload it later"
fi

printf '%s\n\n' "Installed unit definitions and $reload_result."
cat <<'STEPS'

User-applied steps (intentionally not attempted by this script):
  sudo install -m 0755 ~/.cargo/bin/iwec /usr/local/lib/iwe-store/bin/iwec
  systemctl --user enable --now iwec-iwe-memory.service iwec-mind.service

The original per-iwe-store-manager plan would also require (and this script
never attempts):
  sudo loginctl enable-linger iwe-store
  sudo -u iwe-store systemctl --user enable --now iwec-iwe-memory.service iwec-mind.service

This host uses the documented yurii-manager fallback; see docs/iwec-shared-daemon.md.
STEPS
