#!/usr/bin/env bash
# Test suite for milestone iwec-shared-daemon, task T4 ("systemd --user
# daemon units per store + idempotent install script + user-applied-step
# surface"), built from the task's acceptance criteria alone -- before,
# and independently of, Developer's implementation.
#
# Convention assumed (per the task's Shared surface: "systemd/ unit
# definitions, scripts/ install script"): committed unit file(s) live
# under systemd/ at the repo root; the install/enable script lives
# somewhere under scripts/ and is identified by content (it must mention
# `loginctl enable-linger`, one of the three user-applied steps), not by
# a specific filename, since the Shared surface does not pin one.
#
# Checks (mapped 1:1 to acceptance criteria; see REPORT at the bottom of
# a run for the mapping):
#   AC1  ExecStart matches the required binary/flags pattern, per store,
#        with no `sudo` in the ExecStart line.
#   AC2  Both absolute store roots appear verbatim in the committed unit
#        artifacts.
#   AC3  MemoryMax=1500M and Restart=on-failure present alongside each
#        matched ExecStart.
#   AC4  The install script runs twice without error, performs no
#        privileged install/loginctl invocation for the three
#        user-applied steps, and prints (does not execute) them
#        verbatim.
#   AC5  `systemd-analyze verify` passes on every committed unit file.
#   (Fallback-path criterion is a documentation criterion -- not
#   mechanically checkable; see NOTE at the end.)
#
# Usage: scripts/tests/t4_systemd_units_test.sh
# Exit status: 0 iff every checkable criterion passes.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SYSTEMD_DIR="$REPO_ROOT/systemd"
SCRIPTS_DIR="$REPO_ROOT/scripts"

FAILURES=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }
skip() { echo "SKIP: $1"; }

# Expected per-store values, taken verbatim from the acceptance criteria.
STORE_NAMES=(iwe-memory mind)
STORE_ROOTS=(/home/yurii/projects/iwe-memory /home/yurii/projects/mind)
STORE_PORTS=(8765 8766)
EXEC_BIN="/usr/local/lib/iwe-store/bin/iwec"

echo "== AC5: systemd-analyze verify on committed unit(s) =="
UNIT_FILES=()
if [[ -d "$SYSTEMD_DIR" ]]; then
    while IFS= read -r -d '' f; do UNIT_FILES+=("$f"); done \
        < <(find "$SYSTEMD_DIR" -type f -name '*.service' -print0 | sort -z)
