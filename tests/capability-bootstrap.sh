#!/usr/bin/env bash
# First-capability bootstrap proofs (spec D1a): absent fork/<branch>
# remote ref, desired-inputs authority, READY checkpoint admitted
# set, integrator admitted-coverage gate. Fixture repos only.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${WRAPPER_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/automation-wrapper}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export PROOF_SKIP_GH=1

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"

# shellcheck source=/dev/null
source "$LIB"

# --- write_output_checkpoint names the admitted set ---
SCOPE_ROOT=$TMP/scope
STATE_DIR=$SCOPE_ROOT/state
mkdir -p "$STATE_DIR"
captured=$TMP/captured-args
ops() { printf '%s\n' "$*" >"$captured"; }

commit_id=76643a6871e1a17c4a199564df95e73490b1060a
delta_id=a94bf2596c9533e313250b9e79680da3de25187e28ee79e7bf43d88a7571020d
cat >"$STATE_DIR/desired-inputs.json" <<EOF
{"version":1,"inputs":[
  {"originating_work":"omp-vuz.1","artifact_kind":"commit","artifact_revision":"$commit_id","artifact_identity":"$commit_id","admitted_at":"2026-09-22T06:30:18Z"},
  {"originating_work":"w2","artifact_kind":"worktree-delta","base":"$commit_id","delta":[{"path":"p","sha256":"$delta_id"}],"artifact_identity":"$delta_id","admitted_at":"2026-09-22T06:30:18Z"}
]}
EOF
write_output_checkpoint fp-a gen-a out-a
grep -q -- "--admitted $commit_id" "$captured"   || fail "commit identity missing from checkpoint args"
grep -q -- "--admitted $delta_id" "$captured"   || fail "delta identity missing from checkpoint args"

rm "$STATE_DIR/desired-inputs.json"
: >"$captured"
write_output_checkpoint fp-a gen-a out-a
grep -q -- "--admitted" "$captured"   && fail "admitted flag emitted without desired-inputs"

# --- bootstrap: absent fork ref, desired-inputs is authority ---
REPO=$TMP/repo
mkdir -p "$REPO"
git -C "$REPO" init -q
git -C "$REPO" config user.email proof@example.invalid
git -C "$REPO" config user.name proof
git -C "$REPO" checkout -q -b main
printf 'base\n' >"$REPO/file"
git -C "$REPO" add file
git -C "$REPO" commit -qm base
baseline=$(git -C "$REPO" rev-parse HEAD)
# admitted artifact: a commit that exists locally but is not on main
git -C "$REPO" checkout -q -b feat/probe
printf 'artifact\n' >"$REPO/artifact"
git -C "$REPO" add artifact
git -C "$REPO" commit -qm artifact
admitted=$(git -C "$REPO" rev-parse HEAD)
# pre-admission local tip (livu line): diverges from the baseline
git -C "$REPO" checkout -q -b cap/probe main
printf 'livu\n' >"$REPO/livu"
git -C "$REPO" add livu
git -C "$REPO" commit -qm livu-tip
# fork remote without the capability branch
git init -q --bare "$TMP/fork.git"
git -C "$REPO" remote add fork "$TMP/fork.git"
git -C "$REPO" push -q fork main

WORKTREE=$REPO
STATE_DIR=$TMP/boot-state
mkdir -p "$STATE_DIR"
cat >"$STATE_DIR/desired-inputs.json" <<EOF
{"version":1,"inputs":[{"originating_work":"omp-vuz.1","artifact_kind":"commit","artifact_revision":"$admitted","artifact_identity":"$admitted","admitted_at":"2026-09-22T06:30:18Z"}]}
EOF
UPSTREAM_GENERATION=$baseline
OPERATION_STEM=managed-omp-cap-probe
provision_native_addon() { return 0; }
provision_node_modules() { return 0; }

ensure_clean_worktree cap/probe || fail "first bootstrap run failed"
[[ $(git -C "$REPO" rev-parse HEAD) == "$baseline" ]] || fail "branch was not reset to the baseline"
git -C "$REPO" for-each-ref --format='%(refname)' refs/heads/archive/ | grep -q 'cap/probe-pre-bootstrap-' || fail "pre-bootstrap tip was not archived"
[[ $(git -C "$REPO" symbolic-ref --short HEAD) == cap/probe ]] || fail "worktree left the cap/probe branch"
[[ -r $STATE_DIR/bootstrap.json ]] || fail "bootstrap marker was not written"

