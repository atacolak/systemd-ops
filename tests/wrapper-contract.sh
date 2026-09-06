#!/usr/bin/env bash
# Dogfood wrapper success and structured-state proofs. No systemd mutation.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
DOGFOOD_ROOT=${DOGFOOD_ROOT:-$ROOT/dogfood}
SOURCE_DRIVER=${SOURCE_DRIVER:-$DOGFOOD_ROOT/drivers/pr-run}
SOURCE_LIB=${SOURCE_LIB:-$DOGFOOD_ROOT/lib/automation-wrapper}
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
  mkdir -p "$home/state" "$scope/worktree" "$scope/fork.git" "$scope/.systemd-ops/drivers" "$scope/.systemd-ops/lib"
  git -C "$scope/fork.git" init -q --bare
  git -C "$scope/worktree" init -q
  git -C "$scope/worktree" config user.email proof@example.invalid
  git -C "$scope/worktree" config user.name proof
  git -C "$scope/worktree" checkout -q -b fix/settings-project-scope
  touch "$scope/worktree/.proof"
  git -C "$scope/worktree" add .proof
  git -C "$scope/worktree" commit -qm proof
  git -C "$scope/worktree" remote add fork "$scope/fork.git"
  git -C "$scope/worktree" push -q fork fix/settings-project-scope
  git -C "$scope/worktree" fetch -q fork
  cat >"$home/run" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
exec "${SYSTEMD_OPS_SCOPE_ROOT}/.systemd-ops/drivers/pr-run" 9363 fix/settings-project-scope managed-omp-pr-9363 "${WORKTREE}"
EOF
  chmod +x "$home/run"
  cp "$SOURCE_DRIVER" "$scope/.systemd-ops/drivers/pr-run"
  chmod +x "$scope/.systemd-ops/drivers/pr-run"
  cp "$SOURCE_LIB" "$scope/.systemd-ops/lib/automation-wrapper"
  cp "$DOGFOOD_ROOT/lib/pr-attempt" "$scope/.systemd-ops/lib/pr-attempt"
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
world=${PROOF_FINGERPRINT:-fp-proof}
if [[ -n "${SYSTEMD_OPS_SCOPE_ROOT:-}" && -f "${SYSTEMD_OPS_SCOPE_ROOT}/world" ]]; then
  world=$(cat "${SYSTEMD_OPS_SCOPE_ROOT}/world")
fi
jq -cn --arg world "$world" --arg generation "${UPSTREAM_GENERATION:-deadbeef}" \
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
  PR_ATTEMPT_ROOT="$scope/attempts" \
  GIT_BASE="$scope/worktree" \
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

blocked_repeat=$(make_case blocked-repeat)
write_omp "$blocked_repeat/fake-omp" '"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "blocked unchanged" --summary '\''["still blocked"]'\'' --outcome blocked --route parent >/dev/null; echo called >>"${SYSTEMD_OPS_SCOPE_ROOT}/agent-calls"'
run_wrapper "$blocked_repeat"
[[ -f $blocked_repeat/agent-calls ]] || fail "blocked first pass did not invoke OMP"
[[ $(wc -l <"$blocked_repeat/agent-calls") -eq 1 ]] || fail "blocked first pass invoked OMP more than once"
jq -e '.outcome=="blocked"' "$blocked_repeat/.systemd-ops/operations/managed-omp-pr-9363/state/processed.json" >/dev/null \
  || fail "blocked first pass did not process input"
for _ in 1 2 3; do
  run_wrapper "$blocked_repeat"
done
[[ $(wc -l <"$blocked_repeat/agent-calls") -eq 1 ]] || fail "unchanged blocked polls relaunched OMP"

PROOF_FINGERPRINT=fp-changed run_wrapper "$blocked_repeat"
[[ $(wc -l <"$blocked_repeat/agent-calls") -eq 2 ]] || fail "changed world did not wake OMP exactly once"

mut=$(make_case self-mutation)
pre=$(SYSTEMD_OPS_SCOPE_ROOT="$mut" SYSTEMD_OPS_OPERATION=managed-omp-pr-9363 "$BIN" --json --manager user --cwd "$mut/worktree" automation observe)
pre_fp=$(jq -er '.data.observation.input_fingerprint' <<<"$pre")
write_omp "$mut/fake-omp" 'printf fp-after >"${SYSTEMD_OPS_SCOPE_ROOT}/world"
"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "mutated" --summary '\''["self mutation"]'\'' --outcome ready >/dev/null'
run_wrapper "$mut"
post=$(SYSTEMD_OPS_SCOPE_ROOT="$mut" SYSTEMD_OPS_OPERATION=managed-omp-pr-9363 "$BIN" --json --manager user --cwd "$mut/worktree" automation observe)
post_fp=$(jq -er '.data.observation.input_fingerprint' <<<"$post")
[[ $pre_fp != "$post_fp" ]] || fail "self-mutation did not change observation"
[[ $(processed_fp "$mut") == "$post_fp" ]] || fail "processed certified pre-pass input $pre_fp instead of $post_fp"
jq -e --arg fp "$post_fp" '.input_fingerprint==$fp' "$mut/.systemd-ops/operations/managed-omp-pr-9363/state/checkpoint.json" >/dev/null \
  || fail "checkpoint certified pre-pass input"
write_omp "$mut/fake-omp" 'touch "${SYSTEMD_OPS_SCOPE_ROOT}/agent-was-called"'
run_wrapper "$mut"
[[ ! -e $mut/agent-was-called ]] || fail "post-pass processed input invoked OMP again"

echo "wrapper-contract ok"


