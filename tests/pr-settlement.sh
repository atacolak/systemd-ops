#!/usr/bin/env bash
# PR settlement for a target generation and 5-minute quiet debounce.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${SETTLEMENT_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/pr-settlement}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"
# shellcheck source=/dev/null
source "$LIB"

G1=generation-G1
G2=generation-G2

child() {
  jq -n \
    --arg unit "$1" \
    --arg agent "$2" \
    --arg life "$3" \
    --argjson running "$4" \
    --argjson active "$5" \
    --arg sem "$6" \
    --arg obs_fp "$7" \
    --arg obs_gen "$8" \
    --arg proc_fp "$9" \
    --arg proc_out "${10}" \
    --arg finished "${11}" \
    '{
      unit: $unit,
      agent: $agent,
      lifecycle: $life,
      running: $running,
      active_iteration: $active,
      semantic_state: $sem,
      observation: {input_fingerprint: $obs_fp, generation: $obs_gen},
      processed: {input_fingerprint: $proc_fp, outcome: $proc_out},
      latest_iteration: {id: ("it-" + $unit), finished_at: $finished},
      checkpoint: {present: true, kind: "structured", generation: $obs_gen, output_revision: "rev"}
    }'
}

ctx() {
  jq -n --argjson children "$(jq -s '.' "$@")" '{data:{relations:{children:$children}}}'
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

unprocessed=$(child pr-a pr-maintainer active false false stale obs-a2 "$G2" obs-a1 ready "2026-09-01T00:00:00Z")
running=$(child pr-b pr-maintainer active true false ready obs-b2 "$G2" obs-b2 ready "2026-09-01T00:00:00Z")
printf '%s\n' "$unprocessed" >"$tmp/a.json"
printf '%s\n' "$running" >"$tmp/b.json"
not_ready=$(ctx "$tmp/a.json" "$tmp/b.json")
pr_children_settled_for_generation "$not_ready" "$G2" && fail "unprocessed/running children were treated settled"

idle_blocked=$(child pr-a pr-maintainer active false false blocked obs-a "$G2" obs-a blocked "2026-09-01T00:01:00Z")
idle_ready=$(child pr-b pr-maintainer active false false ready obs-b "$G2" obs-b ready "2026-09-01T00:01:00Z")
printf '%s\n' "$idle_blocked" >"$tmp/a.json"
printf '%s\n' "$idle_ready" >"$tmp/b.json"
settled=$(ctx "$tmp/a.json" "$tmp/b.json")
pr_children_settled_for_generation "$settled" "$G2" || fail "BLOCKED+READY idle processed children were not settled"
pr_children_settled_for_generation "$settled" "$G1" && fail "G2-settled children counted for G1"

prev_gen=$(child pr-a pr-maintainer active false false ready obs-a "$G1" obs-a ready "2026-09-01T00:00:00Z")
printf '%s\n' "$prev_gen" >"$tmp/a.json"
printf '%s\n' "$idle_ready" >"$tmp/b.json"
old=$(ctx "$tmp/a.json" "$tmp/b.json")
pr_children_settled_for_generation "$old" "$G2" && fail "previous-generation child counted settled for G2"

empty='{"data":{"relations":{"children":[]}}}'
pr_children_settled_for_generation "$empty" "$G2" || fail "zero PR children should be vacuously settled"

rev1=$(pr_children_settled_revision "$settled")
[[ $rev1 =~ ^[0-9a-f]{64}$ ]] || fail "settled revision was not a digest: $rev1"

# quiet debounce: t+0 and t+299 no, t+300 yes. review at +200 resets.
state=$tmp/quiet.json
PR_SETTLE_QUIET_SECONDS=300
pr_quiet_elapsed "$state" "$G2" "$rev1" 1000 && fail "quiet elapsed at t+0"
pr_quiet_elapsed "$state" "$G2" "$rev1" 1299 && fail "quiet elapsed at t+299"
pr_quiet_elapsed "$state" "$G2" "$rev1" 1300 || fail "quiet not elapsed at t+300"

# new observation resets window
rev2=$(pr_children_settled_revision "$(
  printf '%s\n' "$(child pr-a pr-maintainer active false false blocked obs-a-new "$G2" obs-a-new blocked "2026-09-01T00:04:00Z")" >"$tmp/a.json"
  ctx "$tmp/a.json" "$tmp/b.json"
)")
[[ $rev2 != "$rev1" ]] || fail "new observation did not change settled revision"
pr_quiet_elapsed "$state" "$G2" "$rev2" 1500 && fail "reset quiet elapsed immediately"
pr_quiet_elapsed "$state" "$G2" "$rev2" 1799 && fail "reset quiet elapsed at +299"
pr_quiet_elapsed "$state" "$G2" "$rev2" 1800 || fail "reset quiet not elapsed at +300"

# same generation is not a release upgrade: launch even if a child is unprocessed.
same=$(capability_release_gate "$not_ready" "$G2" "$G2" 1000 "$tmp/quiet-same.json") || true
[[ $same == launch ]] || fail "same-generation unprocessed children blocked launch: $same"

# 60m grace: unsettled children become eligible at +3600, not before.
grace_file=$tmp/grace.json
g0=$(capability_release_gate "$not_ready" "$G2" "$G1" 1000 "$tmp/quiet-grace.json" "$grace_file") || true
[[ $g0 == wait-unsettled ]] || fail "t+0 unsettled was $g0"
g3599=$(capability_release_gate "$not_ready" "$G2" "$G1" 4599 "$tmp/quiet-grace.json" "$grace_file") || true
[[ $g3599 == wait-unsettled ]] || fail "t+3599 unsettled was $g3599"
g3600=$(capability_release_gate "$not_ready" "$G2" "$G1" 4600 "$tmp/quiet-grace.json" "$grace_file") || true
[[ $g3600 == launch-grace ]] || fail "t+3600 unsettled was $g3600, want launch-grace"
units=$(pr_unsettled_units "$not_ready" "$G2")
printf '%s\n' "$units" | grep -q pr-a || fail "unsettled units missing pr-a: $units"

# settled path still wins before grace: quiet 300 after settle, no 3600 wait
pref=$(capability_release_gate "$settled" "$G2" "$G1" 1000 "$tmp/quiet-pref.json" "$tmp/grace-pref.json") || true
[[ $pref == wait-quiet ]] || fail "settled t+0 was $pref, want wait-quiet"
pref2=$(capability_release_gate "$settled" "$G2" "$G1" 1300 "$tmp/quiet-pref.json" "$tmp/grace-pref.json") || true
[[ $pref2 == launch ]] || fail "settled t+300 was $pref2, want launch not grace"

echo "pr-settlement ok"