# pre-consume rerun: the admitted revision is still not an ancestor
# and no checkpoint exists, yet in-progress reconciliation on top of
# the baseline must survive (spec D1a step 3, bootstrap.json marker)
printf 'wip\n' >"$REPO/wip"
git -C "$REPO" add wip
git -C "$REPO" commit -qm wip-toward-consume
wip_head=$(git -C "$REPO" rev-parse HEAD)
ensure_clean_worktree cap/probe || fail "pre-consume rerun failed"
[[ $(git -C "$REPO" rev-parse HEAD) == "$wip_head" ]] || fail "pre-consume rerun reset in-progress work"

# a checkpoint that does not name the admitted identity is not
# bootstrap evidence and must not preserve a pre-admission tip
git -C "$REPO" checkout -q -b cap/probe2 main
printf 'livu2\n' >"$REPO/livu2"
git -C "$REPO" add livu2
git -C "$REPO" commit -qm livu-tip-2
STATE2=$TMP/boot-state2
mkdir -p "$STATE2"
cp "$STATE_DIR/desired-inputs.json" "$STATE2/desired-inputs.json"
printf '{"version":2,"input_fingerprint":"fp-old","checkpointed_at":"2026-09-22T00:00:00Z"}\n' >"$STATE2/checkpoint.json"
saved_state=$STATE_DIR
STATE_DIR=$STATE2
ensure_clean_worktree cap/probe2 || fail "fingerprint-only checkpoint run failed"
STATE_DIR=$saved_state
[[ $(git -C "$REPO" rev-parse cap/probe2) == "$baseline" ]] || fail "fingerprint-only checkpoint preserved the pre-admission tip"
git -C "$REPO" for-each-ref --format='%(refname)' refs/heads/archive/ | grep -q 'cap/probe2-pre-bootstrap-' || fail "cap/probe2 tip was not archived"
[[ -r $STATE2/bootstrap.json ]] || fail "bootstrap marker was not written for cap/probe2"
git -C "$REPO" checkout -q cap/probe

# idempotent rerun: once an admitted revision is an ancestor of
# HEAD, the branch is never reset again
git -C "$REPO" merge -q --no-edit "$admitted" >/dev/null
ensure_clean_worktree cap/probe || fail "idempotent rerun failed"
[[ $(git -C "$REPO" rev-parse HEAD) != "$baseline" ]] || fail "rerun reset the branch again"

# no desired-inputs: no bootstrap authority, fetch failure preserved
git -C "$REPO" checkout -q -b cap/orphan main
rm "$STATE_DIR/desired-inputs.json"
set +e
ensure_clean_worktree cap/orphan
code=$?
set -e
[[ $code -ne 0 && $code -ne 4 ]] || fail "no-authority case exited $code, want the fetch error"
[[ $(git -C "$REPO" rev-parse cap/orphan) == "$baseline" ]] || fail "no-authority case mutated the branch"

