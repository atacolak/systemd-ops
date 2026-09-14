#!/usr/bin/env bash
# Capability runner: release barrier, 5m quiet, BLOCKED PR settled, no OMP.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
DOGFOOD_ROOT=${DOGFOOD_ROOT:-$ROOT/dogfood}
SOURCE_DRIVER=${SOURCE_DRIVER:-$DOGFOOD_ROOT/drivers/capability-run}
SOURCE_LIB=${SOURCE_LIB:-$DOGFOOD_ROOT/lib/automation-wrapper}
SETTLE_LIB=${SETTLE_LIB:-$DOGFOOD_ROOT/lib/pr-settlement}
REVIEW_LIB=${REVIEW_LIB:-$DOGFOOD_ROOT/lib/pr-review-state}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -x $BIN ]] || fail "missing $BIN"
[[ -x $SOURCE_DRIVER ]] || fail "missing $SOURCE_DRIVER"
[[ -r $SOURCE_LIB && -r $SETTLE_LIB && -r $REVIEW_LIB ]] || fail "missing libs"

G1=generation-G1
G2=generation-G2
CAP=managed-omp-cap-proof
PRA=managed-omp-pr-proof-a
PRB=managed-omp-pr-proof-b
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
SCOPE=$TMP/scope
XDG=$TMP/xdg
export XDG_CONFIG_HOME=$XDG
mkdir -p "$XDG/systemd/user"

install_unit() {
  local unit=$1
  cat >"$XDG/systemd/user/${unit}.service" <<EOF
[Service]
ExecStart=/bin/true
EOF
}

write_child() {
  local unit=$1 running=$2 obs_fp=$3 obs_gen=$4 proc_fp=$5 outcome=$6 finished=$7
  mkdir -p "$SCOPE/.systemd-ops/operations/$unit/state"
  cat >"$SCOPE/.systemd-ops/operations/$unit/automation.toml" <<EOF
version = 1
agent = "pr-maintainer"
parent = "$CAP"
EOF
  jq -n --arg fp "$obs_fp" --arg gen "$obs_gen" \
    '{version:1,world_fingerprint:$fp,brain_revision:"sha256:brain",input_fingerprint:$fp,generation:$gen}' \
    >"$SCOPE/.systemd-ops/operations/$unit/state/observation.json"
  jq -n --arg fp "$proc_fp" --arg out "$outcome" \
    '{version:1,input_fingerprint:$fp,outcome:$out,processed_at:"2026-09-01T00:00:00.000000Z"}' \
    >"$SCOPE/.systemd-ops/operations/$unit/state/processed.json"
  if [[ $running == true ]]; then
    jq -n --arg id "it-$unit" \
      '{active_iteration:{id:$id,started_at:"2026-09-01T00:00:00.000000Z"},iterations:[]}' \
      >"$SCOPE/.systemd-ops/operations/$unit/state/operator.json"
  else
    jq -n --arg id "it-$unit" --arg finished "$finished" \
      '{iterations:[{id:$id,started_at:$finished,finished_at:$finished,exit_code:0,reconsolidated:true}]}' \
      >"$SCOPE/.systemd-ops/operations/$unit/state/operator.json"
  fi
}

