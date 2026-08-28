#!/usr/bin/env bash
# Dogfood wrapper success and fingerprint proofs. No systemd mutation.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
SOURCE_DRIVER=${SOURCE_DRIVER:-/home/sf/workspace/oh-my-pi/.systemd-ops/pr-maintainer-run}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -x $BIN ]] || fail "missing $BIN"
[[ -x $SOURCE_DRIVER ]] || fail "missing driver $SOURCE_DRIVER"

make_case() {
  local name=$1
  local scope=$TMP/$name
  local home=$scope/.systemd-ops/managed-omp-pr-9363
  mkdir -p "$home/state" "$scope/worktree"
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
exec "${SYSTEMD_OPS_SCOPE_ROOT}/.systemd-ops/pr-maintainer-run" 9363 fix/settings-project-scope managed-omp-pr-9363 "${WORKTREE}"
EOF
  chmod +x "$home/run"
  cp "$SOURCE_DRIVER" "$scope/.systemd-ops/pr-maintainer-run"
  chmod +x "$scope/.systemd-ops/pr-maintainer-run"
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
brain_paths = [".systemd-ops/pr-fingerprint", ".systemd-ops/pr-maintainer-run"]
EOF
  cat >"$scope/.systemd-ops/pr-fingerprint" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "${PROOF_FINGERPRINT:-fp-proof}"
EOF
  chmod +x "$scope/.systemd-ops/pr-fingerprint"
  printf '%s\n' "$scope"
}

write_omp() {
  local path=$1
  local body=$2
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
  FINGERPRINT_TOOL="$scope/.systemd-ops/pr-fingerprint" \
  OMP_BIN="$scope/fake-omp" \
  AGENT_CWD="$scope/agents" \
  "$scope/.systemd-ops/managed-omp-pr-9363/run"
}

success=$(make_case success)
write_omp "$success/fake-omp" '"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "proof current" --summary '\''["report accepted"]'\'' >/dev/null'
run_wrapper "$success"
success_fp=$(cat "$success/.systemd-ops/managed-omp-pr-9363/state/fingerprint")
[[ $success_fp =~ ^[0-9a-f]{64}$ ]] || fail "success did not write combined fingerprint"
jq -e '.iterations[0].reconsolidated == true' "$success/.systemd-ops/managed-omp-pr-9363/state/operator.json" >/dev/null || fail "success did not reconsolidate"


missing=$(make_case missing-report)
write_omp "$missing/fake-omp" 'exit 0'
set +e
run_wrapper "$missing" >/dev/null 2>&1
code=$?
set -e
[[ $code -eq 3 ]] || fail "missing report exited $code, want 3"
[[ ! -e $missing/.systemd-ops/managed-omp-pr-9363/state/fingerprint ]] || fail "missing report advanced fingerprint"
jq -e '.iterations[0].reconsolidated == false' "$missing/.systemd-ops/managed-omp-pr-9363/state/operator.json" >/dev/null || fail "missing report was reconsolidated"

omp_failure=$(make_case omp-failure)
write_omp "$omp_failure/fake-omp" 'exit 7'
set +e
run_wrapper "$omp_failure" >/dev/null 2>&1
code=$?
set -e
[[ $code -eq 7 ]] || fail "OMP failure exited $code, want 7"
[[ ! -e $omp_failure/.systemd-ops/managed-omp-pr-9363/state/fingerprint ]] || fail "OMP failure advanced fingerprint"
write_omp "$omp_failure/fake-omp" '"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "retry current" --summary '\''["retry accepted"]'\'' >/dev/null'
run_wrapper "$omp_failure"
[[ $(cat "$omp_failure/.systemd-ops/managed-omp-pr-9363/state/fingerprint") =~ ^[0-9a-f]{64}$ ]] || fail "failed iteration was not retried"
retry_count=$(jq '[.iterations[] | select(.headline == "retry current" and .reconsolidated == true)] | length' "$omp_failure/.systemd-ops/managed-omp-pr-9363/state/operator.json")
[[ $retry_count -eq 1 ]] || fail "retry did not reconsolidate exactly once"

finish_failure=$(make_case finish-failure)
write_omp "$finish_failure/fake-omp" '"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "proof current" --summary '\''["report accepted"]'\'' >/dev/null
"$SYSTEMD_OPS_BIN" --json --manager user operator iteration-start --unit "$SYSTEMD_OPS_OPERATION" >/dev/null'
set +e
run_wrapper "$finish_failure" >/dev/null 2>&1
code=$?
set -e
[[ $code -ne 0 ]] || fail "finish failure succeeded"
[[ ! -e $finish_failure/.systemd-ops/managed-omp-pr-9363/state/fingerprint ]] || fail "finish failure advanced fingerprint"

unchanged=$(make_case unchanged)
external_fp=$("$unchanged/.systemd-ops/pr-fingerprint" 9363)
brain_revision=$(SYSTEMD_OPS_SCOPE_ROOT="$unchanged" "$BIN" --json --manager user --cwd "$unchanged/worktree" automation revision --unit managed-omp-pr-9363 | jq -er '.data.brain_revision')
printf 'external=%s\nbrain=%s\n' "$external_fp" "$brain_revision" | sha256sum | cut -d' ' -f1 >"$unchanged/.systemd-ops/managed-omp-pr-9363/state/fingerprint"
write_omp "$unchanged/fake-omp" 'touch "${SYSTEMD_OPS_SCOPE_ROOT}/agent-was-called"'
run_wrapper "$unchanged"
[[ ! -e $unchanged/agent-was-called ]] || fail "unchanged fingerprint invoked OMP"
[[ ! -e $unchanged/.systemd-ops/managed-omp-pr-9363/state/operator.json ]] || fail "unchanged fingerprint created an iteration"

echo "wrapper-contract ok"
