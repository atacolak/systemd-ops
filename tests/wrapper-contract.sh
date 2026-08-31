#!/usr/bin/env bash
# Dogfood wrapper success and structured-state proofs. No systemd mutation.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
SOURCE_DRIVER=${SOURCE_DRIVER:-/home/sf/workspace/oh-my-pi/.systemd-ops/drivers/pr-run}
SOURCE_LIB=${SOURCE_LIB:-/home/sf/workspace/oh-my-pi/.systemd-ops/lib/automation-wrapper}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -x $BIN ]] || fail "missing $BIN"
[[ -x $SOURCE_DRIVER ]] || fail "missing driver $SOURCE_DRIVER"
[[ -r $SOURCE_LIB ]] || fail "missing wrapper $SOURCE_LIB"

make_case() {
  local name=$1
  local scope=$TMP/$name
  local home=$scope/.systemd-ops/operations/managed-omp-pr-9363
  mkdir -p "$home/state" "$scope/worktree" "$scope/.systemd-ops/drivers" "$scope/.systemd-ops/lib"
  git -C "$scope/worktree" init -q
  git -C "$scope/worktree" config user.email proof@example.invalid
  git -C "$scope/worktree" config user.name proof
  git -C "$scope/worktree" checkout -q -b fix/settings-project-scope
  touch "$scope/worktree/.proof"
  git -C "$scope/worktree" add .proof
  git -C "$scope/worktree" commit -qm proof
  git -C "$scope/worktree" remote add fork "$scope/worktree"
  cat >"$home/run" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
exec "${SYSTEMD_OPS_SCOPE_ROOT}/.systemd-ops/drivers/pr-run" 9363 fix/settings-project-scope managed-omp-pr-9363 "${WORKTREE}"
EOF
  chmod +x "$home/run"
  cp "$SOURCE_DRIVER" "$scope/.systemd-ops/drivers/pr-run"
  chmod +x "$scope/.systemd-ops/drivers/pr-run"
  cp "$SOURCE_LIB" "$scope/.systemd-ops/lib/automation-wrapper"
  chmod +x "$scope/.systemd-ops/lib/automation-wrapper"
  cat >"$scope/.systemd-ops/scope.toml" <<EOF
[scope]
id = "omp-proof"
owned = ["managed-omp-*"]

[automation]
agent_root = "$scope/agents"
EOF
  mkdir -p "$scope/agents/.omp/agents"
  cat >"$scope/agents/.omp/agents/pr-maintainer.md" <<'EOF'
---
name: pr-maintainer
description: proof agent
hide: true
tools: [automation_context, automation_report]
---
proof
EOF
  cat >"$home/automation.toml" <<'EOF'
version = 1
agent = "pr-maintainer"
brain_paths = [".systemd-ops/drivers/pr-observe", ".systemd-ops/drivers/pr-run", ".systemd-ops/lib/automation-wrapper"]
output_revision_required = true

[observation]
exec = "drivers/pr-observe"
args = ["9363"]
EOF
  cat >"$scope/.systemd-ops/drivers/pr-observe" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
jq -cn --arg world "${PROOF_FINGERPRINT:-fp-proof}" --arg generation "${UPSTREAM_GENERATION:-deadbeef}" \
  '{version:1,world_fingerprint:$world,generation:$generation}'
EOF
  chmod +x "$scope/.systemd-ops/drivers/pr-observe"
  printf '%s\n' "$scope"
}

write_omp() {
  local path="$1"
  local body="$2"
  cat >"$path" <<EOF
#!/usr/bin/env bash
set -euo pipefail
$body
EOF
  chmod +x "$path"
}


run_wrapper() {
  local scope=$1
  WORKTREE="$scope/worktree" \
  SYSTEMD_OPS_SCOPE_ROOT="$scope" \
  SYSTEMD_OPS_BIN="$BIN" \
  OMP_BIN="$scope/fake-omp" \
  AGENT_CWD="$scope/agents" \
  UPSTREAM_GENERATION="${UPSTREAM_GENERATION:-deadbeef}" \
  PROOF_SKIP_GH=1 \
  "$scope/.systemd-ops/operations/managed-omp-pr-9363/run"
}

processed_fp() {
  jq -er '.input_fingerprint' "$1/.systemd-ops/operations/managed-omp-pr-9363/state/processed.json"
}