make_scope() {
  CAPABILITY_BRANCH=cap/hindsight
  rm -rf "$SCOPE"
  mkdir -p "$SCOPE/.systemd-ops/operations/$CAP/state" "$SCOPE/.systemd-ops/drivers" "$SCOPE/.systemd-ops/lib" "$SCOPE/worktree" "$SCOPE/agents/.omp/agents" "$SCOPE/.systemd-ops/operations/managed-omp-release-watch/state" "$SCOPE/fork.git"
  git -C "$SCOPE/fork.git" init -q --bare
  git -C "$SCOPE/worktree" init -q
  git -C "$SCOPE/worktree" config user.email proof@example.invalid
  git -C "$SCOPE/worktree" config user.name proof
  git -C "$SCOPE/worktree" checkout -q -b cap/hindsight
  git -C "$SCOPE/worktree" commit --allow-empty -qm cap
  git -C "$SCOPE/worktree" remote add fork "$SCOPE/fork.git"
  git -C "$SCOPE/worktree" push -q fork cap/hindsight
  git -C "$SCOPE/worktree" push -q fork cap/hindsight:main
  git -C "$SCOPE/worktree" fetch -q fork
  jq -n --arg t "$G2" '{version:1,mode:"release",target_commit:$t}' \
    >"$SCOPE/.systemd-ops/operations/managed-omp-release-watch/state/generation.json"
  cat >"$SCOPE/.systemd-ops/scope.toml" <<EOF
[scope]
id = "omp-proof"
owned = ["managed-omp-*"]
[automation]
agent_root = "$SCOPE/agents"
EOF
  cat >"$SCOPE/agents/.omp/agents/capability-maintainer.md" <<'EOF'
---
name: capability-maintainer
description: proof
hide: true
tools: [automation_context, automation_report]
---
proof
EOF
  cat >"$SCOPE/.systemd-ops/operations/$CAP/automation.toml" <<EOF
version = 1
agent = "capability-maintainer"
brain_paths = [".systemd-ops/drivers/capability-observe", ".systemd-ops/drivers/capability-run", ".systemd-ops/lib/automation-wrapper"]
output_revision_required = true
[observation]
exec = "drivers/capability-observe"
args = ["cap/hindsight"]
EOF
  jq -n --arg fp "old-fp" --arg gen "$G1" --arg rev "$(git -C "$SCOPE/worktree" rev-parse HEAD)" \
    '{version:2,input_fingerprint:$fp,generation:$gen,output_revision:$rev,checkpointed_at:"2026-09-01T00:00:00.000000Z"}' \
    >"$SCOPE/.systemd-ops/operations/$CAP/state/checkpoint.json"
  cp "$SOURCE_DRIVER" "$SCOPE/.systemd-ops/drivers/capability-run"
  chmod +x "$SCOPE/.systemd-ops/drivers/capability-run"
  cp "$SOURCE_LIB" "$SCOPE/.systemd-ops/lib/automation-wrapper"
  cp "$SETTLE_LIB" "$SCOPE/.systemd-ops/lib/pr-settlement"
  cp "$REVIEW_LIB" "$SCOPE/.systemd-ops/lib/pr-review-state"
  cat >"$SCOPE/.systemd-ops/drivers/capability-observe" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
world=${PROOF_FINGERPRINT:-fp-cap}
jq -cn --arg world "$world" --arg generation "${UPSTREAM_GENERATION:-generation-G2}" \
  '{version:1,world_fingerprint:$world,generation:$generation}'
EOF
  chmod +x "$SCOPE/.systemd-ops/drivers/capability-observe"
  cat >"$SCOPE/.systemd-ops/operations/$CAP/run" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec "\${SYSTEMD_OPS_SCOPE_ROOT}/.systemd-ops/drivers/capability-run" "\${CAPABILITY_BRANCH:-cap/hindsight}" $CAP "\${WORKTREE}"
EOF
  chmod +x "$SCOPE/.systemd-ops/operations/$CAP/run"
  install_unit "$CAP"
  install_unit "$PRA"
  install_unit "$PRB"
  cat >"$SCOPE/fake-omp" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
echo called >>"${SYSTEMD_OPS_SCOPE_ROOT}/agent-calls"
"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "cap ready" --summary '["ok"]' --outcome ready >/dev/null
EOF
  chmod +x "$SCOPE/fake-omp"
  cat >"$SCOPE/fake-ops" <<EOF
#!/usr/bin/env bash
set -euo pipefail
real="$BIN"
args=("\$@")
joined=" \${args[*]} "
printf '%s\n' "\${args[*]}" >>"$SCOPE/ops-argv.log"
if [[ "\$joined" == *" automation context "* ]]; then
  cat "$SCOPE/context.json"
  exit 0
fi
if [[ "\$joined" == *" automation observe "* ]]; then
  jq -n --arg fp "\${PROOF_FINGERPRINT:-fp-cap}" --arg gen "\${UPSTREAM_GENERATION:-generation-G2}" \
    '{ok:true,data:{observation:{input_fingerprint:\$fp,generation:\$gen}}}'
  exit 0
fi
if [[ -n "\${PROOF_REJECT_REPORT:-}" && "\$joined" == *" automation report "* ]]; then
  echo "proof-forced report rejection" >&2
  exit 1
fi
exec "\$real" "\${args[@]}"
EOF
  chmod +x "$SCOPE/fake-ops"
}

