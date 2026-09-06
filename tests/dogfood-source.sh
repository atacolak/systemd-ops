#!/usr/bin/env bash
# Tracked dogfood is the default source for wrapper proofs.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }

[[ -d $ROOT/dogfood/drivers ]] || fail "missing $ROOT/dogfood/drivers"
[[ -d $ROOT/dogfood/lib ]] || fail "missing $ROOT/dogfood/lib"

for f in pr-run pr-observe capability-run capability-observe runtime-run runtime-observe runtime-reconcile omp-release-watch project-lead-watch fork-main-sync; do
  [[ -f $ROOT/dogfood/drivers/$f ]] || fail "missing driver $f"
done
for f in automation-wrapper pr-attempt pr-settlement pr-review-state; do
  [[ -f $ROOT/dogfood/lib/$f ]] || fail "missing lib $f"
done
[[ ! -e $ROOT/dogfood/lib/verify-merged-pr ]] || fail "imported out-of-scope verify-merged-pr"
[[ ! -e $ROOT/dogfood/lib/verify-merged-pr.test.sh ]] || fail "imported out-of-scope verify-merged-pr.test.sh"

needle="/home/sf/workspace/oh-my-pi/"'.systemd-ops'
if grep -R -n -- "$needle" "$ROOT/tests"/*.sh; then
  fail "tests still default to live OMP copy"
fi
echo "dogfood-source ok"
