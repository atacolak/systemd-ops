#!/usr/bin/env bash
# Native payload is copied and export-checked before the agent runs.
# Tiny fixture files, never the live 200MB addon.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${WRAPPER_LIB:-$ROOT/dogfood/lib/automation-wrapper}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v python3 >/dev/null || fail "python3 required"
[[ -r $LIB ]] || fail "missing $LIB"

grep -q 'native-provisioning' "$ROOT/src/automation.rs" \
  || fail "native-provisioning is not a known blocker kind"
grep -q 'provision_native_addon' "$LIB" \
  || fail "automation-wrapper is missing provision_native_addon"
grep -q 'ensure_clean_worktree' "$LIB" \
  || fail "automation-wrapper is missing ensure_clean_worktree"
python3 - "$LIB" <<'PY' || fail "ensure_clean_worktree does not provision before return"
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text()
fn = text.split("ensure_clean_worktree()", 1)[1].split("\nFAILURE_BUDGET", 1)[0]
if "provision_native_addon" not in fn:
    raise SystemExit(1)
PY

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
# shellcheck source=/dev/null
source "$LIB"

write_addon() {
  local path=$1
  shift
  mkdir -p "$(dirname -- "$path")"
  printf '%s\n' "$@" >"$path"
}

good_exports=(countTokens executeShell visibleWidth DesktopSession hashlineStripPrefixes EditStore)
stale_exports=(countTokens executeShell visibleWidth DesktopSession)

export NATIVE_ADDON_NAME=pi_natives.linux-x64-modern.node
export NATIVE_ADDON_EXPORTS="${good_exports[*]}"
export NATIVE_ADDON_DIR="$TMP/pinned"
export NATIVE_ADDON_SOURCE=""

write_addon "$TMP/pinned/$NATIVE_ADDON_NAME" "${good_exports[@]}"
write_addon "$TMP/stale.node" "${stale_exports[@]}"

# Trees without packages/natives are not native worktrees.
WORKTREE=$TMP/no-natives
mkdir -p "$WORKTREE"
provision_native_addon || fail "tree without packages/natives should skip"

# Missing dest is filled from the pinned source.
WORKTREE=$TMP/missing
mkdir -p "$WORKTREE/packages/natives/native"
provision_native_addon || fail "missing dest was not provisioned from pinned source"
[[ -f $WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME ]] \
  || fail "provision did not write dest addon"
native_addon_has_exports "$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME" \
  || fail "provisioned dest is still export-deficient"

# Already-good dest is left alone.
WORKTREE=$TMP/good
mkdir -p "$WORKTREE/packages/natives/native"
write_addon "$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME" "${good_exports[@]}" marker-keep
before=$(cat "$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME")
provision_native_addon || fail "good dest was treated as a provisioning failure"
after=$(cat "$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME")
[[ "$before" == "$after" ]] || fail "good dest was overwritten"

# Stale dest is replaced from the pinned source.
WORKTREE=$TMP/stale
mkdir -p "$WORKTREE/packages/natives/native"
cp -a "$TMP/stale.node" "$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME"
provision_native_addon || fail "stale dest was not replaced"
native_addon_has_exports "$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME" \
  || fail "replaced dest is still export-deficient"

# Stale dest with no other source fails fast.
WORKTREE=$TMP/stale-only
mkdir -p "$WORKTREE/packages/natives/native"
cp -a "$TMP/stale.node" "$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME"
NATIVE_ADDON_DIR=$WORKTREE/packages/natives/native
NATIVE_ADDON_SOURCE=$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME
if provision_native_addon; then
  fail "stale dest with no other source was accepted"
fi
native_addon_has_exports "$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME" \
  && fail "stale-only dest unexpectedly gained required exports"

# ensure_clean_worktree records native-provisioning and does not launch an agent.
SCOPE=$TMP/scope
WORKTREE=$SCOPE/worktree
STATE_DIR=$SCOPE/state
mkdir -p "$WORKTREE/packages/natives/native" "$STATE_DIR" "$SCOPE/fork.git"
git -C "$SCOPE/fork.git" init -q --bare
git -C "$WORKTREE" init -q
git -C "$WORKTREE" config user.email proof@example.invalid
git -C "$WORKTREE" config user.name proof
git -C "$WORKTREE" checkout -q -b cap/nested-lsp-roots
mkdir -p "$WORKTREE/packages/natives/native"
printf '*.node\n' >"$WORKTREE/.gitignore"
touch "$WORKTREE/packages/natives/native/.gitkeep"
git -C "$WORKTREE" add .gitignore packages
git -C "$WORKTREE" commit -qm cap
git -C "$WORKTREE" remote add fork "$SCOPE/fork.git"
git -C "$WORKTREE" push -q fork cap/nested-lsp-roots
cp -a "$TMP/stale.node" "$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME"
NATIVE_ADDON_DIR=$WORKTREE/packages/natives/native
NATIVE_ADDON_SOURCE=$WORKTREE/packages/natives/native/$NATIVE_ADDON_NAME
OPERATION_STEM=managed-omp-cap-nested-lsp-roots
SYSTEMD_OPS_BIN=/bin/true
blocker_file=$TMP/blocker-kind
rm -f "$blocker_file"
ops() {
  if [[ "${1:-}" == automation && "${2:-}" == blocker ]]; then
    local i=0 arg
    while (( i < $# )); do
      i=$((i + 1))
      eval "arg=\${$i}"
      if [[ "$arg" == --kind ]]; then
        i=$((i + 1))
        eval "printf '%s\\n' \"\${$i}\"" >"$blocker_file"
      fi
    done
    echo '{"ok":true,"data":{"changed":true}}'
    return 0
  fi
  echo '{"ok":true}'
}

set +e
ensure_clean_worktree cap/nested-lsp-roots
code=$?
set -e
[[ $code -eq 4 ]] || fail "ensure_clean_worktree exited $code, want 4"
[[ "$(cat "$blocker_file" 2>/dev/null || true)" == native-provisioning ]] \
  || fail "ensure_clean_worktree recorded kind '$(cat "$blocker_file" 2>/dev/null || true)', want native-provisioning"

echo "native-provisioning ok"
