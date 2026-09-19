#!/usr/bin/env bash
# Generation GC uses published occupancy. Throwaway scope; no live units.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
TMP=$(mktemp -d)
SCOPE=$TMP/scope
STEM=managed-omp-runtime-integrator
HOME_DIR=$SCOPE/.systemd-ops/operations/$STEM
STATE_DIR=$HOME_DIR/state
DEPLOY=$STATE_DIR/deployment.json
PUBLISHED=$STATE_DIR/published-generation.json
GENS=$TMP/generations
UNIT_DIR=$TMP/xdg/systemd/user
UNIT=$UNIT_DIR/$STEM.service
AGENT=$TMP/agents
HOLD_PID=""
trap 'if [[ -n ${HOLD_PID:-} ]]; then kill "$HOLD_PID" 2>/dev/null || true; wait "$HOLD_PID" 2>/dev/null || true; fi; rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { jq -e '.ok == true' <<<"$1" >/dev/null || fail "expected ok: $1"; }
rejected() { jq -e '.ok == false' <<<"$1" >/dev/null || fail "expected rejection: $1"; }

command -v jq >/dev/null || fail "jq required"
command -v sha256sum >/dev/null || fail "sha256sum required"
[[ -x $BIN ]] || fail "missing $BIN"

PROMOTED=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
CANDIDATE=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
LEFTOVER=cccccccccccccccccccccccccccccccccccccccc
OLD_LIVE=dddddddddddddddddddddddddddddddddddddddd
PROMOTED_COMPOSE=1111111111111111111111111111111111111111
CANDIDATE_COMPOSE=2222222222222222222222222222222222222222
PROMOTED_LABEL=omp@aaaaaaaa
CANDIDATE_LABEL=omp@bbbbbbbb
LEFTOVER_LABEL=omp@cccccccc
OLD_LIVE_LABEL=omp@dddddddd

LIVE_GENS=/home/sf/worktrees/omp/generations
LIVE_OMP=/home/sf/.bun/bin/omp
live_gens_before=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_before=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)

mkdir -p "$HOME_DIR" "$UNIT_DIR" "$SCOPE/.systemd-ops" "$AGENT/.omp/agents" "$TMP/bin" "$STATE_DIR"
printf '%s\n' 'runtime-integrator role' >"$AGENT/.omp/agents/runtime-integrator.md"

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

cat >"$UNIT" <<EOF
# Managed by systemd-ops
[Unit]
Description=runtime integrator
[Service]
Type=oneshot
ExecStart=/bin/true
WorkingDirectory=$SCOPE
EOF

cat >"$TMP/bin/systemctl" <<EOF
#!/bin/sh
while [ "\${1:-}" = --user ]; do shift; done
case " \$* " in
  *" list-unit-files "*)
    printf '%s\n' '[{"unit_file":"$STEM.service","state":"static","preset":"enabled"}]'
    ;;
  *" list-units "*|*" list-timers "*)
    printf '%s\n' '[]'
    ;;
  *" show "*)
    printf 'LoadState=loaded\nActiveState=inactive\nSubState=dead\nFragmentPath=%s\n' "$UNIT"
    ;;
  *)
    printf '%s\n' '[]'
    ;;
esac
EOF
chmod 755 "$TMP/bin/systemctl"

write_ompalt() {
  local dest=$1
  mkdir -p "$dest/packages/coding-agent/scripts"
  cat >"$dest/packages/coding-agent/scripts/ompalt" <<'SCRIPT'
#!/bin/sh
echo ompalt-fixture
exit 0
SCRIPT
  chmod 755 "$dest/packages/coding-agent/scripts/ompalt"
}

