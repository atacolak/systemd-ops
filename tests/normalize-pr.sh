#!/usr/bin/env bash
# Merged PR leaves the organism through plan-normalize-pr. Throwaway scope.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
TMP=$(mktemp -d)
SCOPE=$TMP/scope
PR1=managed-omp-pr-1
PR2=managed-omp-pr-2
HOME1=$SCOPE/.systemd-ops/operations/$PR1
HOME2=$SCOPE/.systemd-ops/operations/$PR2
UNIT_DIR=$TMP/xdg/systemd/user
AGENT=$TMP/agents
NOTIFY_LOG=$TMP/notify.argv
WT=$TMP/worktrees/pr-1
NOT_WT=$TMP/not-a-worktree
MERGE_SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
BAD_SHA=not-a-40-hex-merge-sha
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { jq -e '.ok == true' <<<"$1" >/dev/null || fail "expected ok: $1"; }
rejected() { jq -e '.ok == false' <<<"$1" >/dev/null || fail "expected rejection: $1"; }

command -v jq >/dev/null || fail "jq required"
command -v git >/dev/null || fail "git required"
[[ -x $BIN ]] || fail "missing $BIN"

LIVE_GENS=/home/sf/worktrees/omp/generations
LIVE_OMP=/home/sf/.bun/bin/omp
LIVE_OHMYPI=/home/sf/workspace/oh-my-pi
live_gens_before=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_before=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)
live_ohmy_before=$(
  find "$LIVE_OHMYPI" -type f -printf '%T@ %p\n' 2>/dev/null | sort | sha256sum | awk '{print $1}'
)

mkdir -p "$HOME1" "$HOME2" "$UNIT_DIR" "$SCOPE/.systemd-ops" "$AGENT/.omp/agents" "$TMP/bin" "$NOT_WT"
printf '%s\n' 'pr-maintainer role' >"$AGENT/.omp/agents/pr-maintainer.md"
printf '%s\n' stay >"$NOT_WT/sentinel"

cat >"$SCOPE/.systemd-ops/scope.toml" <<EOF
[scope]
id = "proof"
owned = ["managed-omp-pr-*"]

[automation]
agent_root = "$AGENT"
EOF

cat >"$HOME1/automation.toml" <<'EOF'
version = 1
agent = "pr-maintainer"
parent = "managed-omp-runtime-integrator"
EOF
cat >"$HOME2/automation.toml" <<'EOF'
version = 1
agent = "pr-maintainer"
parent = "managed-omp-runtime-integrator"
EOF

export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
git -C "$SCOPE" init >/dev/null
git -C "$SCOPE" config user.email "normalize-pr@example.test"
git -C "$SCOPE" config user.name "normalize-pr"
printf '%s\n' seed >"$SCOPE/README"
git -C "$SCOPE" add README
git -C "$SCOPE" -c commit.gpgsign=false commit -m seed >/dev/null
git -C "$SCOPE" worktree add -b pr-1 "$WT" >/dev/null

write_unit() {
  local stem=$1 cwd=$2
  cat >"$UNIT_DIR/$stem.service" <<EOF
# managed: systemd-ops 1
[Unit]
Description=$stem
[Service]
Type=oneshot
ExecStart=/bin/true
WorkingDirectory=$cwd
EOF
}

write_unit "$PR1" "$WT"
write_unit "$PR2" "$NOT_WT"

