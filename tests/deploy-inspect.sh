#!/usr/bin/env bash
# Inspect names deploy phase/stall; attention surfaces canary and admission.
# Throwaway scope; no live units; does not mutate live current/omp.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
TMP=$(mktemp -d)
SCOPE=$TMP/scope
STEM=managed-omp-runtime-integrator
CAP=proof-cap
CAP_STEM=managed-omp-cap-$CAP
HOME_DIR=$SCOPE/.systemd-ops/operations/$STEM
CAP_HOME=$SCOPE/.systemd-ops/operations/$CAP_STEM
STATE_DIR=$HOME_DIR/state
CAP_STATE=$CAP_HOME/state
DEPLOY=$STATE_DIR/deployment.json
DESIRED=$CAP_STATE/desired-inputs.json
UNIT_DIR=$TMP/xdg/systemd/user
UNIT=$UNIT_DIR/$STEM.service
CAP_UNIT=$UNIT_DIR/$CAP_STEM.service
AGENT=$TMP/agents
CAP_WT=$TMP/cap-worktree
TOPO=$ROOT/docs/superpowers/topology-omp-invariant.txt
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { jq -e '.ok == true' <<<"$1" >/dev/null || fail "expected ok: $1"; }

command -v jq >/dev/null || fail "jq required"
command -v git >/dev/null || fail "git required"
command -v sha256sum >/dev/null || fail "sha256sum required"
[[ -x $BIN ]] || fail "missing $BIN"

LIVE_GENS=/home/sf/worktrees/omp/generations
LIVE_OMP=/home/sf/.bun/bin/omp
live_gens_before=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_before=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)

PROMOTED=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
CANDIDATE=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
PROMOTED_COMPOSE=1111111111111111111111111111111111111111
CANDIDATE_COMPOSE=2222222222222222222222222222222222222222
FOREIGN=ffffffffffffffffffffffffffffffffffffffff

mkdir -p "$HOME_DIR" "$CAP_HOME" "$UNIT_DIR" "$SCOPE/.systemd-ops" \
  "$AGENT/.omp/agents" "$TMP/bin" "$STATE_DIR" "$CAP_STATE" "$CAP_WT"
printf '%s\n' 'runtime-integrator role' >"$AGENT/.omp/agents/runtime-integrator.md"
printf '%s\n' 'capability-maintainer role' >"$AGENT/.omp/agents/capability-maintainer.md"

cat >"$SCOPE/.systemd-ops/scope.toml" <<EOF
[scope]
id = "proof"
owned = ["managed-*"]

[automation]
agent_root = "$AGENT"
EOF

cat >"$HOME_DIR/automation.toml" <<'EOF'
version = 1
agent = "runtime-integrator"
EOF

cat >"$CAP_HOME/automation.toml" <<'EOF'
version = 1
agent = "capability-maintainer"
parent = "managed-omp-runtime-integrator"
EOF

cat >"$UNIT" <<EOF
# Managed by systemd-ops
[Unit]
Description=runtime integrator
[Service]
Type=oneshot
ExecStart=/bin/true
WorkingDirectory=$SCOPE
EOF

cat >"$CAP_UNIT" <<EOF
# Managed by systemd-ops
[Unit]
Description=$CAP
[Service]
Type=oneshot
ExecStart=/bin/true
WorkingDirectory=$CAP_WT
EOF

git -C "$CAP_WT" init -q
git -C "$CAP_WT" config user.email proof@example.test
git -C "$CAP_WT" config user.name proof
printf 'seed\n' >"$CAP_WT/README"
git -C "$CAP_WT" add README
git -C "$CAP_WT" commit -qm seed
HEAD_REV=$(git -C "$CAP_WT" rev-parse HEAD)

cat >"$TMP/bin/systemctl" <<EOF
#!/bin/sh
while [ "\${1:-}" = --user ]; do shift; done
case " \$* " in
  *" list-unit-files "*)
    printf '%s\n' '[{"unit_file":"$STEM.service","state":"static","preset":"enabled"},{"unit_file":"$CAP_STEM.service","state":"static","preset":"enabled"}]'
    ;;
  *" list-units "*)
    if [ -f "$TMP/integrator-active" ]; then
      printf '%s\n' '[{"unit":"$STEM.service","load":"loaded","active":"active","sub":"running"}]'
    else
      printf '%s\n' '[]'
    fi
    ;;
  *" list-timers "*)
    printf '%s\n' '[]'
    ;;
  *" show "*)
    if [ -f "$TMP/integrator-active" ]; then
      printf 'Id=$STEM.service\nLoadState=loaded\nActiveState=active\nSubState=running\nFragmentPath=%s\n' "$UNIT"
    else
      printf 'LoadState=loaded\nActiveState=inactive\nSubState=dead\nFragmentPath=%s\n' "$UNIT"
    fi
    ;;
  *)
    printf '%s\n' '[]'
    ;;