seed_generations() {
  rm -rf "$GENS"
  mkdir -p "$GENS"
  write_ompalt "$GENS/$PROMOTED"
  write_ompalt "$GENS/$CANDIDATE"
  write_ompalt "$GENS/$LEFTOVER"
  write_ompalt "$GENS/$OLD_LIVE"
  ln -sfn "$GENS/$PROMOTED" "$GENS/current"
  ln -sfn "$GENS/$PROMOTED/packages/coding-agent/scripts/ompalt" "$TMP/bin/omp"
  jq -n \
    --arg generation "$PROMOTED_COMPOSE" \
    --arg output_revision "$PROMOTED" \
    --arg published_at "2026-01-01T00:00:00Z" \
    --arg path "$(readlink -f "$GENS/$PROMOTED")" \
    '{version:1,generation:$generation,output_revision:$output_revision,published_at:$published_at,path:$path}' \
    >"$PUBLISHED"
  jq -n \
    --arg generations_root "$(readlink -f "$GENS")" \
    --arg prev_gen "$PROMOTED_COMPOSE" \
    --arg prev_rev "$PROMOTED" \
    --arg cand_gen "$CANDIDATE_COMPOSE" \
    --arg cand_rev "$CANDIDATE" \
    '{
      version: 1,
      generations_root: $generations_root,
      promoted: {
        generation: $prev_gen,
        output_revision: $prev_rev,
        promoted_at: "2026-01-01T00:00:00Z"
      },
      candidate: {
        generation: $cand_gen,
        output_revision: $cand_rev,
        composed_at: "2026-01-02T00:00:00Z",
        phase: "candidate"
      },
      canary: {
        actor: "midi",
        result: "fail",
        observed_generation: $cand_rev,
        continuity: {actor: "midi", session_id: "sess-canary-1"},
        recorded_at: "2026-01-02T01:00:00Z",
        reason: "result=fail"
      },
      recovery: {
        target: {sha: $prev_rev, label: "omp@aaaaaaaa"},
        midi_alive_at: "",
        reason: "result=fail"
      },
      occupancy: []
    }' >"$DEPLOY"
}

retained_for() {
  local sha=$1
  jq -c --arg sha "$sha" '
    (.data.runtime_generation.retained // .data.automation.runtime_generation.retained // [])
    | map(select(.sha == $sha)) | .[0]
  ' <<<"$INSPECT"
}

assert_retained() {
  local sha=$1 label=$2
  shift 2
  local row
  row=$(retained_for "$sha")
  [[ $row != null && -n $row ]] || fail "inspect retained missing $sha: $INSPECT"
  jq -e --arg label "$label" '.label == $label' <<<"$row" >/dev/null \
    || fail "retained label for $sha is not $label: $row"
  local reason
  for reason in "$@"; do
    jq -e --arg reason "$reason" '.reasons | index($reason) != null' <<<"$row" >/dev/null \
      || fail "retained $sha missing reason $reason: $row"
  done
}

export PATH=$TMP/bin:$PATH
export HOME=$TMP
export XDG_CONFIG_HOME=$TMP/xdg
export SYSTEMD_OPS_STATE_DIR=$TMP/state
export SYSTEMD_OPS_SCOPE_ROOT=$SCOPE
export SYSTEMD_OPS_OPERATION=$STEM
export OMP_INSTALL=$TMP/bin/omp
export GENERATIONS_ROOT=$GENS
export RUNTIME_PROMOTE=$ROOT/dogfood/lib/runtime-promote

json() {
  "$BIN" --json --manager user --write-prefix 'managed-*' --scope-root "$SCOPE" "$@"
}

apply_token() {
  json automation apply --plan-token "$1"
}

occupancy_apply() {
  local spec=$1
  local planned applied
  planned=$(json automation plan-deploy-occupancy --spec "$spec")
  ok "$planned"
  applied=$(apply_token "$(jq -er '.data.plan_token' <<<"$planned")")
  ok "$applied"
  printf '%s\n' "$applied"
}

gc_plan() {
  json automation plan-deploy-gc --generation "$1"
}

gc_apply() {
  local planned applied
  planned=$(gc_plan "$1")
  ok "$planned"
  applied=$(apply_token "$(jq -er '.data.plan_token' <<<"$planned")")
  ok "$applied"
  printf '%s\n' "$applied"
}

expect_gc_refused() {
  local sha=$1 needle=$2
  local planned applied token
  planned=$(gc_plan "$sha" || true)
  if jq -e '.ok == true' <<<"$planned" >/dev/null 2>&1; then
    token=$(jq -er '.data.plan_token' <<<"$planned")
    applied=$(apply_token "$token" || true)
    rejected "$applied"
    grep -Fq "$needle" <<<"$applied" || fail "GC apply refuse for $sha missing '$needle': $applied"
  else
    rejected "$planned"
    grep -Fq "$needle" <<<"$planned" || fail "GC plan refuse for $sha missing '$needle': $planned"
  fi
  [[ -d $GENS/$sha ]] || fail "refused GC deleted $sha"
}

mixed_occupancy_spec() {
  jq -nc \
    --arg generation "$PROMOTED" \
    --arg promoted "$PROMOTED" \
    --arg promoted_label "$PROMOTED_LABEL" \
    --arg old "$OLD_LIVE" \
    --arg old_label "$OLD_LIVE_LABEL" \
    --arg leftover "$LEFTOVER" \
    --arg leftover_label "$LEFTOVER_LABEL" \
    '{
      version: 1,
      generation: $generation,
      actors: [
        {
          name: "midi",
          runtime_generation: {sha: $promoted, label: $promoted_label},
          upgrade_target: null,
          state: "live",
          reason: "",
          acked_at: "2026-09-19T05:00:00Z"
        },
        {
          name: "hera",
          runtime_generation: {sha: $old, label: $old_label},
          upgrade_target: {sha: $promoted, label: $promoted_label},
          state: "live",
          reason: "already-live",
          acked_at: "2026-09-19T05:00:01Z"
        },
        {
          name: "gina",
          runtime_generation: {sha: $leftover, label: $leftover_label},
          upgrade_target: null,
          state: "stopped",
          reason: "",
          acked_at: "2026-09-19T05:00:02Z"
        }
      ]
    }'
}