write_context() {
  python3 - <<'PY'
import json, os
from pathlib import Path
scope = Path(os.environ["SCOPE"])
cap = os.environ["CAP"]
children = []
for unit in os.environ["CHILD_UNITS"].split():
    home = scope / ".systemd-ops/operations" / unit
    auto = {}
    for line in (home / "automation.toml").read_text().splitlines():
        if "=" in line and not line.strip().startswith("["):
            k,v = line.split("=",1)
            auto[k.strip()] = v.strip().strip('"')
    obs = json.loads((home/"state/observation.json").read_text())
    proc = json.loads((home/"state/processed.json").read_text())
    op = json.loads((home/"state/operator.json").read_text())
    active = op.get("active_iteration")
    latest = (op.get("iterations") or [{}])[0]
    children.append({
        "unit": unit,
        "agent": auto.get("agent","pr-maintainer"),
        "lifecycle": "active",
        "running": bool(active),
        "active_iteration": bool(active),
        "semantic_state": proc.get("outcome") or "stale",
        "observation": {"input_fingerprint": obs.get("input_fingerprint"), "generation": obs.get("generation")},
        "processed": {"input_fingerprint": proc.get("input_fingerprint"), "outcome": proc.get("outcome")},
        "latest_iteration": {"id": latest.get("id") or (active or {}).get("id"), "finished_at": latest.get("finished_at")},
        "checkpoint": {"present": True, "kind": "structured", "generation": obs.get("generation"), "output_revision": "rev"},
        "blocker": None,
    })
automation = {}
cap_proc = scope / ".systemd-ops/operations" / cap / "state/processed.json"
if cap_proc.exists():
    proc = json.loads(cap_proc.read_text())
    automation["processed"] = {"input_fingerprint": proc.get("input_fingerprint"), "outcome": proc.get("outcome")}
print(json.dumps({"ok": True, "data": {"relations": {"children": children, "parent": None}, "automation": automation}}))
PY
}

run_cap() {
  local now=$1
  local generation=${2:-$G2}
  export SCOPE CAP
  export CHILD_UNITS="$PRA $PRB"
  write_context >"$SCOPE/context.json"
  WORKTREE="$SCOPE/worktree" \
  SYSTEMD_OPS_SCOPE_ROOT="$SCOPE" \
  SYSTEMD_OPS_BIN="$SCOPE/fake-ops" \
  OMP_BIN="$SCOPE/fake-omp" \
  AGENT_CWD="$SCOPE/agents" \
  UPSTREAM_GENERATION="$generation" \
  CAPABILITY_BRANCH="${CAPABILITY_BRANCH:-cap/hindsight}" \
  PROOF_REJECT_REPORT="${PROOF_REJECT_REPORT:-}" \
  PR_SETTLE_QUIET_SECONDS=300 \
  PR_SETTLE_NOW="$now" \
  "$SCOPE/.systemd-ops/operations/$CAP/run"
}

# Pin the target generation to the worktree's own checkpoint so the release
# gate launches immediately: the no-op decision, not the quiet window, is what
# these cases exercise.
set_target() {
  jq -n --arg fp "old-fp" --arg gen "$1" --arg rev "$(git -C "$SCOPE/worktree" rev-parse HEAD)" \
    '{version:2,input_fingerprint:$fp,generation:$gen,output_revision:$rev,checkpointed_at:"2026-09-01T00:00:00.000000Z"}' \
    >"$SCOPE/.systemd-ops/operations/$CAP/state/checkpoint.json"
}

# Replace the agent stub. $1 is a verbatim body line, empty for a silent exit.
write_fake_omp() {
  local body=$1
  {
    printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' \
      'echo called >>"${SYSTEMD_OPS_SCOPE_ROOT}/agent-calls"'
    [[ -n $body ]] && printf '%s\n' "$body"
  } >"$SCOPE/fake-omp"
  chmod +x "$SCOPE/fake-omp"
}

run_cap_code() {
  local now=$1
  local generation=${2:-$G2}
  local log=$3
  set +e
  run_cap "$now" "$generation" >"$log" 2>&1
  local code=$?
  set -e
  printf '%s\n' "$code"
}
make_scope
write_child "$PRA" true obs-a "$G2" obs-a ready "2026-09-01T00:00:00Z"
write_child "$PRB" false obs-b "$G2" obs-old ready "2026-09-01T00:00:00Z"
run_cap 1000 >/tmp/cap-unsettled.out 2>&1 || true
[[ ! -e $SCOPE/agent-calls ]] || fail "unsettled children invoked OMP: $(cat /tmp/cap-unsettled.out)"
[[ ! -e $SCOPE/.systemd-ops/operations/$CAP/state/operator.json ]] || fail "unsettled children started an iteration"

