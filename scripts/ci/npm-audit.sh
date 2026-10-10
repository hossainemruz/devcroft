#!/usr/bin/env bash
# Audit the shipped npm lockfiles without installing or fixing.
# Blocking threshold: high/critical. Lower severities are reported for review.
# Any exception must be narrowly recorded in this wrapper, never by suppressing
# all audit failures.
#
# Usage: ./scripts/ci/npm-audit.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIRS=(
  "$ROOT/assets/language-servers/typescript"
  "$ROOT/assets/language-servers/python"
)

fail=0
for dir in "${DIRS[@]}"; do
  echo "==> npm audit: $dir"
  if [[ ! -f "$dir/package-lock.json" ]]; then
    echo "missing package-lock.json in $dir" >&2
    fail=1
    continue
  fi
  # --package-lock-only avoids installing; --ignore-scripts avoids lifecycle hooks.
  if ! npm --prefix "$dir" audit --package-lock-only --omit=dev --audit-level=high --ignore-scripts; then
    echo "npm audit found high/critical findings in $dir" >&2
    fail=1
  fi
done

exit "$fail"