converged_occupancy_spec() {
  jq -nc \
    --arg generation "$CANDIDATE" \
    --arg sha "$CANDIDATE" \
    --arg label "$CANDIDATE_LABEL" \
    '{
      version: 1,
      generation: $generation,
      actors: [
        {
          name: "midi",
          runtime_generation: {sha: $sha, label: $label},
          upgrade_target: null,
          state: "live",
          reason: "",
          acked_at: "2026-09-19T05:10:00Z"
        },
        {
          name: "hera",
          runtime_generation: {sha: $sha, label: $label},
          upgrade_target: null,
          state: "live",
          reason: "",
          acked_at: "2026-09-19T05:10:01Z"
        },
        {
          name: "gina",
          runtime_generation: {sha: $sha, label: $label},
          upgrade_target: null,
          state: "retired",
          reason: "",
          acked_at: "2026-09-19T05:10:02Z"
        }
      ]
    }'
}

midi_pass_spec() {
  jq -nc --arg gen "$CANDIDATE" '{
    version: 1,
    generation: $gen,
    actor: "midi",
    result: "pass",
    observed_generation: $gen,
    continuity: {actor: "midi", session_id: "sess-canary-1"}
  }'
}

seed_generations

"$BIN" --help | grep -q 'plan-deploy-occupancy' || fail "USAGE missing plan-deploy-occupancy"
"$BIN" --help | grep -q 'plan-deploy-gc' || fail "USAGE missing plan-deploy-gc"

INSPECT=$(json automation inspect --unit "$STEM")
ok "$INSPECT"
assert_retained "$PROMOTED" "$PROMOTED_LABEL" promoted current-symlink
assert_retained "$CANDIDATE" "$CANDIDATE_LABEL" candidate failed-canary
assert_retained "$OLD_LIVE" "$OLD_LIVE_LABEL"
row=$(retained_for "$OLD_LIVE")
jq -e '.reasons == []' <<<"$row" >/dev/null \
  || fail "old-live should be empty-reasons before occupancy: $row"
row=$(retained_for "$LEFTOVER")
[[ $row != null && -n $row ]] || fail "inspect retained missing leftover $LEFTOVER: $INSPECT"
jq -e --arg label "$LEFTOVER_LABEL" '.label == $label and .reasons == []' <<<"$row" >/dev/null \
  || fail "leftover is not GC-eligible empty reasons: $row"