make_scope
write_child "$PRA" false obs-a "$G2" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$G2" obs-b ready "2026-09-01T00:01:00Z"
run_cap 1000 >/tmp/cap-t0.out 2>&1 || true
[[ ! -e $SCOPE/agent-calls ]] || fail "t+0 quiet invoked OMP"
run_cap 1299 >/tmp/cap-t299.out 2>&1 || true
[[ ! -e $SCOPE/agent-calls ]] || fail "t+299 quiet invoked OMP"
run_cap 1300 >/tmp/cap-t300.out 2>&1 || true
[[ -f $SCOPE/agent-calls ]] || fail "t+300 quiet did not invoke OMP: $(cat /tmp/cap-t300.out)"
[[ $(wc -l <"$SCOPE/agent-calls") -eq 1 ]] || fail "t+300 invoked OMP more than once"

make_scope
write_child "$PRA" false obs-a "$G2" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$G2" obs-b ready "2026-09-01T00:01:00Z"
run_cap 1000 >/dev/null 2>&1 || true
write_child "$PRA" false obs-a-new "$G2" obs-a-new blocked "2026-09-01T00:04:20Z"
run_cap 1200 >/tmp/cap-reset.out 2>&1 || true
[[ ! -e $SCOPE/agent-calls ]] || fail "quiet reset at +200 invoked OMP"
run_cap 1499 >/dev/null 2>&1 || true
[[ ! -e $SCOPE/agent-calls ]] || fail "reset +299 invoked OMP"
run_cap 1500 >/tmp/cap-reset-go.out 2>&1 || true
[[ -f $SCOPE/agent-calls ]] || fail "reset +300 did not invoke OMP: $(cat /tmp/cap-reset-go.out)"

CAP_STATE=$SCOPE/.systemd-ops/operations/$CAP/state

# Verified no-op: the pass exits 0 without reporting, and the driver can prove
# the worktree already carries the target with no local delta. The driver must
# emit the READY report itself on the pass's own iteration, reconsolidate, and
# reach the checkpoint path instead of parking on the contract failure.
make_scope
target=$(git -C "$SCOPE/worktree" rev-parse HEAD)
set_target "$target"
write_child "$PRA" false obs-a "$target" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$target" obs-b ready "2026-09-01T00:01:00Z"
write_fake_omp ''
code=$(run_cap_code 1000 "$target" "$TMP/cap-noop.out")
[[ $code -eq 0 ]] || fail "verified no-op exited $code: $(cat "$TMP/cap-noop.out")"
grep -q "automation_report is required" "$TMP/cap-noop.out" && fail "verified no-op took the contract-failure path"
jq -e '.iterations[0].reconsolidated == true' "$CAP_STATE/operator.json" >/dev/null \
  || fail "verified no-op did not reconsolidate"
jq -e '.outcome == "ready"' "$CAP_STATE/processed.json" >/dev/null \
  || fail "verified no-op did not process the input as ready"
[[ ! -e $CAP_STATE/failure-budget.json ]] || fail "verified no-op created a failure budget"
jq -e --arg gen "$target" '.version == 2 and .generation == $gen and (.output_revision | length == 40)' \
  "$CAP_STATE/checkpoint.json" >/dev/null || fail "verified no-op did not checkpoint the target generation"
[[ $(wc -l <"$SCOPE/agent-calls") -eq 1 ]] || fail "verified no-op did not run exactly one pass"
grep -c "already correct for" "$SCOPE/ops-argv.log" | grep -qx 1 || fail "driver did not emit exactly one no-op report"
grep "already correct for" "$SCOPE/ops-argv.log" | grep -q "${target:0:8}" \
  || fail "no-op report does not name the target generation"
[[ $(awk '/operator iteration-start/{s++} /automation report/{r++} /operator iteration-finish/{f++} END{print s" "r" "f}' "$SCOPE/ops-argv.log") == "1 1 1" ]] \
  || fail "self-report reused or duplicated an iteration: $(awk '/operator iteration-start|automation report|operator iteration-finish/{print}' "$SCOPE/ops-argv.log")"
