#!/usr/bin/env bash
# Critical-cleared sidecar: P0/P1 pending, rereview clear, no repeat.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${REVIEW_LIB:-/home/sf/workspace/oh-my-pi/.systemd-ops/lib/pr-review-state}
SETTLE=${SETTLE_LIB:-/home/sf/workspace/oh-my-pi/.systemd-ops/lib/pr-settlement}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"
# shellcheck source=/dev/null
source "$LIB"
[[ -r $SETTLE ]] || fail "missing $SETTLE"
# shellcheck source=/dev/null
source "$SETTLE"

HEAD_A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
HEAD_B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
HEAD_C=cccccccccccccccccccccccccccccccccccccccc

codex_thread() {
  local tid=$1 sev=$2 commit=$3
  jq -n --arg id "$tid" --arg sev "$sev" --arg commit "$commit" '{
    id: $id,
    isResolved: false,
    isOutdated: false,
    comments: {nodes: [{
      id: ($id + "-c"),
      author: {login: "chatgpt-codex-connector"},
      commit: {oid: $commit},
      body: ("**<sub><sub>![P" + $sev + " Badge](https://img.shields.io/badge/P" + $sev + "-orange?style=flat)</sub></sub>  finding")
    }]}
  }'
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
state=$tmp/state
mkdir -p "$state"

p1=$(jq -s '.' <(codex_thread PRRT_p1 1 "$HEAD_A"))
sev=$(extract_codex_severity "$p1" "$HEAD_A")
record_codex_critical_state "$state" "$sev"
[[ -f $state/critical-pending.json ]] || fail "P1 did not write pending"
[[ ! -f $state/critical-cleared.json ]] || fail "P1 wrote cleared early"

# same-head rereview still P1: stay pending, no clear
record_codex_critical_state "$state" "$sev"
[[ -f $state/critical-pending.json ]] || fail "repeat P1 dropped pending"
[[ ! -f $state/critical-cleared.json ]] || fail "same-head P1 cleared"

# head B still P1: still pending, no clear
p1b=$(jq -s '.' <(codex_thread PRRT_p1b 1 "$HEAD_B"))
sev_b=$(extract_codex_severity "$p1b" "$HEAD_B")
record_codex_critical_state "$state" "$sev_b"
[[ -f $state/critical-pending.json ]] || fail "new-head P1 dropped pending"
[[ ! -f $state/critical-cleared.json ]] || fail "new-head P1 cleared"

# head C: P2+P3 only
p2=$(codex_thread PRRT_p2 2 "$HEAD_C")
p3=$(codex_thread PRRT_p3 3 "$HEAD_C")
cleared_threads=$(jq -s '.' <(printf '%s' "$p2") <(printf '%s' "$p3"))
sev_c=$(extract_codex_severity "$cleared_threads" "$HEAD_C")
jq -e '.p0==[] and .p1==[] and (.p2|length)==1 and (.p3|length)==1' <<<"$sev_c" >/dev/null \
  || fail "cleared rereview was not P2/P3-only: $sev_c"
record_codex_critical_state "$state" "$sev_c"
[[ ! -f $state/critical-pending.json ]] || fail "cleared rereview left pending"
[[ -f $state/critical-cleared.json ]] || fail "cleared rereview did not write critical-cleared"
rev1=$(jq -er '.revision' "$state/critical-cleared.json")
[[ $rev1 =~ ^[0-9a-f]{64}$ ]] || fail "cleared revision was not a digest: $rev1"
jq -e --arg head "$HEAD_C" '.head==$head and .from_head=="'"$HEAD_B"'"' "$state/critical-cleared.json" >/dev/null \
  || fail "cleared sidecar heads: $(cat "$state/critical-cleared.json")"

# repeated poll of the same clear: no new revision
record_codex_critical_state "$state" "$sev_c"
rev2=$(jq -er '.revision' "$state/critical-cleared.json")
[[ $rev1 == "$rev2" ]] || fail "repeat poll changed critical revision"
[[ ! -f $state/critical-pending.json ]] || fail "repeat poll recreated pending"

# parent capability is eligible on the same generation once the PR is idle.
scope=$tmp/scope
pr_unit=managed-omp-pr-proof
cap_unit=managed-omp-cap-proof
mkdir -p "$scope/.systemd-ops/operations/$pr_unit/state"
cp "$state/critical-cleared.json" "$scope/.systemd-ops/operations/$pr_unit/state/critical-cleared.json"
export SYSTEMD_OPS_OPERATION=$cap_unit
ctx=$(jq -n --arg unit "$pr_unit" '{
  data: { relations: { children: [{
    unit: $unit, agent: "pr-maintainer", lifecycle: "active",
    running: false, active_iteration: false
  }, {
    unit: "managed-omp-pr-unrelated", agent: "pr-maintainer", lifecycle: "active",
    running: false, active_iteration: false
  }] } }
}')
capability_has_fresh_critical_clear "$ctx" "$scope" generation-G1 generation-G1 \
  || fail "idle PR with critical-cleared was not a fast path"
capability_has_fresh_critical_clear "$ctx" "$scope" generation-G1 generation-G2 \
  && fail "critical fast path fired across generations"

ctx_running=$(jq -n --arg unit "$pr_unit" '{
  data: { relations: { children: [{
    unit: $unit, agent: "pr-maintainer", lifecycle: "active",
    running: true, active_iteration: true
  }] } }
}')
pr_children_running "$ctx_running" || fail "running PR was not detected"

d1=$(capability_critical_digest "$ctx" "$scope")
unrelated=$(jq -n '{
  data: { relations: { children: [{
    unit: "managed-omp-pr-unrelated", agent: "pr-maintainer", lifecycle: "active"
  }] } }
}')
d2=$(capability_critical_digest "$unrelated" "$scope")
[[ $d1 != "$d2" ]] || fail "unrelated capability saw the same critical digest"

mark_critical_consumed "$ctx" "$scope"
capability_has_fresh_critical_clear "$ctx" "$scope" generation-G1 generation-G1 \
  && fail "consumed critical-cleared still looked fresh"

record_codex_critical_state "$state" "$sev_c"
rev3=$(jq -er '.revision' "$state/critical-cleared.json")
[[ $rev1 == "$rev3" ]] || fail "post-consume poll changed revision"

echo "pr-critical ok"