jq -e --arg label "$OLD_LIVE_LABEL" '
  (.data.runtime_generation.retained // .data.automation.runtime_generation.retained)
  | map(select(.reasons | length > 0)) | all(.label | startswith("omp@"))
' <<<"$INSPECT" >/dev/null || fail "kept retained labels are not omp@+8: $INSPECT"

occupancy_apply "$(mixed_occupancy_spec)" >/dev/null
jq -e --arg old "$OLD_LIVE" --arg label "$OLD_LIVE_LABEL" --arg promoted "$PROMOTED" '
  (.occupancy | length) == 3
  and (.occupancy[] | select(.name == "hera") | .state == "live" and .runtime_generation.sha == $old and .runtime_generation.label == $label)
  and (.occupancy[] | select(.name == "midi") | .state == "live" and .runtime_generation.sha == $promoted)
  and (.occupancy[] | select(.name == "gina") | .state == "stopped")
  and .candidate.phase != "converged"
' "$DEPLOY" >/dev/null || fail "occupancy apply did not keep already-live hera on old sha: $(cat "$DEPLOY")"
[[ -d $GENS/$OLD_LIVE ]] || fail "live occupancy row deleted old generation"
[[ -d $GENS/$LEFTOVER ]] || fail "occupancy apply deleted leftover"

INSPECT=$(json automation inspect --unit "$STEM")
ok "$INSPECT"
assert_retained "$OLD_LIVE" "$OLD_LIVE_LABEL" "live-occupancy:hera"
row=$(retained_for "$LEFTOVER")
jq -e '.reasons == []' <<<"$row" >/dev/null \
  || fail "stopped occupancy must not retain leftover: $row"

expect_gc_refused "$OLD_LIVE" "live"
expect_gc_refused "$PROMOTED" "current"
[[ -d $GENS/$PROMOTED ]] || fail "GC of current deleted promoted"

hold="$GENS/$LEFTOVER/hold-sleep"
cp /bin/sleep "$hold"
chmod 755 "$hold"
"$hold" 120 &
HOLD_PID=$!
sleep 0.1
kill -0 "$HOLD_PID" 2>/dev/null || fail "process-executable fixture is not running"
expect_gc_refused "$LEFTOVER" "process"
kill "$HOLD_PID" 2>/dev/null || true
wait "$HOLD_PID" 2>/dev/null || true
HOLD_PID=""

gc_apply "$LEFTOVER" >/dev/null
[[ ! -d $GENS/$LEFTOVER ]] || fail "GC did not remove leftover directory"
jq -e --arg leftover "$LEFTOVER" '
  (.occupancy | map(select(.runtime_generation.sha == $leftover)) | length) == 0
' "$DEPLOY" >/dev/null || fail "GC did not drop leftover from deployment.json: $(cat "$DEPLOY")"
INSPECT=$(json automation inspect --unit "$STEM")
ok "$INSPECT"
row=$(retained_for "$LEFTOVER")
[[ $row == null || -z $row ]] || fail "inspect still lists GC'd leftover: $row"

planned=$(json automation plan-deploy-canary-ack --spec "$(midi_pass_spec)")
ok "$planned"
applied=$(apply_token "$(jq -er '.data.plan_token' <<<"$planned")")
ok "$applied"
[[ $(basename "$(readlink -f "$GENS/current")") == "$CANDIDATE" ]] \
  || fail "midi live occupancy on recovery.target did not allow canary pass: current=$(readlink -f "$GENS/current")"
jq -e --arg sha "$CANDIDATE" '.candidate.phase == "rolling_out" and .promoted.output_revision == $sha' "$DEPLOY" >/dev/null \
  || fail "canary pass after occupancy midi live did not roll out: $(cat "$DEPLOY")"

occupancy_apply "$(converged_occupancy_spec)" >/dev/null
jq -e '.candidate.phase == "converged"' "$DEPLOY" >/dev/null \
  || fail "occupancy acked on promoted did not converge: $(cat "$DEPLOY")"
jq -e --arg old "$OLD_LIVE" --arg sha "$CANDIDATE" '
  (.occupancy[] | select(.name == "hera") | .runtime_generation.sha == $sha)
  and (.occupancy | map(select(.runtime_generation.sha == $old and .state == "live")) | length) == 0
' "$DEPLOY" >/dev/null || fail "converged occupancy did not rewrite hera onto promoted: $(cat "$DEPLOY")"

if grep -n prune_runtime_generations "$ROOT/dogfood/drivers/runtime-run"; then
  fail "runtime-run still names prune_runtime_generations"
fi
grep -q 'tests/deploy-gc.sh' "$ROOT/docs/TESTING.md" || fail "TESTING.md Fast gates missing deploy-gc.sh"
grep -q 'tests/deploy-gc.sh' "$ROOT/.github/workflows/ci.yml" || fail "ci.yml Dogfood proofs missing deploy-gc.sh"

if grep -nE 'recycle_actor|hcom r' "$ROOT/src/automation.rs" "$ROOT/src/bin/systemd_ops.rs"; then
  fail "src gained recycle_actor or hcom r"
fi

live_gens_after=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_after=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)
[[ $live_gens_after == "$live_gens_before" ]] || fail "mutated live generations/current"
[[ $live_omp_after == "$live_omp_before" ]] || fail "mutated live omp install"

echo "deploy-gc ok"