[[ $(awk '/operator iteration-start/{printf "start "} /automation report/{printf "report "} /operator iteration-finish/{printf "finish "}' "$SCOPE/ops-argv.log") == "start report finish " ]] \
  || fail "self-report did not precede the pass's own iteration-finish"

# Missing ancestor: the target generation is real but is not an ancestor of
# HEAD, so the tree is unready rather than unchanged. It must not self-report.
make_scope
git -C "$SCOPE/worktree" commit --allow-empty -qm ahead
absent=$(git -C "$SCOPE/worktree" rev-parse HEAD)
git -C "$SCOPE/worktree" reset -q --hard HEAD~1
set_target "$absent"
write_child "$PRA" false obs-a "$absent" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$absent" obs-b ready "2026-09-01T00:01:00Z"
write_fake_omp ''
code=$(run_cap_code 1000 "$absent" "$TMP/cap-absent.out")
[[ $code -eq 3 ]] || fail "missing ancestor exited $code, want 3: $(cat "$TMP/cap-absent.out")"
grep -q "automation_report is required" "$TMP/cap-absent.out" || fail "missing ancestor did not hit the contract failure"
[[ $(grep -c "already correct for" "$SCOPE/ops-argv.log" || true) -eq 0 ]] || fail "missing ancestor self-reported READY"
jq -e '.iterations[0].reconsolidated == false' "$CAP_STATE/operator.json" >/dev/null \
  || fail "missing ancestor reconsolidated"
jq -e '.failures == 1' "$CAP_STATE/failure-budget.json" >/dev/null \
  || fail "missing ancestor did not record the contract failure"
[[ ! -e $CAP_STATE/processed.json ]] || fail "missing ancestor advanced processed state"

# Dirty worktree: the pass leaves an uncommitted product delta behind. The tree
# is not unchanged, so the driver must not close the pass itself.
make_scope
target=$(git -C "$SCOPE/worktree" rev-parse HEAD)
set_target "$target"
write_child "$PRA" false obs-a "$target" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$target" obs-b ready "2026-09-01T00:01:00Z"
write_fake_omp "printf 'local delta\n' >\"\$WORKTREE/cap-local-delta\""
code=$(run_cap_code 1000 "$target" "$TMP/cap-dirty.out")
[[ -e $SCOPE/worktree/cap-local-delta ]] || fail "dirty case did not stage a product delta"
[[ $code -eq 3 ]] || fail "dirty worktree exited $code, want 3: $(cat "$TMP/cap-dirty.out")"
grep -q "automation_report is required" "$TMP/cap-dirty.out" || fail "dirty worktree did not hit the contract failure"
[[ $(grep -c "already correct for" "$SCOPE/ops-argv.log" || true) -eq 0 ]] || fail "dirty worktree self-reported READY"
jq -e '.iterations[0].reconsolidated == false' "$CAP_STATE/operator.json" >/dev/null \
  || fail "dirty worktree reconsolidated"
jq -e '.failures == 1' "$CAP_STATE/failure-budget.json" >/dev/null \
  || fail "dirty worktree did not record the contract failure"
[[ ! -e $CAP_STATE/processed.json ]] || fail "dirty worktree advanced processed state"

# Divergent delta: HEAD and the target changed the same file from a common
# base, so a merge from the merge base is not empty. Not a no-op.
make_scope
printf 'base\n' >"$SCOPE/worktree/shared.txt"
git -C "$SCOPE/worktree" add shared.txt
git -C "$SCOPE/worktree" commit -qm base
git -C "$SCOPE/worktree" checkout -q -b proof/target
printf 'target\n' >"$SCOPE/worktree/shared.txt"
git -C "$SCOPE/worktree" commit -qam target
target=$(git -C "$SCOPE/worktree" rev-parse HEAD)
git -C "$SCOPE/worktree" checkout -q cap/hindsight
printf 'cap\n' >"$SCOPE/worktree/shared.txt"
git -C "$SCOPE/worktree" commit -qam cap
git -C "$SCOPE/worktree" push -q fork cap/hindsight:cap/hindsight
git -C "$SCOPE/worktree" push -q fork proof/target:proof/target
set_target "$target"
write_child "$PRA" false obs-a "$target" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$target" obs-b ready "2026-09-01T00:01:00Z"
write_fake_omp ''
merge_base=$(git -C "$SCOPE/worktree" merge-base HEAD "$target")
[[ -n $(git -C "$SCOPE/worktree" merge-tree "$merge_base" HEAD "$target") ]] \
  || fail "divergent fixture is not divergent"