success=$(make_case success)
write_omp "$success/fake-omp" '"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "proof current" --summary '\''["report accepted"]'\'' --outcome ready >/dev/null'
run_wrapper "$success"
success_fp=$(processed_fp "$success")
[[ $success_fp =~ ^[0-9a-f]{64}$ ]] || fail "success did not write processed input"
jq -e '.iterations[0].reconsolidated == true' "$success/.systemd-ops/operations/managed-omp-pr-9363/state/operator.json" >/dev/null || fail "success did not reconsolidate"
jq -e '.version==2 and .generation=="deadbeef" and (.output_revision|length==40)' \
  "$success/.systemd-ops/operations/managed-omp-pr-9363/state/checkpoint.json" >/dev/null \
  || fail "success did not write structured checkpoint"

missing=$(make_case missing-report)
write_omp "$missing/fake-omp" 'exit 0'
set +e
run_wrapper "$missing" >/dev/null 2>&1
code=$?
set -e
[[ $code -eq 3 ]] || fail "missing report exited $code, want 3"
[[ ! -e $missing/.systemd-ops/operations/managed-omp-pr-9363/state/processed.json ]] || fail "missing report advanced processed state"
[[ ! -e $missing/.systemd-ops/operations/managed-omp-pr-9363/state/checkpoint.json ]] || fail "missing report wrote checkpoint"
jq -e '.iterations[0].reconsolidated == false' "$missing/.systemd-ops/operations/managed-omp-pr-9363/state/operator.json" >/dev/null || fail "missing report was reconsolidated"

omp_failure=$(make_case omp-failure)
write_omp "$omp_failure/fake-omp" 'exit 7'
set +e
run_wrapper "$omp_failure" >/dev/null 2>&1
code=$?
set -e
[[ $code -eq 7 ]] || fail "OMP failure exited $code, want 7"
[[ ! -e $omp_failure/.systemd-ops/operations/managed-omp-pr-9363/state/processed.json ]] || fail "OMP failure advanced processed state"
[[ ! -e $omp_failure/.systemd-ops/operations/managed-omp-pr-9363/state/checkpoint.json ]] || fail "OMP failure wrote checkpoint"
write_omp "$omp_failure/fake-omp" '"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "retry current" --summary '\''["retry accepted"]'\'' --outcome ready >/dev/null'
run_wrapper "$omp_failure"
[[ $(processed_fp "$omp_failure") =~ ^[0-9a-f]{64}$ ]] || fail "failed iteration was not retried"
retry_count=$(jq '[.iterations[] | select(.headline == "retry current" and .reconsolidated == true)] | length' "$omp_failure/.systemd-ops/operations/managed-omp-pr-9363/state/operator.json")
[[ $retry_count -eq 1 ]] || fail "retry did not reconsolidate exactly once"

finish_failure=$(make_case finish-failure)
write_omp "$finish_failure/fake-omp" '"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "proof current" --summary '\''["report accepted"]'\'' --outcome ready >/dev/null
"$SYSTEMD_OPS_BIN" --json --manager user operator iteration-start --unit "$SYSTEMD_OPS_OPERATION" >/dev/null'
set +e
run_wrapper "$finish_failure" >/dev/null 2>&1
code=$?
set -e
[[ $code -ne 0 ]] || fail "finish failure succeeded"
[[ ! -e $finish_failure/.systemd-ops/operations/managed-omp-pr-9363/state/processed.json ]] || fail "finish failure advanced processed state"

unchanged=$(make_case unchanged)
observe=$(SYSTEMD_OPS_SCOPE_ROOT="$unchanged" SYSTEMD_OPS_OPERATION=managed-omp-pr-9363 "$BIN" --json --manager user --cwd "$unchanged/worktree" automation observe)
input=$(jq -er '.data.observation.input_fingerprint' <<<"$observe")
SYSTEMD_OPS_SCOPE_ROOT="$unchanged" SYSTEMD_OPS_OPERATION=managed-omp-pr-9363 "$BIN" --json --manager user --cwd "$unchanged/worktree" \
  automation process --input-fingerprint "$input" --outcome ready >/dev/null
write_omp "$unchanged/fake-omp" 'touch "${SYSTEMD_OPS_SCOPE_ROOT}/agent-was-called"'
run_wrapper "$unchanged"
[[ ! -e $unchanged/agent-was-called ]] || fail "unchanged processed input invoked OMP"
[[ ! -e $unchanged/.systemd-ops/operations/managed-omp-pr-9363/state/operator.json ]] || fail "unchanged processed input created an iteration"

echo "wrapper-contract ok"
