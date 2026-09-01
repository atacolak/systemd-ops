#!/usr/bin/env bash
# Runtime pre-gate: six READY capabilities do not compose; seventh does once.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${WRAPPER_LIB:-/home/sf/workspace/oh-my-pi/.systemd-ops/lib/automation-wrapper}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"
# shellcheck source=/dev/null
source "$LIB"

cap() {
  local n=$1 ready=$2 running=${3:-false}
  local gen=generation-G2
  local out=rev-$n
  if [[ $ready != true ]]; then
    gen=generation-G1
  fi
  jq -n --arg unit "managed-omp-cap-$n" --argjson running "$running" --arg gen "$gen" --arg out "$out" '{
    unit: $unit,
    agent: "capability-maintainer",
    lifecycle: "active",
    running: $running,
    active_iteration: $running,
    semantic_state: (if $running then "running" else "ready" end),
    blocker: null,
    checkpoint: {present: true, kind: "structured", generation: $gen, output_revision: $out, input_fingerprint: ("fp-" + $unit)}
  }'
}

ctx_from() {
  jq -n --argjson children "$(jq -s '.' "$@")" '{data:{relations:{children:$children}}}'
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for i in 1 2 3 4 5 6; do
  cap "$i" true >"$tmp/$i.json"
done
cap 7 false >"$tmp/7.json"
six=$(ctx_from "$tmp"/1.json "$tmp"/2.json "$tmp"/3.json "$tmp"/4.json "$tmp"/5.json "$tmp"/6.json "$tmp"/7.json)
capability_sources_ready_for_generation "$six" generation-G2 && fail "six READY + one stale still composed"

cap 7 true >"$tmp/7.json"
seven=$(ctx_from "$tmp"/1.json "$tmp"/2.json "$tmp"/3.json "$tmp"/4.json "$tmp"/5.json "$tmp"/6.json "$tmp"/7.json)
capability_sources_ready_for_generation "$seven" generation-G2 || fail "seven READY capabilities were not composition-eligible"

cap 7 true true >"$tmp/7.json"
running=$(ctx_from "$tmp"/1.json "$tmp"/2.json "$tmp"/3.json "$tmp"/4.json "$tmp"/5.json "$tmp"/6.json "$tmp"/7.json)
capability_sources_ready_for_generation "$running" generation-G2 && fail "running seventh capability was treated ready"

echo "runtime-wave ok"
