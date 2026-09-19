#!/usr/bin/env bash
# Capability leaves the organism through plan-normalize-capability.
# Throwaway scope. Sources capability_pass_is_verified_noop; does not run a pass.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
CAPABILITY_RUN=${CAPABILITY_RUN_LIB:-$ROOT/dogfood/drivers/capability-run}
TMP=$(mktemp -d)
SCOPE=$TMP/scope
CAP=managed-omp-cap-example
PR=managed-omp-pr-1
HOME_CAP=$SCOPE/.systemd-ops/operations/$CAP
HOME_PR=$SCOPE/.systemd-ops/operations/$PR
UNIT_DIR=$TMP/xdg/systemd/user
AGENT=$TMP/agents
NOTIFY_LOG=$TMP/notify.argv
WT_DELTA=$TMP/worktrees/cap-delta
WT_EMPTY=$TMP/worktrees/cap-empty
WT_PR=$TMP/worktrees/pr-1
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { jq -e '.ok == true' <<<"$1" >/dev/null || fail "expected ok: $1"; }
rejected() { jq -e '.ok == false' <<<"$1" >/dev/null || fail "expected rejection: $1"; }

command -v jq >/dev/null || fail "jq required"
command -v git >/dev/null || fail "git required"
command -v rg >/dev/null || fail "rg required"
[[ -x $BIN ]] || fail "missing $BIN"
[[ -r $CAPABILITY_RUN ]] || fail "missing $CAPABILITY_RUN"

LIVE_GENS=/home/sf/worktrees/omp/generations
LIVE_OMP=/home/sf/.bun/bin/omp
LIVE_OHMYPI=/home/sf/workspace/oh-my-pi
live_gens_before=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_before=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)
live_ohmy_before=$(
  find "$LIVE_OHMYPI" -type f -printf '%T@ %p\n' 2>/dev/null | sort | sha256sum | awk '{print $1}'
)

mkdir -p "$HOME_CAP" "$HOME_PR" "$UNIT_DIR" "$SCOPE/.systemd-ops" \
  "$AGENT/.omp/agents" "$TMP/bin" \
  "$SCOPE/.systemd-ops/operations/managed-omp-release-watch/state"
printf '%s\n' 'capability-maintainer role' >"$AGENT/.omp/agents/capability-maintainer.md"
printf '%s\n' 'pr-maintainer role' >"$AGENT/.omp/agents/pr-maintainer.md"

cat >"$SCOPE/.systemd-ops/scope.toml" <<EOF
[scope]
id = "proof"
owned = ["managed-*"]

[automation]
agent_root = "$AGENT"
EOF

cat >"$HOME_CAP/automation.toml" <<'EOF'
version = 1
agent = "capability-maintainer"
parent = "managed-omp-runtime-integrator"
EOF
cat >"$HOME_PR/automation.toml" <<EOF
version = 1
agent = "pr-maintainer"
parent = "$CAP"
EOF

export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
git -C "$SCOPE" init >/dev/null
git -C "$SCOPE" config user.email "normalize-capability@example.test"
git -C "$SCOPE" config user.name "normalize-capability"
printf '%s\n' seed >"$SCOPE/README"
git -C "$SCOPE" add README
git -C "$SCOPE" -c commit.gpgsign=false commit -m seed >/dev/null
BASE=$(git -C "$SCOPE" rev-parse HEAD)
printf '%s\n' generation >"$SCOPE/generation.txt"
git -C "$SCOPE" add generation.txt
git -C "$SCOPE" -c commit.gpgsign=false commit -m generation >/dev/null
GEN=$(git -C "$SCOPE" rev-parse HEAD)

git -C "$SCOPE" worktree add -b cap-empty "$WT_EMPTY" >/dev/null
git -C "$SCOPE" worktree add -b cap-delta "$WT_DELTA" "$BASE" >/dev/null
printf '%s\n' extra >"$WT_DELTA/extra.txt"
git -C "$WT_DELTA" add extra.txt
git -C "$WT_DELTA" -c commit.gpgsign=false commit -m extra >/dev/null
git -C "$SCOPE" worktree add -b pr-1 "$WT_PR" >/dev/null

jq -n --arg t "$GEN" '{version:1,mode:"release",target_commit:$t}' \
  >"$SCOPE/.systemd-ops/operations/managed-omp-release-watch/state/generation.json"

# Source only the named predicate. Never execute capability-run's driver body.
eval "$(sed -n '/^capability_pass_is_verified_noop()/,/^}/p' "$CAPABILITY_RUN")"
command -v capability_pass_is_verified_noop >/dev/null \
  || fail "could not source capability_pass_is_verified_noop"
capability_pass_is_verified_noop 0 "$GEN" "$WT_DELTA" \
  && fail "delta fixture is a verified no-op"
delta_base=$(git -C "$WT_DELTA" merge-base HEAD "$GEN")
[[ -n $(git -C "$WT_DELTA" merge-tree "$delta_base" HEAD "$GEN") ]] \
  || fail "delta fixture merge-tree is empty"
capability_pass_is_verified_noop 0 "$GEN" "$WT_EMPTY" \
  || fail "empty fixture is not a verified no-op"

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

write_unit "$CAP" "$WT_DELTA"
write_unit "$PR" "$WT_PR"

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
  listed_units | jq -e --arg u "$1" 'index($u) != null' >/dev/null
}

