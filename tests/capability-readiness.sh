#!/usr/bin/env bash
# Deterministic capability child-readiness proofs against fixture context.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${WRAPPER_LIB:-/home/sf/workspace/oh-my-pi/.systemd-ops/automation-wrapper-lib}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"

# shellcheck source=/dev/null
source "$LIB"

context_for() {
  local a_gen=$1 a_out=$2 b_gen=$3 b_out=$4
  local a_blocker=${5:-null}
  jq -n --arg a_gen "$a_gen" --arg a_out "$a_out" --arg b_gen "$b_gen" --arg b_out "$b_out" --argjson a_blocker "$a_blocker" '{
    data: {
      relations: {
        children: [
          {
            unit: "managed-omp-pr-a",
            lifecycle: "active",
            running: false,
            active_iteration: false,
            blocker: $a_blocker,
            checkpoint: {present: true, kind: "structured", generation: $a_gen, output_revision: $a_out, fingerprint: "fp-a"},
            child_revision: ("rev-a-" + $a_out)
          },
          {
            unit: "managed-omp-pr-b",
            lifecycle: "active",
            running: false,
            active_iteration: false,
            blocker: null,
            checkpoint: {present: true, kind: "structured", generation: $b_gen, output_revision: $b_out, fingerprint: "fp-b"},
            child_revision: ("rev-b-" + $b_out)
          }
        ]
      }
    }
  }'
}

ctx=$(context_for generation-B out-a generation-A out-b)
children_ready_for_generation "$ctx" generation-B && fail "mismatch generations were treated ready"
children_have_blocker "$ctx" && fail "no blocker should be present"

ctx=$(context_for generation-B out-a generation-B out-b)
children_ready_for_generation "$ctx" generation-B || fail "matching generations were not ready"

ctx=$(context_for generation-B out-a2 generation-B out-b)
children_ready_for_generation "$ctx" generation-B || fail "review-only sibling change was not ready"

ctx=$(context_for generation-B out-a generation-B out-b '{"id":"blk-1","kind":"iteration-failed"}')
children_ready_for_generation "$ctx" generation-B && fail "blocked child was treated ready"
children_have_blocker "$ctx" || fail "blocked child was not detected"

empty='{"data":{"relations":{"children":[]}}}'
children_ready_for_generation "$empty" generation-B || fail "zero-child capability was not ready"
children_have_blocker "$empty" && fail "zero-child capability reported a blocker"

completed=$(jq -n '{
  data: {
    relations: {
      children: [
        {
          unit: "managed-omp-pr-merged",
          lifecycle: "completed",
          running: false,
          active_iteration: false,
          blocker: null,
          checkpoint: {present: true, kind: "structured", generation: "generation-A", output_revision: "old", fingerprint: "fp-old"}
        },
        {
          unit: "managed-omp-pr-live",
          lifecycle: "active",
          running: false,
          active_iteration: false,
          blocker: null,
          checkpoint: {present: true, kind: "structured", generation: "generation-B", output_revision: "live", fingerprint: "fp-live"}
        }
      ]
    }
  }
}')
children_ready_for_generation "$completed" generation-B || fail "completed sibling still blocked generation readiness"

rev1=$(child_revision_digest "$(context_for generation-B out-a generation-B out-b)")
rev2=$(child_revision_digest "$(context_for generation-B out-a2 generation-B out-b)")
[[ "$rev1" != "$rev2" ]] || fail "child revision ignored output change"

echo "capability-readiness ok"
