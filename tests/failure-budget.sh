#!/usr/bin/env bash
# Shared identical-input failure budget for runtime/capability wrappers.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DOGFOOD_ROOT=${DOGFOOD_ROOT:-$ROOT/dogfood}
LIB=${WRAPPER_LIB:-$DOGFOOD_ROOT/lib/automation-wrapper}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"

grep -q 'operational_failure_maybe_park' "$DOGFOOD_ROOT/drivers/runtime-run" \
  || fail "runtime-run does not park identical operational failures"
grep -q 'operational_failure_maybe_park' "$DOGFOOD_ROOT/drivers/capability-run" \
  || fail "capability-run does not park identical operational failures"
grep -q 'Current local OMP release pin (informational)' "$DOGFOOD_ROOT/drivers/pr-run" \
  || fail "pr-run still mandates Required target generation"
grep -q 'P2 is not automatically fixable' "$DOGFOOD_ROOT/drivers/pr-run" \
  || fail "pr-run is missing in-scope P2 policy"
grep -q 'target: $target' "$DOGFOOD_ROOT/drivers/pr-observe" \
  && fail "pr-observe still hashes release-watch generation"

# shellcheck source=/dev/null
source "$LIB"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
STATE_DIR="$TMP/state"
mkdir -p "$STATE_DIR"

ops() {
  mkdir -p "$STATE_DIR"
  local kind="" fp="" outcome=""
  local i=0 args=("$@")
  while (( i < ${#args[@]} )); do
    case ${args[$i]} in
      --input-fingerprint) fp=${args[$((i+1))]} ;;
      --outcome) outcome=${args[$((i+1))]} ;;
      blocker) kind=blocker ;;
      process) kind=process ;;
    esac
    i=$((i+1))
  done
  case $kind in
    process)
      jq -nc --arg fp "$fp" --arg out "$outcome" '{input_fingerprint:$fp,outcome:$out}' >"$STATE_DIR/processed.json"
      echo '{"ok":true}'
      ;;
    blocker)
      echo '{"ok":true,"data":{"changed":true}}'
      ;;
    *)
      echo '{"ok":true}'
      ;;
  esac
}

FAILURE_BUDGET=2
operational_failure_maybe_park fp-a crash 2 && fail "first failure parked"
jq -e '.failures==1 and .input_fingerprint=="fp-a"' "$STATE_DIR/failure-budget.json" >/dev/null \
  || fail "first failure did not record budget 1"
[[ -e $STATE_DIR/processed.json ]] && fail "first failure marked processed"

operational_failure_maybe_park fp-a crash 2 || fail "second identical failure did not park"
jq -e '.outcome=="blocked" and .input_fingerprint=="fp-a"' "$STATE_DIR/processed.json" >/dev/null \
  || fail "parked input was not processed blocked"
jq -e '.failures==2 and .input_fingerprint=="fp-a"' "$STATE_DIR/failure-budget.json" >/dev/null \
  || fail "budget sidecar was not 2 on fp-a"

n=$(record_attempt_failure fp-b crash)
[[ $n -eq 1 ]] || fail "new fingerprint did not reset budget, got $n"

clear_failure_budget
[[ ! -e $STATE_DIR/failure-budget.json ]] || fail "clear_failure_budget left sidecar"

echo "failure-budget ok"
