#!/usr/bin/env bash
# AC3: "journal.path unset (default) -> run IWE's FULL existing
# regression/integration suite with the journal feature present-but-
# unconfigured, and assert zero output differences anywhere in that suite
# versus a baseline run without your changes -- this is a stronger bar
# than 'tests still pass,' it's 'behavior is byte-identical.'"
#
# None of the existing test fixtures set `journal.path` (it is a brand
# new config key), so "the journal feature present-but-unconfigured"
# is simply: run the existing suite against HEAD_REF exactly as it is.
# "a baseline run without your changes" is the same suite run against
# BASE_REF (the commit the journal feature was built on top of).
#
# Usage:
#   scripts/journal_baseline_diff.sh [BASE_REF] [HEAD_REF]
#
# Builds two disposable git worktrees (one per ref), runs `cargo test
# --workspace` in each with a single test thread (for deterministic
# ordering), normalizes away known non-deterministic noise (per-run
# timings, temp-directory paths, build-hash-bearing binary names), and
# diffs the result. Exit 0 with no output on a byte-identical (after
# normalization) result; exit 1 with the unified diff otherwise.
#
# Testability: `scripts/journal_baseline_diff.sh --normalize <file>`
# prints `<file>` through the same normalization the real comparison
# applies, and exits 0 -- this lets a fast test exercise the
# normalization rules without paying for two full workspace builds.

set -euo pipefail

normalize() {
  # $1: path to a raw `cargo test` combined stdout+stderr capture.
  sed -E \
    -e 's/finished in [0-9]+\.[0-9]+s/finished in Ns/g' \
    -e 's#(target/(debug|release)/deps/[A-Za-z0-9_]+-)[0-9a-f]{16}#\1HASH#g' \
    -e 's#/tmp/[A-Za-z0-9_./-]*#<TMPPATH>#g' \
    -e 's#Compiling [A-Za-z0-9_-]+ v[0-9][A-Za-z0-9_.+-]*#Compiling <CRATE>#g' \
    "$1"
}

if [ "${1:-}" = "--normalize" ]; then
  normalize "$2"
  exit 0
fi

# Defaults to the local `master` branch rather than a remote-qualified
# ref, so this doesn't depend on a configured remote existing at all
# (see scripts/c1_compliance_check.sh, which this repository's own diff
# must also pass cleanly).
BASE_REF="${1:-master}"
HEAD_REF="${2:-HEAD}"

repo_root() {
  local path
  path="$(pwd)"
  while [ ! -f "$path/Cargo.toml" ] || [ ! -d "$path/crates" ]; do
    path="$(dirname "$path")"
    if [ "$path" = "/" ]; then
      echo "could not find workspace root" >&2
      exit 2
    fi
  done
  echo "$path"
}

ROOT="$(repo_root)"
WORK="$(mktemp -d)"
trap 'git -C "$ROOT" worktree remove --force "$WORK/base" >/dev/null 2>&1 || true;
      git -C "$ROOT" worktree remove --force "$WORK/head" >/dev/null 2>&1 || true;
      rm -rf "$WORK"' EXIT

git -C "$ROOT" worktree add -q --detach "$WORK/base" "$BASE_REF"
git -C "$ROOT" worktree add -q --detach "$WORK/head" "$HEAD_REF"

run_suite() {
  # $1: worktree dir, $2: output file
  (cd "$1" && cargo test --workspace -- --test-threads=1) >"$2" 2>&1 || true
}

run_suite "$WORK/base" "$WORK/base.out"
run_suite "$WORK/head" "$WORK/head.out"

normalize "$WORK/base.out" >"$WORK/base.norm"
normalize "$WORK/head.out" >"$WORK/head.norm"

if ! diff -u "$WORK/base.norm" "$WORK/head.norm"; then
  echo "journal_baseline_diff FAILED: behavior differs between ${BASE_REF} and ${HEAD_REF}" >&2
  exit 1
fi

exit 0