lacks_unit() {
  listed_units | jq -e --arg u "$1" 'index($u) == null' >/dev/null
}

cap_spec() {
  jq -nc --arg unit "$CAP" '{version: 1, unit: $unit}'
}

"$BIN" --help | grep -q 'plan-normalize-capability' || fail "USAGE missing plan-normalize-capability"

has_unit "$CAP" || fail "cap missing from list-operations before normalize"
has_unit "$PR" || fail "pr child missing from list-operations before normalize"

rm -f "$NOTIFY_LOG"
delta=$(json automation plan-normalize-capability --spec "$(cap_spec)" || true)
rejected "$delta"
jq -e '.error.message | test("verified_noop")' <<<"$delta" >/dev/null \
  || fail "non-empty merge-tree did not name verified_noop: $delta"
has_unit "$CAP" || fail "non-empty merge-tree retired the capability"
[[ -f $UNIT_DIR/$CAP.service ]] || fail "non-empty merge-tree removed the cap unit file"
[[ -d $WT_DELTA ]] || fail "non-empty refuse removed the delta worktree"
[[ ! -f $NOTIFY_LOG ]] || fail "non-empty merge-tree notified: $(cat "$NOTIFY_LOG")"

write_unit "$CAP" "$WT_EMPTY"
rm -f "$NOTIFY_LOG"
child=$(json automation plan-normalize-capability --spec "$(cap_spec)" || true)
rejected "$child"
jq -e '.error.message | test("pr-maintainer")' <<<"$child" >/dev/null \
  || fail "PR child did not name pr-maintainer: $child"
has_unit "$CAP" || fail "PR child retired the capability"
[[ -f $UNIT_DIR/$CAP.service ]] || fail "PR child removed the cap unit file"
[[ -d $WT_EMPTY ]] || fail "PR-child refuse removed the empty worktree"
[[ ! -f $NOTIFY_LOG ]] || fail "PR child notified: $(cat "$NOTIFY_LOG")"

rm -f "$UNIT_DIR/$PR.service"
rm -f "$NOTIFY_LOG"
planned=$(json automation plan-normalize-capability --spec "$(cap_spec)")
ok "$planned"
[[ ! -f $NOTIFY_LOG ]] || fail "plan-normalize-capability notified before apply"
applied=$(json automation apply --plan-token "$(jq -er '.data.plan_token' <<<"$planned")")
ok "$applied"
lacks_unit "$CAP" || fail "valid apply left cap in list-operations: $(listed_units)"
[[ ! -f $UNIT_DIR/$CAP.service ]] || fail "valid apply did not retire the cap unit file"
[[ -f $NOTIFY_LOG ]] || fail "notify stub was not invoked"
[[ $(grep -cx inform "$NOTIFY_LOG") -eq 1 ]] || fail "notify did not inform once: $(cat "$NOTIFY_LOG")"
grep -qx 'omp-runtime' "$NOTIFY_LOG" || fail "notify source is not omp-runtime: $(cat "$NOTIFY_LOG")"
grep -q "$CAP" "$NOTIFY_LOG" || fail "notify missing unit $CAP: $(cat "$NOTIFY_LOG")"
if git -C "$SCOPE" worktree list | grep -F "$WT_EMPTY"; then
  fail "registered empty worktree still listed: $(git -C "$SCOPE" worktree list)"
fi
[[ ! -e $WT_EMPTY ]] || fail "registered empty worktree cwd was not removed"
[[ -d $WT_DELTA ]] || fail "apply removed the unused delta worktree"
[[ -d $WT_PR ]] || fail "apply removed the PR sibling worktree"
if git -C "$SCOPE" worktree list | grep -Fq "$WT_DELTA"; then
  :
else
  fail "unused delta worktree was unregistered"
fi

if rg -n 'status --porcelain|merge-base --is-ancestor|git .*merge-tree' "$ROOT/src"; then
  fail "src copied capability_pass_is_verified_noop body"
fi
rg -n 'capability_pass_is_verified_noop' "$ROOT/src" >/dev/null \
  || fail "src does not invoke capability_pass_is_verified_noop"
rg -n 'CAPABILITY_RUN_LIB|capability-run' "$ROOT/src/automation.rs" >/dev/null \
  || fail "src does not source capability-run / CAPABILITY_RUN_LIB"

grep -q 'tests/normalize-capability.sh' "$ROOT/docs/TESTING.md" \
  || fail "TESTING.md Fast gates missing normalize-capability.sh"
grep -q 'tests/normalize-capability.sh' "$ROOT/.github/workflows/ci.yml" \
  || fail "ci.yml Dogfood proofs missing normalize-capability.sh"

live_gens_after=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_after=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)
live_ohmy_after=$(
  find "$LIVE_OHMYPI" -type f -printf '%T@ %p\n' 2>/dev/null | sort | sha256sum | awk '{print $1}'
)
[[ $live_gens_after == "$live_gens_before" ]] || fail "mutated live generations/current"
[[ $live_omp_after == "$live_omp_before" ]] || fail "mutated live omp install"
[[ $live_ohmy_after == "$live_ohmy_before" ]] || fail "mutated live oh-my-pi"

echo "normalize-capability ok"
