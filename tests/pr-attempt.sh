#!/usr/bin/env bash
# Disposable PR attempt worktrees, dirty-exit reject, fresh retry, parking.
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
[[ -x $SOURCE_DRIVER ]] || fail "missing $SOURCE_DRIVER"
[[ -r $SOURCE_LIB ]] || fail "missing $SOURCE_LIB"

STEM=managed-omp-pr-9363
BRANCH=fix/settings-project-scope

make_scope() {
  local name=$1
  local scope=$TMP/$name
  rm -rf "$scope"
  mkdir -p "$scope/.systemd-ops/operations/$STEM/state" "$scope/.systemd-ops/drivers" "$scope/.systemd-ops/lib" \
    "$scope/user-worktree" "$scope/fork.git" "$scope/agents/.omp/agents" "$scope/attempts"
  git -C "$scope/fork.git" init -q --bare
  git -C "$scope/user-worktree" init -q
  git -C "$scope/user-worktree" config user.email proof@example.invalid
  git -C "$scope/user-worktree" config user.name proof
  git -C "$scope/user-worktree" checkout -q -b "$BRANCH"
  echo original >"$scope/user-worktree/keep.txt"
  git -C "$scope/user-worktree" add keep.txt
  git -C "$scope/user-worktree" commit -qm original
  echo dirty-user >"$scope/user-worktree/user-dirty.txt"
  git -C "$scope/user-worktree" remote add fork "$scope/fork.git"
  git -C "$scope/user-worktree" push -q fork "$BRANCH"
  git -C "$scope/user-worktree" fetch -q fork
  cat >"$scope/.systemd-ops/scope.toml" <<EOF
[scope]
id = "omp-proof"
owned = ["managed-omp-*"]
[automation]
agent_root = "$scope/agents"
EOF
  cat >"$scope/agents/.omp/agents/pr-maintainer.md" <<'EOF'
---
name: pr-maintainer
description: proof
hide: true
tools: [automation_context, automation_report]
---
proof
EOF
  cat >"$scope/.systemd-ops/operations/$STEM/automation.toml" <<EOF
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
  cp "$SOURCE_DRIVER" "$scope/.systemd-ops/drivers/pr-run"
  chmod +x "$scope/.systemd-ops/drivers/pr-run"
  cp "$SOURCE_LIB" "$scope/.systemd-ops/lib/automation-wrapper"
  cp "$DOGFOOD_ROOT/lib/pr-attempt" "$scope/.systemd-ops/lib/pr-attempt"
  cat >"$scope/.systemd-ops/operations/$STEM/run" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec "\${SYSTEMD_OPS_SCOPE_ROOT}/.systemd-ops/drivers/pr-run" 9363 $BRANCH $STEM "\${WORKTREE}"
EOF
  chmod +x "$scope/.systemd-ops/operations/$STEM/run"
  printf '%s\n' "$scope"
}