code=$(run_cap_code 1000 "$target" "$TMP/cap-divergent.out")
[[ $code -eq 3 ]] || fail "divergent delta exited $code, want 3: $(cat "$TMP/cap-divergent.out")"
grep -q "automation_report is required" "$TMP/cap-divergent.out" || fail "divergent delta did not hit the contract failure"
[[ $(grep -c "already correct for" "$SCOPE/ops-argv.log" || true) -eq 0 ]] || fail "divergent delta self-reported READY"
jq -e '.iterations[0].reconsolidated == false' "$CAP_STATE/operator.json" >/dev/null \
  || fail "divergent delta reconsolidated"
jq -e '.failures == 1' "$CAP_STATE/failure-budget.json" >/dev/null \
  || fail "divergent delta did not record the contract failure"
[[ ! -e $CAP_STATE/processed.json ]] || fail "divergent delta advanced processed state"

# The agent's own report always wins: the pass reports blocked on a tree that
# would otherwise verify as a no-op. The driver must leave that report alone.
make_scope
target=$(git -C "$SCOPE/worktree" rev-parse HEAD)
set_target "$target"
write_child "$PRA" false obs-a "$target" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$target" obs-b ready "2026-09-01T00:01:00Z"
write_fake_omp "\"\$SYSTEMD_OPS_BIN\" --json --manager user automation report --headline cap-blocked --summary '[\"blocked\"]' --outcome blocked --route self >/dev/null"
code=$(run_cap_code 1000 "$target" "$TMP/cap-agent-report.out")
[[ $code -eq 0 ]] || fail "agent blocked report exited $code: $(cat "$TMP/cap-agent-report.out")"
[[ $(grep -c "already correct for" "$SCOPE/ops-argv.log" || true) -eq 0 ]] || fail "driver overrode the agent's own report"
jq -e '.iterations[0].reconsolidated == true and .iterations[0].outcome == "blocked"' "$CAP_STATE/operator.json" >/dev/null \
  || fail "agent blocked report was not reconsolidated as blocked"
jq -e '.outcome == "blocked"' "$CAP_STATE/processed.json" >/dev/null || fail "agent blocked report did not process the input"
[[ ! -e $CAP_STATE/failure-budget.json ]] || fail "agent blocked report created a failure budget"
# A genuine semantic blocked outcome is not an operational park: it records
# semantic-blocked and writes no park record.
jq -e '.kind == "semantic-blocked"' "$CAP_STATE/blocker.json" >/dev/null \
  || fail "semantic blocked outcome did not record semantic-blocked: $(cat "$CAP_STATE/blocker.json")"
[[ ! -e $CAP_STATE/operational-park.json ]] || fail "semantic blocked outcome wrote the park sidecar"

# Regression: an unchanged semantic blocked input is already processed, so the
# driver exits without starting an iteration or relaunching the agent.
make_scope
target=$(git -C "$SCOPE/worktree" rev-parse HEAD)
set_target "$target"
write_child "$PRA" false obs-a "$target" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$target" obs-b ready "2026-09-01T00:01:00Z"
jq -n '{version:1,input_fingerprint:"fp-cap",outcome:"blocked",processed_at:"2026-09-01T00:00:00.000000Z"}' \
  >"$CAP_STATE/processed.json"
write_fake_omp ''
code=$(run_cap_code 1000 "$target" "$TMP/cap-unchanged.out")
[[ $code -eq 0 ]] || fail "unchanged blocked input exited $code: $(cat "$TMP/cap-unchanged.out")"
[[ ! -e $SCOPE/agent-calls ]] || fail "unchanged blocked input relaunched the agent"
[[ ! -e $CAP_STATE/operator.json ]] || fail "unchanged blocked input started an iteration"
jq -e '.outcome == "blocked"' "$CAP_STATE/processed.json" >/dev/null || fail "unchanged blocked input rewrote processed state"