esac
EOF
chmod 755 "$TMP/bin/systemctl"

export PATH=$TMP/bin:$PATH
export XDG_CONFIG_HOME=$TMP/xdg
export SYSTEMD_OPS_STATE_DIR=$TMP/state
export SYSTEMD_OPS_SCOPE_ROOT=$SCOPE

json() {
  "$BIN" --json --manager user --write-prefix 'managed-*' --scope-root "$SCOPE" "$@"
}

write_deployment() {
  local phase=$1
  local canary_result=$2
  local stall_reason=$3
  local include_candidate=$4
  jq -n \
    --arg generations_root "$TMP/generations" \
    --arg prev_gen "$PROMOTED_COMPOSE" \
    --arg prev_rev "$PROMOTED" \
    --arg cand_gen "$CANDIDATE_COMPOSE" \
    --arg cand_rev "$CANDIDATE" \
    --arg phase "$phase" \
    --arg canary "$canary_result" \
    --arg stall "$stall_reason" \
    --argjson include_candidate "$include_candidate" \
    '{
      version: 1,
      generations_root: $generations_root,
      promoted: {
        generation: $prev_gen,
        output_revision: $prev_rev,
        promoted_at: "2026-01-01T00:00:00Z"
      },
      occupancy: [
        {
          name: "hera",
          runtime_generation: {sha: $prev_rev, label: "omp@aaaaaaaa"},
          upgrade_target: null,
          state: "live",
          reason: "",
          acked_at: "2026-01-01T00:00:00Z"
        }
      ],
      recovery: {
        target: {sha: $prev_rev, label: "omp@aaaaaaaa"},
        midi_alive_at: "",
        reason: ""
      },
      stall: (if $stall == "" then {phase: $phase, reason: ""} else {phase: $phase, reason: $stall} end)
    }
    + (if $include_candidate then {
      candidate: {
        generation: $cand_gen,
        output_revision: $cand_rev,
        composed_at: "2026-01-02T00:00:00Z",
        phase: $phase
      },
      canary: {
        actor: "midi",
        result: $canary,
        observed_generation: "",
        continuity: {},
        recorded_at: ""
      }
    } else {} end)' >"$DEPLOY"
}

inspect_phase() {
  local out=$1
  jq -r '
    .data.deployment.phase
    // .data.automation.deployment.phase
    // empty
  ' <<<"$out"
}

has_attention() {
  local out=$1
  local code=$2
  local unit=$3
  jq -e --arg code "$code" --arg unit "$unit" '
    [.data.attention[]? | select(.code == $code and .operation == $unit)] | length == 1
  ' <<<"$out" >/dev/null
}

