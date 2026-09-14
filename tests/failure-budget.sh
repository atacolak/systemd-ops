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
grep -q 'input_matches_operational_park' "$DOGFOOD_ROOT/drivers/runtime-run" \
  || fail "runtime-run does not skip an unchanged parked input"
grep -q 'input_matches_operational_park' "$DOGFOOD_ROOT/drivers/capability-run" \
  || fail "capability-run does not skip an unchanged parked input"
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
OPS_LOG="$TMP/ops-argv.log"
mkdir -p "$STATE_DIR"

ops() {
  mkdir -p "$STATE_DIR"
  printf '%s\n' "$*" >>"$OPS_LOG"
  local kind="" fp="" outcome="" blocker_kind="" summary="" route="" code="" context=0
  local i=0 args=("$@")
  while (( i < ${#args[@]} )); do
    case ${args[$i]} in
      --input-fingerprint) fp=${args[$((i+1))]} ;;
      --outcome) outcome=${args[$((i+1))]} ;;
      --kind) blocker_kind=${args[$((i+1))]} ;;
      --summary) summary=${args[$((i+1))]} ;;
      --route) route=${args[$((i+1))]} ;;
      --code) code=${args[$((i+1))]} ;;
      blocker) kind=blocker ;;
      process) kind=process ;;
      context) context=1 ;;
      inspect) kind=inspect ;;
      author) kind=author ;;
    esac
    i=$((i+1))
  done
  case $kind in
    process)
      jq -nc --arg fp "$fp" --arg out "$outcome" '{input_fingerprint:$fp,outcome:$out}' >"$STATE_DIR/processed.json"
      echo '{"ok":true}'
      ;;
    blocker)
      jq -nc --arg fp "$fp" --arg k "$blocker_kind" --arg s "$summary" \
        --arg r "$route" --arg c "$code" \
        '{input_fingerprint:$fp,kind:$k,summary:$s,route:$r,code:$c}' \
        >>"$STATE_DIR/blockers.jsonl"
      echo '{"ok":true,"data":{"changed":true}}'
      ;;
    inspect)
      echo '{"ok":true,"data":{"editable_spec":{"cwd":"cwd-unset","exec":{"argv":["cwd-unset"]}}}}'
      ;;
    author)
      if [[ "$*" == *plan-update* ]]; then
        echo '{"ok":true,"data":{"plan_token":"plan-token-proof"}}'
      else
        echo '{"ok":true}'
      fi
      ;;
    *)
      if (( context )); then
        # The context mirrors durable processed state, so input_is_processed
        # answers from what automation process actually wrote.
        local processed=""
        if [[ -r $STATE_DIR/processed.json ]]; then
          processed=$(jq -r '.input_fingerprint // empty' "$STATE_DIR/processed.json")
        fi
        jq -nc --arg fp "$processed" \
          '{ok:true,data:{automation:{processed:{input_fingerprint:$fp}}}}'
      else
        echo '{"ok":true}'
      fi
      ;;
  esac
}

FAILURE_BUDGET=2
operational_failure_maybe_park fp-a crash 2 && fail "first failure parked"
jq -e '.failures==1 and .input_fingerprint=="fp-a"' "$STATE_DIR/failure-budget.json" >/dev/null \
  || fail "first failure did not record budget 1"
[[ -e $STATE_DIR/processed.json ]] && fail "first failure marked processed"

operational_failure_maybe_park fp-a crash 2 || fail "second identical failure did not park"
# An operational park (crash, timeout, contract-failure) must not certify the
# input as processed: that is what made every later tick skip the input and left
# the unit unable to retry. The blocker and the failure budget are the record.
[[ ! -e $STATE_DIR/processed.json ]] || fail "operational park marked the input processed"
jq -e '.failures==2 and .input_fingerprint=="fp-a"' "$STATE_DIR/failure-budget.json" >/dev/null \
  || fail "budget sidecar was not 2 on fp-a"
# The park records a real kind. crash, timeout and contract-failure are
# iteration failures, not semantic outcomes, so a parked input must not claim
# semantic-blocked. The summary wording is the park's own.
jq -e 'select(.input_fingerprint=="fp-a" and .kind=="iteration-failed"
      and (.summary|contains("parked after 2 identical crash failures")))' \
  "$STATE_DIR/blockers.jsonl" >/dev/null \
  || fail "operational park did not record the blocker as iteration-failed"
jq -e 'select(.input_fingerprint=="fp-a" and .kind=="iteration-failed"
      and (.summary|contains("crash with exit 2")))' \
  "$STATE_DIR/blockers.jsonl" >/dev/null \
  || fail "operational park did not record the failure blocker"
jq -e 'select(.kind=="semantic-blocked")' "$STATE_DIR/blockers.jsonl" >/dev/null \
  && fail "operational park claimed a semantic block"