cat >"$TMP/bin/systemctl" <<EOF
#!/bin/bash
set -euo pipefail
while [ "\${1:-}" = --user ]; do shift; done
UNIT_DIR="$UNIT_DIR"
case " \$* " in
  *" list-unit-files "*)
    {
      shopt -s nullglob
      for f in "\$UNIT_DIR"/*.service "\$UNIT_DIR"/*.timer; do
        printf '%s\n' "\${f##*/}"
      done
    } | jq -R -s -c 'split("\n") | map(select(length > 0)) | map({unit_file: ., state: "static", preset: "enabled"})'
    ;;
  *" list-units "*|*" list-timers "*)
    printf '%s\n' '[]'
    ;;
  *" show "*)
    after=0
    units=()
    for a in "\$@"; do
      if [ "\$a" = -- ]; then after=1; continue; fi
      if [ "\$after" -eq 1 ]; then units+=("\$a"); fi
    done
    if [ \${#units[@]} -eq 0 ]; then
      units=("\${@: -1}")
    fi
    first=1
    for unit in "\${units[@]}"; do
      [ "\$first" -eq 1 ] || printf '\\n'
      first=0
      path="\$UNIT_DIR/\$unit"
      if [ -f "\$path" ]; then
        printf 'Id=%s\\nLoadState=loaded\\nActiveState=inactive\\nSubState=dead\\nFragmentPath=%s\\n' "\$unit" "\$path"
      else
        printf 'Id=%s\\nLoadState=not-found\\nActiveState=inactive\\nSubState=dead\\n' "\$unit"
      fi
    done
    ;;
  *)
    printf '%s\n' '[]'
    ;;
esac
EOF
chmod 755 "$TMP/bin/systemctl"

cat >"$TMP/bin/notify-stub" <<EOF
#!/bin/sh
printf '%s\n' "\$@" > "$NOTIFY_LOG"
exit 0
EOF
chmod 755 "$TMP/bin/notify-stub"

export PATH=$TMP/bin:$PATH
export HOME=$TMP
export XDG_CONFIG_HOME=$TMP/xdg
export SYSTEMD_OPS_STATE_DIR=$TMP/state
export SYSTEMD_OPS_SCOPE_ROOT=$SCOPE
export SYSTEM_NOTIFY_LIB=$TMP/bin/notify-stub

json() {
  "$BIN" --json --manager user --write-prefix 'managed-*' --scope-root "$SCOPE" "$@"
}

listed_units() {
  json inspect list-operations | jq -c '[.data[]?.unit]'
}

has_unit() {
  local stem=$1
  listed_units | jq -e --arg stem "$stem" 'index($stem) != null' >/dev/null
}

lacks_unit() {
  local stem=$1
  listed_units | jq -e --arg stem "$stem" 'index($stem) == null' >/dev/null
}

github_hits() {
  rg -n 'gh |github.com|api.github' "$ROOT/src/automation.rs" || true
}

GITHUB_BEFORE=$(github_hits)

"$BIN" --help | grep -q 'plan-normalize-pr' || fail "USAGE missing plan-normalize-pr"

has_unit "$PR1" || fail "pr-1 missing from list-operations before normalize"
has_unit "$PR2" || fail "pr-2 missing from list-operations before normalize"

missing=$(json automation plan-normalize-pr --spec "$(jq -nc --arg unit "$PR1" '{
  version: 1,
  unit: $unit,
  pr: "1",
  merged_at: "2026-09-19T00:00:00Z"
}')" || true)
rejected "$missing"
jq -e '.error.message | test("merge_sha")' <<<"$missing" >/dev/null \
  || fail "missing merge_sha did not name merge_sha: $missing"
has_unit "$PR1" || fail "missing merge_sha retired pr-1"
[[ -f $UNIT_DIR/$PR1.service ]] || fail "missing merge_sha removed pr-1 unit file"
[[ ! -f $NOTIFY_LOG ]] || fail "missing merge_sha notified: $(cat "$NOTIFY_LOG")"

bad=$(json automation plan-normalize-pr --spec "$(jq -nc --arg unit "$PR1" --arg sha "$BAD_SHA" '{
  version: 1,
  unit: $unit,
  pr: "1",
  merge_sha: $sha,
  merged_at: "2026-09-19T00:00:00Z"
}')" || true)
rejected "$bad"
has_unit "$PR1" || fail "bad hex merge_sha retired pr-1"
[[ -f $UNIT_DIR/$PR1.service ]] || fail "bad hex merge_sha removed pr-1 unit file"

rm -f "$NOTIFY_LOG"
unregistered_spec=$(jq -nc --arg unit "$PR2" --arg sha "$MERGE_SHA" '{
  version: 1,
  unit: $unit,
  pr: "2",
  merge_sha: $sha,
  merged_at: "2026-09-19T00:00:00Z"
}')
planned=$(json automation plan-normalize-pr --spec "$unregistered_spec")
ok "$planned"
applied=$(json automation apply --plan-token "$(jq -er '.data.plan_token' <<<"$planned")")
ok "$applied"
lacks_unit "$PR2" || fail "valid apply left pr-2 in list-operations: $(listed_units)"
[[ ! -f $UNIT_DIR/$PR2.service ]] || fail "valid apply did not retire pr-2 unit file"
[[ -d $NOT_WT ]] || fail "non-registered cwd was rm -rf'd"
[[ -f $NOT_WT/sentinel ]] || fail "non-registered cwd contents were destroyed"
[[ -d $WT ]] || fail "unregistered apply removed the registered worktree"

rm -f "$NOTIFY_LOG"
happy_spec=$(jq -nc --arg unit "$PR1" --arg sha "$MERGE_SHA" '{
  version: 1,
  unit: $unit,
  pr: "1",
  merge_sha: $sha,
  merged_at: "2026-09-19T00:00:00Z"
}')
planned=$(json automation plan-normalize-pr --spec "$happy_spec")
ok "$planned"
applied=$(json automation apply --plan-token "$(jq -er '.data.plan_token' <<<"$planned")")
ok "$applied"
lacks_unit "$PR1" || fail "valid apply left pr-1 in list-operations: $(listed_units)"
[[ ! -f $UNIT_DIR/$PR1.service ]] || fail "valid apply did not retire pr-1 unit file"
[[ -f $NOTIFY_LOG ]] || fail "notify stub was not invoked"
[[ $(grep -cx inform "$NOTIFY_LOG") -eq 1 ]] || fail "notify did not inform once: $(cat "$NOTIFY_LOG")"
grep -qx 'omp-runtime' "$NOTIFY_LOG" || fail "notify source is not omp-runtime: $(cat "$NOTIFY_LOG")"
grep -q "$PR1" "$NOTIFY_LOG" || fail "notify missing unit $PR1: $(cat "$NOTIFY_LOG")"
grep -q "$MERGE_SHA" "$NOTIFY_LOG" || fail "notify missing merge_sha: $(cat "$NOTIFY_LOG")"
grep -q '1' "$NOTIFY_LOG" || fail "notify missing pr: $(cat "$NOTIFY_LOG")"
if git -C "$SCOPE" worktree list | grep -F "$WT"; then
  fail "registered worktree still listed: $(git -C "$SCOPE" worktree list)"
fi
[[ ! -e $WT ]] || fail "registered worktree cwd was not removed"

GITHUB_AFTER=$(github_hits)
[[ $GITHUB_AFTER == "$GITHUB_BEFORE" ]] || fail "src/automation.rs gained a GitHub call site:
before:
$GITHUB_BEFORE
after:
$GITHUB_AFTER"

grep -q 'tests/normalize-pr.sh' "$ROOT/docs/TESTING.md" || fail "TESTING.md Fast gates missing normalize-pr.sh"
grep -q 'tests/normalize-pr.sh' "$ROOT/.github/workflows/ci.yml" || fail "ci.yml Dogfood proofs missing normalize-pr.sh"

live_gens_after=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_after=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)
live_ohmy_after=$(
  find "$LIVE_OHMYPI" -type f -printf '%T@ %p\n' 2>/dev/null | sort | sha256sum | awk '{print $1}'
)
[[ $live_gens_after == "$live_gens_before" ]] || fail "mutated live generations/current"
[[ $live_omp_after == "$live_omp_before" ]] || fail "mutated live omp install"
[[ $live_ohmy_after == "$live_ohmy_before" ]] || fail "mutated live oh-my-pi"

echo "normalize-pr ok"
