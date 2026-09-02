#!/usr/bin/env bash
# Automated reviewer check for the transaction journal feature: the
# feature must read as a generic, independently-motivated IWE feature (a
# transaction journal for audit trails, undo, backup tooling, or external
# sync), so its own diff must contain no vocabulary that only makes sense
# in the context of some other, external system layering itself on top of
# IWE.
#
# Usage:
#   scripts/c1_compliance_check.sh [BASE_REF] [HEAD_REF]
#
# BASE_REF defaults to the local `master` branch (not `origin/master`:
# that string itself contains the whole word "origin" as a git remote
# name, an unrelated, unavoidable collision with this check's own
# vocabulary -- pass an explicit ref if a different baseline is needed).
# HEAD_REF defaults to HEAD.
#
# Every line ADDED between BASE_REF and HEAD_REF is scanned, case-
# insensitively, for whole-word occurrences of:
#   layer(s), assembly/assemblies, package(s), origin(s), compositor(s)
#
# Two paths are excluded from the scan: this script's own file (so its
# vocabulary *describing* the check doesn't self-flag), and
# `crates/iwe/tests/c1_compliance_test.rs` (this check's own test suite,
# which necessarily writes the literal forbidden words into disposable
# fixture repositories to prove detection actually works -- it is meta-
# tooling for the check itself, not part of the shipped feature the check
# exists to police).
#
# Exit status is 0 with no output when the diff is clean, non-zero with
# the offending lines printed (path: line content) otherwise.

set -euo pipefail

BASE_REF="${1:-master}"
HEAD_REF="${2:-HEAD}"
SELF_PATH="scripts/c1_compliance_check.sh"
SELF_TEST_PATH="crates/iwe/tests/c1_compliance_test.rs"

# GNU/glibc extended-regex word boundaries (bash's [[ =~ ]] uses the C
# library's ERE engine, which supports \b as an extension).
PATTERN='\b(layers?|assembl(y|ies)|packages?|origins?|compositors?)\b'

violations=""
current_file="?"
shopt -s nocasematch

while IFS= read -r diff_line; do
  case "$diff_line" in
    "+++ "*)
      current_file="${diff_line#+++ }"
      ;;
    "+"*)
      added="${diff_line#+}"
      if [[ "$added" =~ $PATTERN ]]; then
        violations+="${current_file}: ${added}"$'\n'
      fi
      ;;
  esac
done < <(git diff --no-color --unified=0 \
  "${BASE_REF}" "${HEAD_REF}" -- . ":(exclude)${SELF_PATH}" ":(exclude)${SELF_TEST_PATH}")

if [ -n "$violations" ]; then
  echo "C1 compliance check FAILED: forbidden vocabulary found in added lines:" >&2
  printf '%s' "$violations" >&2
  exit 1
fi

exit 0
