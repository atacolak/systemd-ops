#!/usr/bin/env bash
# Dogfood wrapper success and fingerprint proofs. No systemd mutation.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
SOURCE_WRAPPER=${SOURCE_WRAPPER:-/home/sf/workspace/oh-my-pi/.systemd-ops/managed-omp-pr-9363/run}
SOURCE_DRIVER=${SOURCE_DRIVER:-/home/sf/workspace/oh-my-pi/.systemd-ops/pr-maintainer-run}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -x $BIN ]] || fail "missing $BIN"
[[ -x $SOURCE_WRAPPER ]] || fail "missing wrapper $SOURCE_WRAPPER"

make_case() {
  local name=$1
  local scope=$TMP/$name
  local home=$scope/.systemd-ops/managed-omp-pr-9363
  mkdir -p "$home/state" "$scope/worktree"
  cp "$SOURCE_WRAPPER" "$home/run"
  chmod +x "$home/run"
  cp "$SOURCE_DRIVER" "$scope/.systemd-ops/pr-maintainer-run"
  chmod +x "$scope/.systemd-ops/pr-maintainer-run"
  cat >"$scope/.systemd-ops/scope.toml" <<'EOF'
[scope]
id = "omp-proof"
owned = ["managed-omp-*"]
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
  SYSTEMD_OPS_BIN="$BIN" \
  FINGERPRINT_TOOL="$scope/.systemd-ops/pr-fingerprint" \
  OMP_BIN="$scope/fake-omp" \
  AGENT_CWD="$scope/agents" \
  "$scope/.systemd-ops/managed-omp-pr-9363/run"
}

success=$(make_case success)
write_omp "$success/fake-omp" '"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "proof current" --summary '\''["report accepted"]'\'' >/dev/null'
run_wrapper "$success"
[[ $(cat "$success/.systemd-ops/managed-omp-pr-9363/state/fingerprint") == fp-proof ]] || fail "success did not advance fingerprint"
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
[[ $(cat "$omp_failure/.systemd-ops/managed-omp-pr-9363/state/fingerprint") == fp-proof ]] || fail "failed iteration was not retried"
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
printf 'fp-proof\n' >"$unchanged/.systemd-ops/managed-omp-pr-9363/state/fingerprint"
write_omp "$unchanged/fake-omp" 'touch "${SYSTEMD_OPS_SCOPE_ROOT}/agent-was-called"'
run_wrapper "$unchanged"
[[ ! -e $unchanged/agent-was-called ]] || fail "unchanged fingerprint invoked OMP"
[[ ! -e $unchanged/.systemd-ops/managed-omp-pr-9363/state/operator.json ]] || fail "unchanged fingerprint created an iteration"

echo "wrapper-contract ok"