# unresolvable admitted artifact: typed blocker, never git-128
git -C "$REPO" checkout -q -b cap/gone main
missing=1111111111111111111111111111111111111111
cat >"$STATE_DIR/desired-inputs.json" <<EOF
{"version":1,"inputs":[{"originating_work":"w9","artifact_kind":"commit","artifact_revision":"$missing","artifact_identity":"$missing","admitted_at":"2026-09-22T06:30:18Z"}]}
EOF
blocker_kind=$TMP/blocker-kind
ops() {
  if [[ ${1:-} == automation && ${2:-} == blocker ]]; then
    shift 2
    while (($#)); do
      [[ $1 == --kind ]] && printf '%s\n' "$2" >"$blocker_kind"
      shift
    done
    echo '{"ok":true,"data":{"changed":true}}'
    return 0
  fi
  echo '{"ok":true}'
}
set +e
ensure_clean_worktree cap/gone
code=$?
set -e
[[ $code -eq 4 ]] || fail "unresolvable artifact exited $code, want 4"
[[ $(cat "$blocker_kind" 2>/dev/null) == artifact-unresolvable ]] || fail "blocker kind '$(cat "$blocker_kind" 2>/dev/null)', want artifact-unresolvable"

# --- capability-observe tolerates an absent fork ref ---
OBSERVE=${DOGFOOD_ROOT:-$ROOT/dogfood}/drivers/capability-observe
mkdir -p "$SCOPE_ROOT/.systemd-ops/operations/managed-omp-release-watch/state"
printf '{"target_commit":"%s"}\n' "$baseline" >"$SCOPE_ROOT/.systemd-ops/operations/managed-omp-release-watch/state/generation.json"
git -C "$REPO" checkout -q cap/probe
SYSTEMD_OPS_CWD=$REPO SYSTEMD_OPS_SCOPE_ROOT=$SCOPE_ROOT "$OBSERVE" cap/probe >"$TMP/obs.json" || fail "capability-observe failed with an absent fork ref"
jq -e '.version == 1 and (.world_fingerprint | length == 64)' "$TMP/obs.json" >/dev/null || fail "capability-observe did not emit a world fingerprint"
# the absent ref hashes as an empty remote_head: recompute the world
# fingerprint with remote_head="" and require byte equality
obs_dirty=$(git -C "$REPO" status --porcelain=v1 --untracked-files=normal | sha256sum | cut -d' ' -f1)
obs_head=$(git -C "$REPO" rev-parse HEAD)
obs_want=$(jq -cn --arg branch cap/probe --arg target "$baseline" --arg local_head "$obs_head" --arg remote_head "" --arg dirty "$obs_dirty" '{branch:$branch,target:$target,local_head:$local_head,remote_head:$remote_head,dirty:$dirty}' | sha256sum | cut -d' ' -f1)
jq -e --arg want "$obs_want" '.world_fingerprint == $want' "$TMP/obs.json" >/dev/null || fail "absent fork ref did not hash as an empty remote_head"
git -C "$REPO" push -q fork cap/probe
git -C "$REPO" fetch -q fork cap/probe
SYSTEMD_OPS_CWD=$REPO SYSTEMD_OPS_SCOPE_ROOT=$SCOPE_ROOT "$OBSERVE" cap/probe >/dev/null || fail "capability-observe failed with a present fork ref"

# --- integrator admitted-coverage gate ---
GEN=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
OUT=cccccccccccccccccccccccccccccccccccccccc
CAPUNIT=managed-omp-cap-probe
mkdir -p "$SCOPE_ROOT/.systemd-ops/operations/$CAPUNIT/state"
cat >"$SCOPE_ROOT/.systemd-ops/operations/$CAPUNIT/state/desired-inputs.json" <<EOF
{"version":1,"inputs":[{"originating_work":"omp-vuz.1","artifact_kind":"commit","artifact_revision":"$admitted","artifact_identity":"$admitted","admitted_at":"2026-09-22T06:30:18Z"}]}
EOF
ctx_for() {
  jq -n --arg gen "$1" --arg out "$2" --argjson admitted "$3" '{
    data: {relations: {children: [{
      unit: "managed-omp-cap-probe",
      lifecycle: "active",
      agent: "capability-maintainer",
      running: false,
      active_iteration: false,
      blocker: null,
      checkpoint: {present: true, kind: "structured", generation: $gen, output_revision: $out, admitted: $admitted}
    }]}}
  }'
}
ctx=$(ctx_for "$GEN" "$OUT" "[\"$admitted\"]")
capability_sources_ready_for_generation "$ctx" "$GEN"   || fail "covered admitted set was not ready"
ctx=$(ctx_for "$GEN" "$OUT" '[]')
capability_sources_ready_for_generation "$ctx" "$GEN"   && fail "stale checkpoint without admitted coverage was ready"
rm "$SCOPE_ROOT/.systemd-ops/operations/$CAPUNIT/state/desired-inputs.json"
capability_sources_ready_for_generation "$ctx" "$GEN"   || fail "child without desired-inputs was held back"

echo "capability-bootstrap ok"
