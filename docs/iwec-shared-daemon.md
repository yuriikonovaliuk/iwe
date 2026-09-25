# Shared IWEC systemd user daemons

`systemd/user/iwec-iwe-memory.service` and
`systemd/user/iwec-mind.service` run the HTTP servers for, respectively,
`/home/yurii/projects/iwe-memory` on port 8765 and `/home/yurii/projects/mind`
on port 8766.  Their `--store` arguments make their operation independent of
the service working directory.

## Manager choice

The intended arrangement was services in `iwe-store`'s own `systemd --user`
manager, with an `iwec` `ExecStart` that needs no `sudo`.  Build-time host
inspection found that account's passwd home is `/var/lib/iwe-store`, it is not
logged in or lingering (`loginctl show-user iwe-store` failed with “User ID
980 is not logged in or lingering”), and there is no usable user manager.
The committed units therefore use the fallback: they are installed in
`yurii`'s user manager and execute
`sudo -n -u iwe-store /usr/local/lib/iwe-store/bin/iwec ...`.  The existing
narrow NOPASSWD rule permits exactly the `iwe`, `iwec`, and `kc` binaries at
that root-owned location; no sudoers change is required.

This is an explicit environment-specific fallback, not a second daemon
configuration.  If `iwe-store` later gains a lingered user manager and a
writable user-unit directory, replace these fallback units with non-sudo
units in that manager and revise this document.

## Install and user-applied steps

Run `scripts/install-iwec-user-units.sh` from any directory.  It idempotently
installs both unit definitions into the invoking user's
`$XDG_CONFIG_HOME/systemd/user` (or `~/.config/systemd/user`) and reloads the
manager.  It intentionally does not enable or start either service, so it is
safe before the privileged binary refresh lands.

After cargo installs the version containing `iwec --store`, apply these steps
manually:

```sh
sudo install -m 0755 ~/.cargo/bin/iwec /usr/local/lib/iwe-store/bin/iwec
systemctl --user enable --now iwec-iwe-memory.service iwec-mind.service
```

For the rejected per-`iwe-store`-manager arrangement, the user-applied steps
would instead include `sudo loginctl enable-linger iwe-store` and enabling and
starting those units in that manager.  The installer prints those commands
but never runs them.

## Resource policy

Both services set `MemoryMax=1500M`, roughly 1.7 times the observed
600–900 MB post-write RSS range.  `Restart=on-failure` ensures an OOM-killed
server is replaced by a fresh, small-footprint process.

## Restarts (iwe-plus 1.4.0+)

`iwec --state-dir <PATH>` makes a restart invisible to connected MCP clients
and turns transactions a restart drops into explicit refusals; see
`docs/mcp.md`, "Restarts".  The committed units do not pass it yet: the flag
needs the 1.4.0 binary installed first (an older `iwec` refuses to start on an
unknown flag), and `PATH` must be writable by `iwe-store`, e.g.

```sh
sudo -u iwe-store install -d -m 0700 /var/lib/iwe-store/iwec-state/iwe-memory /var/lib/iwe-store/iwec-state/mind
# then add to each ExecStart:  --state-dir /var/lib/iwe-store/iwec-state/<store>
```

On `systemctl --user stop`/`restart`, systemd sends SIGTERM to `sudo`, which
relays it to `iwec`; iwec drains open transactions for up to
`--drain-timeout-secs` (default 30, below systemd's 90 s stop timeout) before
exiting.