# Long capability branch: automation report rejects a headline over 80
# characters, so a branch that does not fit must not degrade the verified no-op
# into a park. The driver must still self-report, reconsolidate, and record a
# headline inside the limit.
make_scope
long_branch="cap/hindsight-$(printf 'x%.0s' {1..31})"
[[ ${#long_branch} -eq 45 ]] || fail "long branch fixture is not 45 characters"
git -C "$SCOPE/worktree" branch -m "$long_branch"
git -C "$SCOPE/worktree" push -q fork "$long_branch"
CAPABILITY_BRANCH=$long_branch
target=$(git -C "$SCOPE/worktree" rev-parse HEAD)
set_target "$target"
write_child "$PRA" false obs-a "$target" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$target" obs-b ready "2026-09-01T00:01:00Z"
write_fake_omp ''
code=$(run_cap_code 1000 "$target" "$TMP/cap-long-branch.out")
[[ $code -eq 0 ]] || fail "long branch no-op exited $code: $(cat "$TMP/cap-long-branch.out")"
grep -q "automation_report is required" "$TMP/cap-long-branch.out" && fail "long branch no-op took the contract-failure path"
jq -e '.iterations[0].reconsolidated == true' "$CAP_STATE/operator.json" >/dev/null \
  || fail "long branch no-op did not reconsolidate"
jq -e '.outcome == "ready"' "$CAP_STATE/processed.json" >/dev/null \
  || fail "long branch no-op did not process the input as ready"
[[ ! -e $CAP_STATE/failure-budget.json ]] || fail "long branch no-op created a failure budget"
long_headline=$(sed -n 's/^.*--headline \(.*\) --summary .*$/\1/p' "$SCOPE/ops-argv.log")
[[ -n $long_headline ]] || fail "long branch no-op did not record a headline"
[[ ${#long_headline} -le 80 ]] || fail "long branch headline is ${#long_headline} characters: $long_headline"
[[ $long_headline == *"verified no-op"* ]] || fail "long branch headline lost the verified no-op meaning: $long_headline"
[[ $long_headline == *"${target:0:8}"* ]] || fail "long branch headline does not name the target generation: $long_headline"

# Rejected self-report: a driver-emitted report the operator surface refuses
# must not park silently. The run still takes the existing contract-failure
# path, and the rejection is diagnosable on stderr.
make_scope
target=$(git -C "$SCOPE/worktree" rev-parse HEAD)
set_target "$target"
write_child "$PRA" false obs-a "$target" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$target" obs-b ready "2026-09-01T00:01:00Z"
write_fake_omp ''
set +e
PROOF_REJECT_REPORT=1 run_cap 1000 "$target" >"$TMP/cap-reject.out" 2>"$TMP/cap-reject.err"
code=$?
set -e
[[ $code -eq 3 ]] || fail "rejected self-report exited $code, want 3: $(cat "$TMP/cap-reject.err")"
grep -q "automation_report is required" "$TMP/cap-reject.err" || fail "rejected self-report did not hit the contract failure"
[[ $(grep -c "already correct for" "$SCOPE/ops-argv.log" || true) -eq 1 ]] || fail "rejected self-report did not attempt the driver report"
grep -q "verified no-op self-report was rejected" "$TMP/cap-reject.err" \
  || fail "rejected self-report did not name the failure on stderr: $(cat "$TMP/cap-reject.err")"
grep -q "proof-forced report rejection" "$TMP/cap-reject.err" \
  || fail "rejected self-report did not echo the operator error to stderr: $(cat "$TMP/cap-reject.err")"
grep -q "proof-forced report rejection" "$TMP/cap-reject.out" \
  && fail "rejected self-report wrote the operator error to stdout"
jq -e '.iterations[0].reconsolidated == false' "$CAP_STATE/operator.json" >/dev/null \
  || fail "rejected self-report reconsolidated"
jq -e '.failures == 1' "$CAP_STATE/failure-budget.json" >/dev/null \
  || fail "rejected self-report did not record the contract failure"
[[ ! -e $CAP_STATE/processed.json ]] || fail "rejected self-report advanced processed state"

# Operational park: an operational failure does not certify the input (commit
# d2673ad), so the retry is bounded by a sidecar holding the fingerprint the
# park covered. The first failure is retried, the second parks, and a later tick
# with the same unchanged input burns no pass, writes no processed state and
# clears nothing. A different fingerprint is a new input and runs normally.
make_scope
target=$(git -C "$SCOPE/worktree" rev-parse HEAD)
set_target "$target"
write_child "$PRA" false obs-a "$target" obs-a blocked "2026-09-01T00:01:00Z"
write_child "$PRB" false obs-b "$target" obs-b ready "2026-09-01T00:01:00Z"
write_fake_omp 'exit 7'
code=$(run_cap_code 1000 "$target" "$TMP/cap-park-first.out")
[[ $code -eq 7 ]] || fail "first operational failure exited $code, want 7: $(cat "$TMP/cap-park-first.out")"
[[ ! -e $CAP_STATE/operational-park.json ]] || fail "first operational failure wrote the park sidecar"
[[ ! -e $CAP_STATE/processed.json ]] || fail "first operational failure certified the input"
code=$(run_cap_code 1000 "$target" "$TMP/cap-park-parked.out")
[[ $code -eq 0 ]] || fail "exhausted budget exited $code, want the park: $(cat "$TMP/cap-park-parked.out")"
jq -e '.input_fingerprint == "fp-cap"' "$CAP_STATE/operational-park.json" >/dev/null \
  || fail "park did not record the parked fingerprint"
jq -e '.failures == 2 and .input_fingerprint == "fp-cap"' "$CAP_STATE/failure-budget.json" >/dev/null \
  || fail "park did not leave the budget on the parked input"
jq -e '.kind == "iteration-failed"
    and (.summary | contains("parked after 2 identical crash failures"))' \
  "$CAP_STATE/blocker.json" >/dev/null \
  || fail "park did not record the blocker as iteration-failed: $(cat "$CAP_STATE/blocker.json")"

starts_before=$(grep -c 'operator iteration-start' "$SCOPE/ops-argv.log" || true)
cp "$CAP_STATE/failure-budget.json" "$TMP/cap-park-budget.json"
cp "$CAP_STATE/blocker.json" "$TMP/cap-park-blocker.json"
code=$(run_cap_code 1000 "$target" "$TMP/cap-park-skip.out")
[[ $code -eq 0 ]] || fail "unchanged parked input exited $code: $(cat "$TMP/cap-park-skip.out")"
[[ $(grep -c 'operator iteration-start' "$SCOPE/ops-argv.log" || true) -eq "$starts_before" ]] \
  || fail "unchanged parked input started an iteration"
[[ $(grep -c called "$SCOPE/agent-calls" || true) -eq 2 ]] || fail "unchanged parked input burned a pass"
[[ ! -e $CAP_STATE/processed.json ]] || fail "the park skip certified the input as processed"
cmp -s "$TMP/cap-park-budget.json" "$CAP_STATE/failure-budget.json" \
  || fail "the park skip rewrote the failure budget"
cmp -s "$TMP/cap-park-blocker.json" "$CAP_STATE/blocker.json" \
  || fail "the park skip rewrote the blocker"
[[ $(grep -c "already correct for" "$SCOPE/ops-argv.log" || true) -eq 0 ]] \
  || fail "the park skip self-reported READY"

# A different fingerprint is a new input, so it runs. A failure that is not a
# park leaves the park record alone.
export PROOF_FINGERPRINT=fp-cap-later
code=$(run_cap_code 1000 "$target" "$TMP/cap-park-next.out")
[[ $code -eq 7 ]] || fail "changed input exited $code, want a fresh pass failure: $(cat "$TMP/cap-park-next.out")"
[[ $(grep -c called "$SCOPE/agent-calls" || true) -eq 3 ]] || fail "changed input did not run a pass"
jq -e '.input_fingerprint == "fp-cap"' "$CAP_STATE/operational-park.json" >/dev/null \
  || fail "a failed attempt rewrote the park record"

# A new input that reaches READY still runs, and clearing the budget clears the
# stale park record with it: a recovered input is not shadowed.
write_fake_omp ''
code=$(run_cap_code 1000 "$target" "$TMP/cap-park-recovered.out")
unset PROOF_FINGERPRINT
[[ $code -eq 0 ]] || fail "recovered input exited $code: $(cat "$TMP/cap-park-recovered.out")"
jq -e '.outcome == "ready"' "$CAP_STATE/processed.json" >/dev/null || fail "recovered input did not process as ready"
[[ ! -e $CAP_STATE/operational-park.json ]] || fail "recovered input left the stale park record"
[[ ! -e $CAP_STATE/failure-budget.json ]] || fail "recovered input left the stale failure budget"

echo "capability-release-run ok"
