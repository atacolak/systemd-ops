#!/usr/bin/env bash
# Capability runner: release barrier, 5m quiet, BLOCKED PR settled, no OMP.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
SOURCE_DRIVER=${SOURCE_DRIVER:-/home/sf/workspace/oh-my-pi/.systemd-ops/drivers/capability-run}
SOURCE_LIB=${SOURCE_LIB:-/home/sf/workspace/oh-my-pi/.systemd-ops/lib/automation-wrapper}
SETTLE_LIB=${SETTLE_LIB:-/home/sf/workspace/oh-my-pi/.systemd-ops/lib/pr-settlement}
REVIEW_LIB=${REVIEW_LIB:-/home/sf/workspace/oh-my-pi/.systemd-ops/lib/pr-review-state}

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
exec "\${SYSTEMD_OPS_SCOPE_ROOT}/.systemd-ops/drivers/capability-run" cap/hindsight $CAP "\${WORKTREE}"
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
if [[ "\$joined" == *" automation context "* ]]; then
  cat "$SCOPE/context.json"
  exit 0
fi
if [[ "\$joined" == *" automation observe "* ]]; then
  jq -n --arg fp "\${PROOF_FINGERPRINT:-fp-cap}" --arg gen "\${UPSTREAM_GENERATION:-generation-G2}" \
    '{ok:true,data:{observation:{input_fingerprint:\$fp,generation:\$gen}}}'
  exit 0
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
print(json.dumps({"ok": True, "data": {"relations": {"children": children, "parent": None}}}))
PY
}

run_cap() {
  local now=$1
  export SCOPE CAP
  export CHILD_UNITS="$PRA $PRB"
  write_context >"$SCOPE/context.json"
  WORKTREE="$SCOPE/worktree" \
  SYSTEMD_OPS_SCOPE_ROOT="$SCOPE" \
  SYSTEMD_OPS_BIN="$SCOPE/fake-ops" \
  OMP_BIN="$SCOPE/fake-omp" \
  AGENT_CWD="$SCOPE/agents" \
  UPSTREAM_GENERATION="$G2" \
  PR_SETTLE_QUIET_SECONDS=300 \
  PR_SETTLE_NOW="$now" \
  "$SCOPE/.systemd-ops/operations/$CAP/run"
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

echo "capability-release-run ok"
