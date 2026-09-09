#!/usr/bin/env bash
# Codex severity extractor and P3-ignore policy. No systemd mutation.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${REVIEW_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/pr-review-state}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"

# shellcheck source=/dev/null
source "$LIB"

HEAD_A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
HEAD_B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
HEAD_C=cccccccccccccccccccccccccccccccccccccccc

codex_comment() {
  local id=$1 severity=$2 commit=$3
  jq -n --arg id "$id" --arg sev "$severity" --arg commit "$commit" '{
    id: $id,
    author: {login: "chatgpt-codex-connector"},
    commit: {oid: $commit},
    body: ("**<sub><sub>![P" + $sev + " Badge](https://img.shields.io/badge/P" + $sev + "-orange?style=flat)</sub></sub>  finding " + $id)
  }'
}

thread() {
  local tid=$1 resolved=$2 outdated=$3
  shift 3
  jq -n --arg id "$tid" --argjson resolved "$resolved" --argjson outdated "$outdated" --argjson comments "$(jq -s '.' "$@")" '{
    id: $id,
    isResolved: $resolved,
    isOutdated: $outdated,
    comments: {nodes: $comments}
  }'
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
c_p0=$(codex_comment thread-p0 0 "$HEAD_A")
c_p1=$(codex_comment thread-p1 1 "$HEAD_A")
c_p2=$(codex_comment thread-p2 2 "$HEAD_A")
c_p3=$(codex_comment thread-p3 3 "$HEAD_A")
printf '%s\n' "$c_p0" >"$tmp/p0.json"
printf '%s\n' "$c_p1" >"$tmp/p1.json"
printf '%s\n' "$c_p2" >"$tmp/p2.json"
printf '%s\n' "$c_p3" >"$tmp/p3.json"

t_p0=$(thread PRRT_p0 false false "$tmp/p0.json")
t_p1=$(thread PRRT_p1 false false "$tmp/p1.json")
t_p2=$(thread PRRT_p2 false false "$tmp/p2.json")
t_p3=$(thread PRRT_p3 false false "$tmp/p3.json")

threads_all=$(jq -s '.' <(printf '%s' "$t_p0") <(printf '%s' "$t_p1") <(printf '%s' "$t_p2") <(printf '%s' "$t_p3"))
state=$(extract_codex_severity "$threads_all" "$HEAD_A")
jq -e --arg head "$HEAD_A" '
  .head==$head
  and (.p0==["PRRT_p0"])
  and (.p1==["PRRT_p1"])
  and (.p2==["PRRT_p2"])
  and (.p3==["PRRT_p3"])
' <<<"$state" >/dev/null || fail "mixed P0-P3 extraction: $state"

policy=$(codex_repair_policy "$state")
[[ $policy == repair-p0 ]] || fail "P0 policy was $policy, want repair-p0"

state_p1=$(extract_codex_severity "$(jq -s '.' <(printf '%s' "$t_p1"))" "$HEAD_A")
[[ $(codex_repair_policy "$state_p1") == repair-p1 ]] || fail "P1 was not MUST-fix"

state_p2=$(extract_codex_severity "$(jq -s '.' <(printf '%s' "$t_p2"))" "$HEAD_A")
[[ $(codex_repair_policy "$state_p2") == consider-p2 ]] || fail "P2 was not consider-p2"

state_p3=$(extract_codex_severity "$(jq -s '.' <(printf '%s' "$t_p3"))" "$HEAD_A")
jq -e '.p0==[] and .p1==[] and .p2==[] and .p3==["PRRT_p3"]' <<<"$state_p3" >/dev/null \
  || fail "P3-only extraction: $state_p3"
[[ $(codex_repair_policy "$state_p3") == ignore-p3 ]] || fail "P3-only was not ignore"

# outdated / resolved findings are not current-head
t_old=$(thread PRRT_old false true "$tmp/p0.json")
t_done=$(thread PRRT_done true false "$tmp/p1.json")
state_current=$(extract_codex_severity "$(jq -s '.' <(printf '%s' "$t_old") <(printf '%s' "$t_done") <(printf '%s' "$t_p3"))" "$HEAD_A")
jq -e '.p0==[] and .p1==[] and .p3==["PRRT_p3"]' <<<"$state_current" >/dev/null \
  || fail "outdated/resolved leaked into current severity: $state_current"

# finding on a different commit is not current-head
c_other=$(codex_comment thread-other 0 "$HEAD_B")
printf '%s\n' "$c_other" >"$tmp/other.json"
t_other=$(thread PRRT_other false false "$tmp/other.json")
state_head=$(extract_codex_severity "$(jq -s '.' <(printf '%s' "$t_other") <(printf '%s' "$t_p3"))" "$HEAD_A")
jq -e '.p0==[] and .p3==["PRRT_p3"]' <<<"$state_head" >/dev/null \
  || fail "other-head P0 counted on HEAD_A: $state_head"

echo "pr-severity ok"