# Fixture 1: candidate + pending canary => phase=candidate, attention canary_pending.
write_deployment candidate pending "" true
INSPECT=$(json automation inspect --unit "$STEM")
ok "$INSPECT"
phase=$(inspect_phase "$INSPECT")
[[ $phase == candidate ]] || fail "expected phase=candidate, got '$phase': $INSPECT"
jq -e '
  (.data.deployment.stall // .data.automation.deployment.stall) | type == "object"
' <<<"$INSPECT" >/dev/null || fail "inspect missing stall: $INSPECT"
jq -e '
  (.data.deployment.occupancy // .data.automation.deployment.occupancy) | type == "array"
' <<<"$INSPECT" >/dev/null || fail "inspect missing occupancy: $INSPECT"
jq -e '
  (.data.deployment.recovery // .data.automation.deployment.recovery) | type == "object"
' <<<"$INSPECT" >/dev/null || fail "inspect missing recovery: $INSPECT"

SHOW=$(json scope show)
ok "$SHOW"
[[ $(jq -er '.data.health' <<<"$SHOW") == healthy ]] \
  || fail "canary_pending degraded health: $SHOW"
has_attention "$SHOW" canary_pending "$STEM" \
  || fail "missing canary_pending attention: $SHOW"

GET=$(json inspect get-operation --unit "$STEM")
ok "$GET"
jq -e --arg phase "$phase" '.data.automation.deployment.phase == $phase' <<<"$GET" >/dev/null \
  || fail "get-operation phase != inspect '$phase': $GET"

# Fixture 2: stall.reason set => deploy_stalled. Health stays healthy.
write_deployment candidate pending "canary actor missing" true
INSPECT=$(json automation inspect --unit "$STEM")
ok "$INSPECT"
stall=$(jq -er '.data.deployment.stall.reason // .data.automation.deployment.stall.reason // empty' <<<"$INSPECT")
[[ $stall == "canary actor missing" ]] || fail "expected stall reason, got '$stall': $INSPECT"
SHOW=$(json scope show)
ok "$SHOW"
[[ $(jq -er '.data.health' <<<"$SHOW") == healthy ]] \
  || fail "deploy_stalled degraded health: $SHOW"
has_attention "$SHOW" deploy_stalled "$STEM" \
  || fail "missing deploy_stalled attention: $SHOW"

GET=$(json inspect get-operation --unit "$STEM")
ok "$GET"
jq -e --arg stall "$stall" '.data.automation.deployment.stall.reason == $stall' <<<"$GET" >/dev/null \
  || fail "get-operation stall != inspect '$stall': $GET"

# Fixture 3: integrator active + no candidate => composing (inspect-derived).
rm -f "$DEPLOY"
write_deployment candidate pending "" false
touch "$TMP/integrator-active"
INSPECT=$(json automation inspect --unit "$STEM")
ok "$INSPECT"
phase=$(inspect_phase "$INSPECT")
[[ $phase == composing ]] || fail "expected phase=composing, got '$phase': $INSPECT"
rm -f "$TMP/integrator-active"

# Fixture 4: cap desired commit not ancestor / missing delta path =>
# desired_unconsumed and admission_unconsumed.
DELTA_SHA=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
jq -n \
  --arg rev "$FOREIGN" \
  --arg sha "$DELTA_SHA" \
  --arg head "$HEAD_REV" \
  '{
    version: 1,
    inputs: [
      {
        originating_work: "omp-proof.1",
        artifact_kind: "commit",
        artifact_revision: $rev,
        artifact_identity: $rev,
        admitted_at: "2026-01-01T00:00:00Z"
      },
      {
        originating_work: "omp-proof.2",
        artifact_kind: "worktree-delta",
        base: $head,
        delta: [{path: "does-not-exist.rs", sha256: $sha}],
        artifact_identity: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        admitted_at: "2026-01-01T00:00:00Z"
      }
    ]
  }' >"$DESIRED"

CAP_INSPECT=$(json automation inspect --unit "$CAP_STEM")
ok "$CAP_INSPECT"
jq -e '
  (.data.desired_unconsumed // .data.automation.desired_unconsumed) == true
' <<<"$CAP_INSPECT" >/dev/null || fail "expected desired_unconsumed true: $CAP_INSPECT"
jq -e '
  (.data.desired_inputs // .data.automation.desired_inputs) | type == "object"
' <<<"$CAP_INSPECT" >/dev/null || fail "expected desired_inputs object: $CAP_INSPECT"
SHOW=$(json scope show)
ok "$SHOW"
[[ $(jq -er '.data.health' <<<"$SHOW") == healthy ]] \
  || fail "admission_unconsumed degraded health: $SHOW"
has_attention "$SHOW" admission_unconsumed "$CAP_STEM" \
  || fail "missing admission_unconsumed attention: $SHOW"

GET_ERR=$TMP/get-operation.err
GET=$(json inspect get-operation --unit "$CAP_STEM" 2>"$GET_ERR")
ok "$GET"
jq -e '.data.automation.desired_unconsumed == true' <<<"$GET" >/dev/null \
  || fail "get-operation expected desired_unconsumed true: $GET"
jq -e '.data.automation.desired_inputs | type == "object"' <<<"$GET" >/dev/null \
  || fail "get-operation expected desired_inputs object: $GET"
if grep -E 'fatal:|Not a valid commit name' "$GET_ERR"; then
  fail "get-operation printed git fatal: $(cat "$GET_ERR")"
fi

# Topology: two loops, no PR numbers, no instance names.
[[ -f $TOPO ]] || fail "missing $TOPO"
if grep -E 'PR [0-9]' "$TOPO"; then
  fail "topology names a PR number"
fi
if grep -F 'managed-omp-pr-' "$TOPO"; then
  fail "topology names managed-omp-pr-"
fi
for name in "Nested LSP" Hindsight Viewport Extensions "Agent Flag" Collab; do
  if grep -F "$name" "$TOPO"; then
    fail "topology names instance '$name'"
  fi
done
grep -qi 'capability' "$TOPO" || fail "topology does not describe capability-maintain"
grep -qi 'PR maintain\|pr-maintain\|PR-maintain' "$TOPO" \
  || fail "topology does not describe PR-maintain"
grep -qi 'compose' "$TOPO" || fail "topology does not describe compose"
grep -qi 'canary' "$TOPO" || fail "topology does not describe canary"
grep -qi 'occupancy' "$TOPO" || fail "topology does not describe occupancy"
grep -qi 'GC\|gc' "$TOPO" || fail "topology does not describe GC"

live_gens_after=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_after=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)
[[ $live_gens_before == "$live_gens_after" ]] \
  || fail "live generations/current moved: before=$live_gens_before after=$live_gens_after"
[[ $live_omp_before == "$live_omp_after" ]] \
  || fail "live omp install moved: before=$live_omp_before after=$live_omp_after"

echo "PASS: deploy inspect and admission stalls"
