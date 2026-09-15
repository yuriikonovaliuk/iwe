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

echo "== Hardened: no privileged invocation in the committed unit files outside the sanctioned shape =="
# Test-reviewer finding 1: the old AC4 checks pattern-matched three
# fixed invocation strings, so any unenumerated 4th privileged-call
# form would slip through. Unit files have simple, unambiguous syntax
# (no quoting/heredoc games), so they can be scanned statically for
# the privileged-command tokens themselves: any occurrence that is not
# the one already-audited, contract-sanctioned fallback ExecStart
# shape is a failure, regardless of exactly what form it takes.
PRIV_RE='(^|[^A-Za-z0-9_])(sudo|pkexec|su|doas)([^A-Za-z0-9_]|$)'
priv_violations=0
for f in "${UNIT_FILES[@]}"; do
    rel="${f#"$REPO_ROOT"/}"
    while IFS= read -r line; do
        [[ "$line" =~ $PRIV_RE ]] || continue
        # systemd unit-file comments (# or ; at the start, per
        # systemd.syntax(7)) are not invocations -- e.g. a comment
        # documenting the NOPASSWD sudo rule the fallback ExecStart
        # relies on.
        trimmed="${line#"${line%%[![:space:]]*}"}"
        [[ "$trimmed" == \#* || "$trimmed" == ";"* ]] && continue
        allowed=0
        for i in "${!STORE_PORTS[@]}"; do
            port="${STORE_PORTS[$i]}"; root="${STORE_ROOTS[$i]}"
            fb="^ExecStart=(/usr/bin/)?sudo -n -u iwe-store ${EXEC_BIN} --transport http --host 127\.0\.0\.1 --port ${port} --store ${root}\$"
            if [[ "$line" =~ $fb ]]; then allowed=1; break; fi
        done
        if [[ "$allowed" -eq 0 ]]; then
            fail "hardened: $rel -- privileged invocation outside the sanctioned fallback ExecStart shape: $line"
            priv_violations=$((priv_violations + 1))
        fi
    done < "$f"
done
if [[ "$priv_violations" -eq 0 ]]; then
    pass "hardened: no unit file contains a privileged (sudo/pkexec/su/doas) invocation outside the sanctioned fallback ExecStart shape"
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
    # NOPASSWD-permitted. sudo/pkexec/su/doas are all mocked (not just
    # sudo) so that whichever escalation binary a call actually reaches
    # -- direct, or via some wrapper -- is intercepted the same way:
    # this is a black-box, shape-agnostic net rather than a match
    # against enumerated invocation strings (finding 1). The one
    # acknowledged limit of PATH-shadowing: a call that hardcodes an
    # absolute path (e.g. /usr/bin/sudo) instead of relying on PATH
    # resolution would bypass it; that limit is inherent to this
    # technique and predates this fix.
    SANDBOX="$(mktemp -d)"
    trap 'rm -rf "$SANDBOX"' EXIT
    MOCKBIN="$SANDBOX/bin"
    mkdir -p "$MOCKBIN"
    CALL_LOG="$SANDBOX/calls.log"   # default target if IWEC_TEST_CALL_LOG is unset
    : > "$CALL_LOG"
    for cmd in sudo pkexec su doas loginctl systemctl; do
        cat > "$MOCKBIN/$cmd" <<EOF
#!/usr/bin/env bash
echo "$cmd \$*" >> "\${IWEC_TEST_CALL_LOG:-$CALL_LOG}"
exit 0
EOF
        chmod +x "$MOCKBIN/$cmd"
    done
    # `install` is special: the script's only non-privileged,
    # sanctioned use of it is copying the committed unit files into
    # the caller's own (here: scratch) systemd/user dir, which must
    # actually happen for real so run-1-vs-run-2 file state is
    # comparable (finding 2). The one dangerous target -- the
    # root-owned /usr/local/lib/iwe-store/bin/iwec path, which must
    # never be written by this script directly -- is still logged and
    # short-circuited rather than executed; every other invocation is
    # passed through to the real `install` binary.
    REAL_INSTALL="$(command -v install)"
    cat > "$MOCKBIN/install" <<EOF
#!/usr/bin/env bash
echo "install \$*" >> "\${IWEC_TEST_CALL_LOG:-$CALL_LOG}"
for a in "\$@"; do
    if [[ "\$a" == "/usr/local/lib/iwe-store/bin/iwec" ]]; then
        exit 0
    fi
done
exec "$REAL_INSTALL" "\$@"
EOF
    chmod +x "$MOCKBIN/install"

    SCRATCH_HOME="$SANDBOX/home"
    mkdir -p "$SCRATCH_HOME"

    run_once() {
        local n="$1" log="$2"
        PATH="$MOCKBIN:$PATH" HOME="$SCRATCH_HOME" \
            XDG_CONFIG_HOME="$SCRATCH_HOME/.config" \
            IWEC_TEST_CALL_LOG="$log" \
            bash "$INSTALL_SCRIPT" >"$SANDBOX/out.$n" 2>&1
        echo $?
    }

    # Real, comparable state after each run (finding 2) -- not just
    # exit code: the recursive content of the directory the script
    # installs unit files into (name + mode + sha256 per file), which
    # catches a duplicate unit, changed content, or a changed mode;
    # and the sequence of systemctl invocations the script actually
    # made. Mocked systemctl carries no real system state to inspect
    # (systemd itself is never touched in this sandbox, by design --
    # the units are meant to be enabled by the user, per AC4), so the
    # invocation-argument sequence is the closest available, honest
    # proxy for "systemctl --user output" here.
    snapshot_units() {
        local dir="$SCRATCH_HOME/.config/systemd/user"
        [[ -d "$dir" ]] || return 0
        while IFS= read -r -d '' f; do
            printf '%s %s %s\n' "$(stat -c '%a' "$f")" "$(sha256sum "$f" | cut -d' ' -f1)" "${f#"$dir"/}"
        done < <(find "$dir" -type f -print0 | sort -z)
    }

    CALL_LOG_1="$SANDBOX/calls.1.log"; : > "$CALL_LOG_1"
    CALL_LOG_2="$SANDBOX/calls.2.log"; : > "$CALL_LOG_2"

    rc1="$(run_once 1 "$CALL_LOG_1")"
    snapshot_units > "$SANDBOX/units.1"
    grep -E '^systemctl ' "$CALL_LOG_1" > "$SANDBOX/systemctl.1"

    rc2="$(run_once 2 "$CALL_LOG_2")"
    snapshot_units > "$SANDBOX/units.2"
    grep -E '^systemctl ' "$CALL_LOG_2" > "$SANDBOX/systemctl.2"

    CALL_LOG_BOTH="$SANDBOX/calls.both.log"
    cat "$CALL_LOG_1" "$CALL_LOG_2" > "$CALL_LOG_BOTH"

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

    if [[ -s "$SANDBOX/units.1" ]]; then
        pass "AC4: first run installed unit file(s) into the scratch systemd/user dir (state snapshot non-empty, so the drift comparison below is meaningful)"
    else
        fail "AC4: first run installed no unit files into the scratch systemd/user dir -- the idempotency drift check below would be vacuous"
    fi

    if diff -u "$SANDBOX/units.1" "$SANDBOX/units.2" >"$SANDBOX/units.diff" 2>&1; then
        pass "AC4: installed unit files identical (name, mode, content hash) after run 1 and run 2 -- no duplicate, no drift"
    else
        fail "AC4: installed unit file state differs between run 1 and run 2 (duplicate unit, changed content, or changed mode) -- $(cat "$SANDBOX/units.diff")"
    fi

    if diff -u "$SANDBOX/systemctl.1" "$SANDBOX/systemctl.2" >"$SANDBOX/systemctl.diff" 2>&1; then
        pass "AC4: systemctl --user invocation sequence identical between run 1 and run 2 -- no drift in what the script asks systemctl to do"
    else
        fail "AC4: systemctl --user invocation sequence differs between run 1 and run 2 -- $(cat "$SANDBOX/systemctl.diff")"
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

    # C4e (finding 3): the old check was `A || (B && C)` where the
    # second disjunct only required "iwe-store" and "enable"/"start"
    # to appear *anywhere at all* in the whole output -- trivially
    # true here regardless, since both words already occur in the
    # unrelated install-path and linger lines. Narrowed to require the
    # real thing: on the primary (no-fallback) path, "iwe-store",
    # "user manager" and "enable"/"start" together on one line; on the
    # documented fallback path (where the contract's own fallback
    # clause allows the step to be revised), a concrete printed
    # systemctl --user enable/start instruction naming the iwec units,
    # standing in for the revised step.
    c4e_ok=0
    if [[ "$FALLBACK_USED" -eq 1 ]]; then
        if grep -qiE 'systemctl[[:space:]]+--user[[:space:]]+(enable|start).*iwec' "$OUT"; then
            c4e_ok=1
        fi
    else
        while IFS= read -r line; do
            if grep -qi 'iwe-store' <<<"$line" && grep -qi 'user manager' <<<"$line" && grep -qiE 'enable|start' <<<"$line"; then
                c4e_ok=1
                break
            fi
        done < "$OUT"
    fi
    if [[ "$c4e_ok" -eq 1 ]]; then
        pass "AC4 (C4e): mentions enabling/starting the units, tied to the manager applicable to the shape in use"
    else
        fail "AC4 (C4e): no genuine mention of enabling/starting the units in the applicable manager (iwe-store's, or the documented fallback's revised systemctl --user step)"
    fi

    # "never attempts": none of the three specific privileged steps
    # may show up as a real invocation, whether bare or prefixed by
    # sudo/pkexec/su/doas -- these patterns are unanchored so a prefix
    # of any shape in front of the operation text is still caught.
    check_never_invoked() {
        local desc="$1" pattern="$2"
        if grep -qE -- "$pattern" "$CALL_LOG_BOTH"; then
            fail "AC4: script actually invoked $desc instead of only printing it -- $(grep -E -- "$pattern" "$CALL_LOG_BOTH")"
        else
            pass "AC4: $desc was not actually invoked (checked bare and via sudo/pkexec/su/doas)"
        fi
    }
    check_never_invoked "'loginctl enable-linger iwe-store'" 'loginctl[[:space:]]+enable-linger[[:space:]]+iwe-store'
    check_never_invoked "the root-owned install of iwec" 'install[[:space:]].*-m[[:space:]]+0755.*/usr/local/lib/iwe-store/bin/iwec'
    check_never_invoked "systemctl --user enable/start for iwec units" 'systemctl[[:space:]]+--user[[:space:]]+(enable|start).*iwec'

    # Finding 1's dynamic half: rather than checking absence of three
    # enumerated invocation strings, assert the escalation binaries
    # themselves were never called at all, in any argument shape --
    # this is what actually closes the gap, since the old checks never
    # even looked at what a real `sudo <step>` invocation would have
    # logged (it logs under "sudo ...", not under the bare command).
    for esc in sudo pkexec su doas; do
        if grep -qE "^${esc}([[:space:]]|\$)" "$CALL_LOG_BOTH"; then
            fail "AC4: script actually invoked '$esc' -- $(grep -E "^${esc}([[:space:]]|\$)" "$CALL_LOG_BOTH")"
        else
            pass "AC4: '$esc' was never actually invoked, in any argument shape"
        fi
    done

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