write_omp() {
  local path=$1 body=$2
  cat >"$path" <<EOF
#!/usr/bin/env bash
set -euo pipefail
args=("\$@")
cwd=""
i=0
while (( i < \${#args[@]} )); do
  if [[ "\${args[\$i]}" == --cwd ]]; then
    cwd=\${args[\$((i+1))]}
  fi
  i=\$((i+1))
done
[[ -n "\$cwd" ]] && cd "\$cwd"
$body
EOF
  chmod +x "$path"
}

run_pr() {
  local scope=$1
  WORKTREE="$scope/user-worktree" \
  SYSTEMD_OPS_SCOPE_ROOT="$scope" \
  SYSTEMD_OPS_BIN="$BIN" \
  OMP_BIN="$scope/fake-omp" \
  AGENT_CWD="$scope/agents" \
  UPSTREAM_GENERATION="${UPSTREAM_GENERATION:-deadbeef}" \
  PR_ATTEMPT_ROOT="$scope/attempts" \
  GIT_BASE="$scope/user-worktree" \
  PROOF_SKIP_GH=1 \
  "$scope/.systemd-ops/operations/$STEM/run"
}

# --- success: attempt cwd, 45m, dispose, user dirty preserved ---
scope=$(make_scope success)
write_omp "$scope/fake-omp" 'printf "%s\n" "$@" >"${SYSTEMD_OPS_SCOPE_ROOT}/omp-argv"
pwd >"${SYSTEMD_OPS_SCOPE_ROOT}/omp-cwd"
git rev-parse HEAD >"${SYSTEMD_OPS_SCOPE_ROOT}/omp-head"
git status --porcelain=v1 >"${SYSTEMD_OPS_SCOPE_ROOT}/omp-dirty"
"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "proof current" --summary '\''["report accepted"]'\'' --outcome ready >/dev/null'
run_pr "$scope" >/tmp/pr-attempt-success.out 2>/tmp/pr-attempt-success.err || fail "success wrapper failed: $(cat /tmp/pr-attempt-success.err)"
grep -qx -- '--max-time' "$scope/omp-argv" && grep -qx 45m "$scope/omp-argv" || fail "PR command was not 45m: $(cat "$scope/omp-argv")"
attempt_cwd=$(cat "$scope/omp-cwd")
[[ $attempt_cwd == "$scope/attempts/$STEM/"* ]] || fail "attempt cwd was $attempt_cwd"
[[ $attempt_cwd != *recovered* ]] || fail "attempt used recovered path: $attempt_cwd"
[[ $attempt_cwd != "$scope/user-worktree" ]] || fail "attempt used the user worktree"
head_a=$(git -C "$scope/user-worktree" rev-parse HEAD)
[[ $(cat "$scope/omp-head") == "$head_a" ]] || fail "attempt did not start at fork head"
[[ ! -s $scope/omp-dirty ]] || fail "attempt was dirty at start: $(cat "$scope/omp-dirty")"
[[ ! -e $attempt_cwd ]] || fail "successful attempt worktree was not removed: $attempt_cwd"
[[ -f $scope/user-worktree/user-dirty.txt ]] || fail "user dirty worktree was destroyed"
[[ -f $scope/.systemd-ops/operations/$STEM/state/checkpoint.json ]] || fail "success did not checkpoint"
[[ $(find "$scope/attempts" -mindepth 2 -maxdepth 2 -type d 2>/dev/null | wc -l) -eq 0 ]] || fail "attempt leak after success"

# --- dirty READY rejected ---
scope=$(make_scope dirty-ready)
write_omp "$scope/fake-omp" 'echo leftover >foo.ts
"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "dirty ready" --summary '\''["claimed ready"]'\'' --outcome ready >/dev/null'
set +e
run_pr "$scope" >/tmp/pr-attempt-dirty.out 2>/tmp/pr-attempt-dirty.err
code=$?
set -e
[[ $code -ne 0 ]] || fail "dirty READY succeeded"
[[ ! -e $scope/.systemd-ops/operations/$STEM/state/checkpoint.json ]] || fail "dirty READY wrote checkpoint"
[[ ! -e $scope/.systemd-ops/operations/$STEM/state/processed.json ]] || fail "dirty READY marked processed"
grep -q foo.ts /tmp/pr-attempt-dirty.err || fail "dirty READY did not log dirty files"
[[ $(find "$scope/attempts" -mindepth 2 -maxdepth 2 -type d 2>/dev/null | wc -l) -eq 0 ]] || fail "dirty READY leaked attempt"
[[ -f $scope/user-worktree/user-dirty.txt ]] || fail "dirty READY destroyed user worktree"

# --- dirty BLOCKED rejected ---
scope=$(make_scope dirty-blocked)
write_omp "$scope/fake-omp" 'echo leftover >bar.ts
"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "dirty blocked" --summary '\''["claimed blocked"]'\'' --outcome blocked --route parent >/dev/null'
set +e
run_pr "$scope" >/tmp/pr-attempt-dblock.out 2>/tmp/pr-attempt-dblock.err
code=$?
set -e
[[ $code -ne 0 ]] || fail "dirty BLOCKED succeeded"
[[ ! -e $scope/.systemd-ops/operations/$STEM/state/processed.json ]] || fail "dirty BLOCKED marked processed"
grep -q bar.ts /tmp/pr-attempt-dblock.err || fail "dirty BLOCKED did not log dirty files"

# --- timeout: log, dispose, user preserved, retry from A ---
scope=$(make_scope timeout)
head_a=$(git -C "$scope/user-worktree" rev-parse HEAD)
write_omp "$scope/fake-omp" 'echo scratch >scratch.ts
pwd >"${SYSTEMD_OPS_SCOPE_ROOT}/timeout-cwd"
exit 124'
set +e
run_pr "$scope" >/tmp/pr-attempt-t1.out 2>/tmp/pr-attempt-t1.err
code=$?
set -e
[[ $code -eq 124 ]] || fail "timeout exited $code"
grep -q 'uncommitted attempt discarded' /tmp/pr-attempt-t1.err || fail "timeout did not log discard"
[[ ! -e $scope/.systemd-ops/operations/$STEM/state/processed.json ]] || fail "first timeout marked processed"
t1=$(cat "$scope/timeout-cwd")
[[ ! -e $t1 ]] || fail "timeout attempt was not removed"
[[ -f $scope/user-worktree/user-dirty.txt ]] || fail "timeout destroyed user worktree"
jq -e '.failures==1' "$scope/.systemd-ops/operations/$STEM/state/failure-budget.json" >/dev/null \
  || fail "first timeout did not record failure budget"

write_omp "$scope/fake-omp" 'pwd >"${SYSTEMD_OPS_SCOPE_ROOT}/timeout2-cwd"
git rev-parse HEAD >"${SYSTEMD_OPS_SCOPE_ROOT}/timeout2-head"
git status --porcelain=v1 >"${SYSTEMD_OPS_SCOPE_ROOT}/timeout2-dirty"
exit 124'
set +e
run_pr "$scope" >/tmp/pr-attempt-t2.out 2>/tmp/pr-attempt-t2.err
code=$?
set -e
[[ $(cat "$scope/timeout2-head") == "$head_a" ]] || fail "retry did not start from fork head A"
[[ ! -s $scope/timeout2-dirty ]] || fail "retry inherited attempt-1 dirt"
[[ $t1 != "$(cat "$scope/timeout2-cwd")" ]] || fail "retry reused attempt path"
jq -e '.failures==2' "$scope/.systemd-ops/operations/$STEM/state/failure-budget.json" >/dev/null \
  || fail "second timeout did not park budget at 2"
jq -e '.outcome=="blocked"' "$scope/.systemd-ops/operations/$STEM/state/processed.json" >/dev/null \
  || fail "second timeout did not park processed"
[[ ! -e $scope/.systemd-ops/operations/$STEM/state/checkpoint.json ]] || fail "parking wrote a READY checkpoint"

write_omp "$scope/fake-omp" 'echo called >>"${SYSTEMD_OPS_SCOPE_ROOT}/agent-calls"'
for _ in 1 2 3; do
  run_pr "$scope" >/dev/null 2>&1 || true
done
[[ ! -e $scope/agent-calls ]] || fail "parked identical input relaunched OMP"

printf y >"$scope/world"
write_omp "$scope/fake-omp" 'echo called-y >>"${SYSTEMD_OPS_SCOPE_ROOT}/agent-calls-y"
"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "new world" --summary '\''["ok"]'\'' --outcome ready >/dev/null'
run_pr "$scope" >/tmp/pr-attempt-y.out 2>/tmp/pr-attempt-y.err || fail "new world did not unpark: $(cat /tmp/pr-attempt-y.err)"
[[ -f $scope/agent-calls-y ]] || fail "new world did not invoke OMP"

# --- pushed commit is durable ---
scope=$(make_scope push-then-die)
write_omp "$scope/fake-omp" 'echo durable >keep.txt
git add keep.txt
git -c user.email=proof@example.invalid -c user.name=proof commit -qm pushed
git push -q fork HEAD:refs/heads/'"$BRANCH"'
exit 124'
set +e
run_pr "$scope" >/tmp/pr-attempt-push.out 2>/tmp/pr-attempt-push.err
set -e
head_b=$(git -C "$scope/user-worktree" rev-parse "fork/$BRANCH")
[[ $head_b != "$(git -C "$scope/user-worktree" rev-parse HEAD)" ]] || true
write_omp "$scope/fake-omp" 'git rev-parse HEAD >"${SYSTEMD_OPS_SCOPE_ROOT}/retry-head"
"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "from B" --summary '\''["ok"]'\'' --outcome ready >/dev/null'
run_pr "$scope" >/tmp/pr-attempt-fromb.out 2>/tmp/pr-attempt-fromb.err || fail "retry after push failed: $(cat /tmp/pr-attempt-fromb.err)"
[[ $(cat "$scope/retry-head") == "$head_b" ]] || fail "retry after push started at $(cat "$scope/retry-head") want $head_b"

echo "pr-attempt ok"
