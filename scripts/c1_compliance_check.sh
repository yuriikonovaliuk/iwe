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
# BASE_REF defaults to origin/master, HEAD_REF defaults to HEAD. Every
# line ADDED between BASE_REF and HEAD_REF (this script's own file
# excluded, so its own vocabulary describing the check doesn't self-flag)
# is scanned, case-insensitively, for whole-word occurrences of:
#   layer(s), assembly/assemblies, package(s), origin(s), compositor(s)
#
# Exit status is 0 with no output when the diff is clean, non-zero with
# the offending lines printed (path: line content) otherwise.

set -euo pipefail

BASE_REF="${1:-origin/master}"
HEAD_REF="${2:-HEAD}"
SELF_PATH="scripts/c1_compliance_check.sh"

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
  "${BASE_REF}" "${HEAD_REF}" -- . ":(exclude)${SELF_PATH}")

if [ -n "$violations" ]; then
  echo "C1 compliance check FAILED: forbidden vocabulary found in added lines:" >&2
  printf '%s' "$violations" >&2
  exit 1
fi

exit 0