fi
if [[ ${#UNIT_FILES[@]} -eq 0 ]]; then
    fail "AC5/AC1/AC2/AC3: no *.service files found under systemd/ (Developer artifact not present yet)"
else
    for f in "${UNIT_FILES[@]}"; do
        rel="${f#"$REPO_ROOT"/}"
        if systemd-analyze verify --user "$f" >/tmp/t4_verify_out.$$ 2>&1; then
            pass "AC5: systemd-analyze verify $rel"
        else
            fail "AC5: systemd-analyze verify $rel -- $(cat /tmp/t4_verify_out.$$)"
        fi
        rm -f /tmp/t4_verify_out.$$
    done
fi

echo "== AC1/AC2/AC3: ExecStart pattern, verbatim store roots, resource bounds =="
# The contract sanctions two shapes for ExecStart: the primary one (runs
# directly under iwe-store's own user manager, no sudo) and a documented
# fallback (a yurii-side unit with `sudo -n -u iwe-store` prefixing the
# same binary/flags) for when the primary proves unworkable. Both are
# checked for structurally; which one a given unit uses determines which
# further checks apply (no-sudo for primary, fallback-documented for
# fallback) -- this dual shape comes from the contract's own fallback
# criterion, not from inspecting Developer's code.
FALLBACK_USED=0
if [[ ${#UNIT_FILES[@]} -gt 0 ]]; then
    for i in "${!STORE_NAMES[@]}"; do
        name="${STORE_NAMES[$i]}"
        root="${STORE_ROOTS[$i]}"
        port="${STORE_PORTS[$i]}"
        primary_re="^ExecStart=${EXEC_BIN} --transport http --host 127\.0\.0\.1 --port ${port} --store ${root}\$"
        fallback_re="^ExecStart=(/usr/bin/)?sudo -n -u iwe-store ${EXEC_BIN} --transport http --host 127\.0\.0\.1 --port ${port} --store ${root}\$"

        match_file=""
        shape=""
        for f in "${UNIT_FILES[@]}"; do
            if grep -qE -- "$primary_re" "$f"; then
                match_file="$f"; shape="primary"; break
            elif grep -qE -- "$fallback_re" "$f"; then
                match_file="$f"; shape="fallback"; break
            fi
        done

        if [[ -z "$match_file" ]]; then
            fail "AC1/AC2 ($name): no unit file's ExecStart matches the primary pattern (${EXEC_BIN} ... --port ${port} --store ${root}, no sudo) or the documented fallback (sudo -n -u iwe-store prefix, same args)"
            continue
        fi
        rel="${match_file#"$REPO_ROOT"/}"
        pass "AC1/AC2 ($name): $rel ExecStart matches the $shape shape with verbatim store root $root and port $port"

        if [[ "$shape" == "primary" ]]; then
            pass "AC1 ($name): ExecStart in $rel does not invoke sudo (primary, no-sudo path)"
        else
            FALLBACK_USED=1
            # Fallback path: sudo is expected here, but the contract
            # requires the fallback choice to be documented (evidence +
            # revised user-applied-step list), not just silently used.
            doc_hit=""
            while IFS= read -r -d '' d; do
                if grep -qi 'fallback' "$d" 2>/dev/null \
                    && grep -qi 'iwe-store' "$d" 2>/dev/null \
                    && grep -qiE 'sudo[[:space:]]+-n[[:space:]]+-u[[:space:]]+iwe-store' "$d" 2>/dev/null; then
                    doc_hit="$d"; break
                fi
            done < <(find "$REPO_ROOT" -not -path '*/target/*' -not -path '*/.git/*' \
                        -not -path "$REPO_ROOT/scripts/tests/*" \
                        \( -path '*/docs/*' -o -path "$SYSTEMD_DIR/*" -o -path "$SCRIPTS_DIR/*" \) \
                        -type f -print0)
            if [[ -n "$doc_hit" ]]; then
                pass "AC-fallback ($name): fallback path use is documented (${doc_hit#"$REPO_ROOT"/})"
            else
                fail "AC-fallback ($name): $rel uses the sudo -n -u iwe-store fallback but no doc under docs/, systemd/ or scripts/ documents choosing the fallback (evidence + revised user-applied-step list)"
            fi
        fi

        if grep -qF 'MemoryMax=1500M' "$match_file"; then
            pass "AC3 ($name): MemoryMax=1500M present in $rel"
        else
            fail "AC3 ($name): MemoryMax=1500M missing from $rel"
        fi

        if grep -qF 'Restart=on-failure' "$match_file"; then
            pass "AC3 ($name): Restart=on-failure present in $rel"
        else
            fail "AC3 ($name): Restart=on-failure missing from $rel"
        fi
    done
else
    skip "AC1/AC2/AC3: no unit files to inspect"
fi

echo "== AC4: idempotent install script prints, never executes, the user-applied steps =="
INSTALL_SCRIPT=""
if [[ -d "$SCRIPTS_DIR" ]]; then
    while IFS= read -r -d '' f; do
        if grep -qi 'enable-linger' "$f" 2>/dev/null; then
            INSTALL_SCRIPT="$f"
            break
        fi
    done < <(find "$SCRIPTS_DIR" -type f -name '*.sh' -print0 | sort -z)
fi

if [[ -z "$INSTALL_SCRIPT" ]]; then
    fail "AC4: no install/enable script found under scripts/ mentioning 'enable-linger' (Developer artifact not present yet)"
else
    rel="${INSTALL_SCRIPT#"$REPO_ROOT"/}"
    echo "  candidate install script: $rel"

    # Sandbox: intercept every privileged/system-mutating command the
    # script might invoke so a run against a not-yet-provisioned system
    # cannot do real, unrecoverable damage, while still letting us see
    # exactly what was invoked. Each mock logs its full argv and then
    # succeeds, so the script can proceed past any step it thinks is
    # NOPASSWD-permitted.
    SANDBOX="$(mktemp -d)"
    trap 'rm -rf "$SANDBOX"' EXIT
    MOCKBIN="$SANDBOX/bin"
    mkdir -p "$MOCKBIN"
    CALL_LOG="$SANDBOX/calls.log"
    : > "$CALL_LOG"
    for cmd in sudo loginctl install systemctl; do
        cat > "$MOCKBIN/$cmd" <<EOF
#!/usr/bin/env bash
echo "$cmd \$*" >> "$CALL_LOG"
exit 0
EOF
        chmod +x "$MOCKBIN/$cmd"
    done

    SCRATCH_HOME="$SANDBOX/home"
    mkdir -p "$SCRATCH_HOME"

    run_once() {
        PATH="$MOCKBIN:$PATH" HOME="$SCRATCH_HOME" \
            XDG_CONFIG_HOME="$SCRATCH_HOME/.config" \
            bash "$INSTALL_SCRIPT" >"$SANDBOX/out.$1" 2>&1
        echo $?
    }

    rc1="$(run_once 1)"
    rc2="$(run_once 2)"

    if [[ "$rc1" == "0" ]]; then
        pass "AC4: first run exits 0"
    else
        fail "AC4: first run exited $rc1 -- $(cat "$SANDBOX/out.1")"
    fi
    if [[ "$rc2" == "0" ]]; then
        pass "AC4: second run exits 0 (idempotent)"
    else
        fail "AC4: second run exited $rc2 -- $(cat "$SANDBOX/out.2")"
    fi

    OUT="$SANDBOX/out.1"

    if grep -qF 'sudo loginctl enable-linger iwe-store' "$OUT"; then
        pass "AC4: prints verbatim 'sudo loginctl enable-linger iwe-store'"
    elif [[ "$FALLBACK_USED" -eq 1 ]] && grep -qiE 'linger' "$OUT" && grep -qiE 'unnecessary|not need|no longer need' "$OUT"; then
        pass "AC4: fallback path taken and script surfaces that linger is unnecessary (per contract's fallback clause)"
    else
        fail "AC4: does not print verbatim 'sudo loginctl enable-linger iwe-store', and (if on the fallback path) does not surface that linger is unnecessary"
    fi

    if grep -qF 'sudo install -m 0755 ~/.cargo/bin/iwec /usr/local/lib/iwe-store/bin/iwec' "$OUT"; then
        pass "AC4: prints verbatim 'sudo install -m 0755 ~/.cargo/bin/iwec /usr/local/lib/iwe-store/bin/iwec'"
    else
        fail "AC4: does not print verbatim 'sudo install -m 0755 ~/.cargo/bin/iwec /usr/local/lib/iwe-store/bin/iwec'"
    fi

    if grep -qiE 'iwe-store.*user manager|user manager.*iwe-store' "$OUT" \
        || (grep -qi 'iwe-store' "$OUT" && grep -qiE 'enable|start' "$OUT"); then
        pass "AC4: mentions enabling/starting the units in iwe-store's user manager"
    else
        fail "AC4: no mention of enabling/starting units in iwe-store's user manager"
    fi

    # "never attempts": the three specific privileged steps must not
    # show up as *invocations* in the call log, even though the mocks
    # would have let them succeed harmlessly.
    if grep -qE '^loginctl enable-linger iwe-store$' "$CALL_LOG"; then
        fail "AC4: script actually invoked 'loginctl enable-linger iwe-store' instead of only printing it"
    else
        pass "AC4: 'loginctl enable-linger iwe-store' was not actually invoked"
    fi

    if grep -qE '^install .*-m 0755.*/usr/local/lib/iwe-store/bin/iwec$' "$CALL_LOG"; then
        fail "AC4: script actually invoked the root-owned install of iwec instead of only printing it"
    else
        pass "AC4: root-owned install of iwec was not actually invoked"
    fi

    if grep -qE '^systemctl --user (enable|start).*iwec' "$CALL_LOG"; then
        fail "AC4: script actually invoked systemctl --user enable/start for iwec units instead of only printing"
    else
        pass "AC4: systemctl --user enable/start for iwec units was not actually invoked"
    fi

    trap - EXIT
    rm -rf "$SANDBOX"
fi

echo
echo "== Untestable-as-contracted =="
echo "NOTE: the fallback-path criterion's substantive engineering judgment"
echo "(\"iwe-store's own user manager proves unworkable\") cannot itself be"
echo "mechanically verified -- this suite only checks that, IF a unit's"
echo "ExecStart uses the sudo -n -u iwe-store fallback shape, that choice"
echo "is documented somewhere under docs/, systemd/ or scripts/ (with the"
echo "words fallback / iwe-store / sudo -n -u iwe-store), and that the"
echo "install script's linger guidance is consistent with which shape is"
echo "in use. Whether the underlying judgment to take the fallback was"
echo "actually warranted is not something a test can decide."

echo
if [[ "$FAILURES" -eq 0 ]]; then
    echo "RESULT: all checkable criteria pass ($((${#UNIT_FILES[@]})) unit file(s) checked)."
    exit 0
else
    echo "RESULT: $FAILURES check(s) failed."
    exit 1
fi