# The retry is real: the same fingerprint still reads as unprocessed next tick.
input_is_processed fp-a && fail "operationally parked input still reads as processed"

# The retry is bounded by a sidecar next to the failure budget: it holds the
# fingerprint the last operational park covered. While the observation still
# matches it a tick has nothing to retry. This is not processed state and
# nothing on the processed path reads it.
jq -e '.input_fingerprint=="fp-a"' "$STATE_DIR/operational-park.json" >/dev/null \
  || fail "operational park did not record the parked fingerprint in the sidecar"
input_matches_operational_park fp-a || fail "parked input did not match the park sidecar"
input_matches_operational_park fp-later && fail "a different input matched the park sidecar"

# Contrast, and a guard against a vacuous negative above: a semantic blocked
# input is still processed, so the driver keeps skipping it. The end-to-end
# version of this contrast is the blocked-repeat case in wrapper-contract.sh.
mark_processed fp-sem blocked
input_is_processed fp-sem || fail "semantic blocked input did not read as processed"

n=$(record_attempt_failure fp-b crash)
[[ $n -eq 1 ]] || fail "new fingerprint did not reset budget, got $n"

clear_failure_budget
[[ ! -e $STATE_DIR/failure-budget.json ]] || fail "clear_failure_budget left sidecar"
# A recovered input must not stay shadowed by a stale park record.
[[ ! -e $STATE_DIR/operational-park.json ]] || fail "clear_failure_budget left the park sidecar"

# A genuine semantic blocked outcome is not an operational park: it stays
# processed and records semantic-blocked, and it writes no park sidecar.
mark_processed fp-sem2 blocked
input_is_processed fp-sem2 || fail "semantic blocked input did not read as processed"
record_blocker semantic-blocked 0 "the local capability cannot be made correct" "" self fp-sem2
jq -e 'select(.input_fingerprint=="fp-sem2" and .kind=="semantic-blocked")' \
  "$STATE_DIR/blockers.jsonl" >/dev/null \
  || fail "semantic blocked outcome did not record semantic-blocked"
[[ ! -e $STATE_DIR/operational-park.json ]] || fail "semantic blocked outcome wrote the park sidecar"
input_matches_operational_park fp-sem2 && fail "semantic blocked input matched a park sidecar"

# An unreadable park record is reported by the wrapper, not leaked as jq's own
# error text, and it is never a skip: with no readable park there is nothing to
# honour, so the input stays live and the tick runs normally.
printf 'not json\n' >"$STATE_DIR/operational-park.json"
park_err=$(operational_park_fingerprint 2>&1 >/dev/null || true)
[[ $park_err != *"jq:"* ]] || fail "corrupt park sidecar leaked jq's error: $park_err"
[[ $park_err == *"operational park record"* ]] \
  || fail "corrupt park sidecar was not reported by the wrapper: $park_err"
input_matches_operational_park fp-a && fail "a corrupt park sidecar skipped the input"
rm -f "$STATE_DIR/operational-park.json"
absent_err=$(operational_park_fingerprint 2>&1 >/dev/null || true)
[[ -z $absent_err ]] || fail "an absent park sidecar was reported: $absent_err"

# A recovery that repairs a dirty worktree clears the park sidecar alongside the
# blocker it already clears. Otherwise a later tick whose observation is
# unchanged still matches the parked fingerprint and skips with no work, with
# the blocker that justified the park already gone.
rec=$TMP/recovery
mkdir -p "$rec/worktree" "$rec/fork.git"
git -C "$rec/fork.git" init -q --bare
git -C "$rec/worktree" init -q
git -C "$rec/worktree" config user.email proof@example.invalid
git -C "$rec/worktree" config user.name proof
git -C "$rec/worktree" checkout -q -b cap/recover
touch "$rec/worktree/keep.txt"
git -C "$rec/worktree" add keep.txt
git -C "$rec/worktree" commit -qm base
git -C "$rec/worktree" remote add fork "$rec/fork.git"
git -C "$rec/worktree" push -q fork cap/recover
git -C "$rec/worktree" fetch -q fork
: >"$OPS_LOG"
WORKTREE=$rec/worktree
GIT_BASE=$rec/worktree
SCOPE_ROOT=$rec
OPERATION_STEM=managed-omp-cap-recover
record_operational_park fp-rec crash
input_matches_operational_park fp-rec || fail "recovery fixture did not record a park"
recover_worktree cap/recover worktree-dirty "dedicated worktree is dirty: $WORKTREE" \
  || fail "recover_worktree failed"
[[ ! -e $STATE_DIR/operational-park.json ]] || fail "recover_worktree left the park sidecar"
input_matches_operational_park fp-rec && fail "a recovered input still matched the park sidecar"
grep -q 'automation clear-blocker' "$OPS_LOG" || fail "recover_worktree did not clear the blocker"

echo "failure-budget ok"
