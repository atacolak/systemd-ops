#!/usr/bin/env bash
# PR observer wake object: generation and CI success must not change the hash.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${REVIEW_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/pr-review-state}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"
# shellcheck source=/dev/null
source "$LIB"

HEAD=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

pr_with_checks() {
  jq -n --arg head "$HEAD" --argjson checks "$1" '{
    number: 9969,
    state: "OPEN",
    headRefName: "fix/nested-lsp-roots",
    headRefOid: $head,
    reviewDecision: "CHANGES_REQUESTED",
    mergeable: "MERGEABLE",
    checks: $checks
  }'
}

codex_thread() {
  local tid=$1 sev=$2 resolved=${3:-false} outdated=${4:-false}
  jq -n --arg id "$tid" --arg sev "$sev" --arg head "$HEAD" --argjson resolved "$resolved" --argjson outdated "$outdated" '{
    id: $id,
    isResolved: $resolved,
    isOutdated: $outdated,
    comments: {nodes: [{
      id: ($id + "-c1"),
      author: {login: "chatgpt-codex-connector"},
      commit: {oid: $head},
      body: ("![P" + $sev + " Badge](https://img.shields.io/badge/P" + $sev + "-orange?style=flat) finding")
    }]}
  }'
}

human_thread() {
  jq -n --arg head "$HEAD" '{
    id: "PRRT_human",
    isResolved: false,
    isOutdated: false,
    comments: {nodes: [{
      id: "human-c1",
      author: {login: "reviewer"},
      commit: {oid: $head},
      body: "please consider this"
    }]}
  }'
}

queued=$(jq -n '[{name:"native",context:null,conclusion:null},{name:"unit",context:null,conclusion:"SUCCESS"}]')
success=$(jq -n '[{name:"native",context:null,conclusion:"SUCCESS"},{name:"unit",context:null,conclusion:"SUCCESS"},{name:"skipped",context:null,conclusion:"SKIPPED"}]')
failed=$(jq -n '[{name:"native",context:null,conclusion:"FAILURE"},{name:"unit",context:null,conclusion:"SUCCESS"}]')

pr_q=$(pr_with_checks "$queued")
pr_s=$(pr_with_checks "$success")
pr_f=$(pr_with_checks "$failed")

threads_empty='[]'
wake_q=$(pr_wake_object "$pr_q" "$threads_empty")
wake_s=$(pr_wake_object "$pr_s" "$threads_empty")
[[ $(jq -cS '.' <<<"$wake_q") == "$(jq -cS '.' <<<"$wake_s")" ]] || fail "queued/success/skipped checks changed wake object: $wake_q vs $wake_s"
jq -e '.failingChecks==[]' <<<"$wake_q" >/dev/null || fail "non-failures leaked into failingChecks: $wake_q"

wake_f=$(pr_wake_object "$pr_f" "$threads_empty")
[[ $(jq -cS '.' <<<"$wake_f") != "$(jq -cS '.' <<<"$wake_s")" ]] || fail "terminal FAILURE did not change wake object"
jq -e '.failingChecks==[{name:"native",context:null,conclusion:"FAILURE"}]' <<<"$wake_f" >/dev/null \
  || fail "FAILURE was not the sole failing check: $wake_f"

jq -e 'has("target") == false and has("reviews") == false and has("comments") == false' <<<"$wake_s" >/dev/null \
  || fail "wake object still includes generation or review metadata: $wake_s"

t_p3=$(codex_thread PRRT_p3 3)
t_p2=$(codex_thread PRRT_p2 2)
t_p3_old=$(codex_thread PRRT_p3old 0 true false)
wake_p3=$(pr_wake_object "$pr_s" "$(jq -s '.' <(printf '%s' "$t_p3") <(printf '%s' "$t_p3_old"))")
jq -e '.actionableReview==[]' <<<"$wake_p3" >/dev/null || fail "P3-only Codex or resolved P0 woke the PR: $wake_p3"

wake_p2=$(pr_wake_object "$pr_s" "$(jq -s '.' <(printf '%s' "$t_p2"))")
jq -e '.actionableReview | length==1 and .[0].sev=="P2"' <<<"$wake_p2" >/dev/null \
  || fail "in-scope P2 did not remain actionable: $wake_p2"

wake_h=$(pr_wake_object "$pr_s" "$(jq -s '.' <(printf '%s' "$(human_thread)"))")
jq -e '.actionableReview | length==1 and .[0].author=="reviewer"' <<<"$wake_h" >/dev/null \
  || fail "human unresolved thread was dropped: $wake_h"

echo "pr-observe-wake ok"
