#!/usr/bin/env bash
# Pins the native-addon export guard in automation-wrapper.
#
# The required export set is DERIVED from the tree's own generated binding
# surface (packages/natives/native/index.js), so an addon older than the JS that
# loads it can never pass: the frozen six-name literal let an addon without
# `hashlineIsReadTruncationNotice` through and every `.md` write threw.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
HELPER=${WRAPPER_LIB:-$ROOT/dogfood/lib/automation-wrapper}
[[ -f "$HELPER" ]] || { echo "missing $HELPER" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 required" >&2; exit 1; }

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# A tree shaped like the real one: packages/natives/native/{index.js,*.node}.
fixture_tree() {
  mkdir -p "$1/packages/natives/native"
}

# Write a generated surface binding exactly the given names, the way
# scripts/build-bindings.ts does.
surface() {
  local tree=$1 name
  shift
  : >"$tree/packages/natives/native/index.js"
  for name in "$@"; do
    printf 'export const %s = nativeBindings.%s;\n' "$name" "$name" \
      >>"$tree/packages/natives/native/index.js"
  done
}

# A stand-in addon: the scan is a byte-substring scan, so the names are the
# only thing that matters.
addon() {
  local file=$1
  shift
  printf '%s\n' "$*" >"$file"
}

# shellcheck source=/dev/null
source "$HELPER"

CORE="countTokens executeShell visibleWidth DesktopSession hashlineStripPrefixes EditStore"
NEW=hashlineIsReadTruncationNotice

# 1. The shipped defect: an addon carrying the whole frozen six-name literal but
#    not a name this tree's generated surface binds must be reported missing.
T=$WORKDIR/regression
fixture_tree "$T"
surface "$T" $CORE "$NEW"
addon "$T/addon.node" $CORE
WORKTREE=$T
out=$(native_addon_has_exports "$T/addon.node") && rc=0 || rc=$?
[[ "$rc" -ne 0 ]] || fail "addon without $NEW must not pass the derived guard"
[[ "$out" == "$NEW" ]] || fail "expected missing '$NEW', got '$out'"

# 2. Derivation comes from the tree's own surface, not a declared list.
T=$WORKDIR/derived
fixture_tree "$T"
surface "$T" $CORE "$NEW"
addon "$T/addon.node" $CORE "$NEW"
WORKTREE=$T
derived=$(native_addon_required_exports | sort | tr '\n' ' ')
expected=$(printf '%s %s\n' "$CORE" "$NEW" | tr ' ' '\n' | sort -u | tr '\n' ' ')
[[ "$derived" == "$expected" ]] || fail "derived set '$derived' != '$expected'"
[[ "$(native_addon_required_exports | wc -l)" -eq 7 ]] || fail "derived count is not 7"
native_addon_has_exports "$T/addon.node" >/dev/null \
  || fail "an addon satisfying the derived set must no-op"
[[ -z "$(native_addon_has_exports "$T/addon.node")" ]] \
  || fail "a satisfying addon must report nothing"

# 3. A surface that binds nothing (or is absent) keeps the no-op semantics
#    instead of failing the whole wrapper.
T=$WORKDIR/absent
fixture_tree "$T"
addon "$T/addon.node" nothing
WORKTREE=$T
native_addon_has_exports "$T/addon.node" >/dev/null \
  || fail "missing index.js must not fail the guard"
[[ -z "$(native_addon_has_exports "$T/addon.node")" ]] \
  || fail "missing index.js must report nothing"

# 4. The version sentinel is not an ABI requirement: the workspace loader skips
#    sentinel validation, so a release-versioned addon must still pass.
T=$WORKDIR/sentinel
fixture_tree "$T"
surface "$T" $CORE __piNativesV99_9_9
addon "$T/addon.node" $CORE
WORKTREE=$T
native_addon_has_exports "$T/addon.node" >/dev/null \
  || fail "the version sentinel must not be required of an addon"
out=$(native_addon_required_exports)
[[ "$out" != *__piNativesV* ]] || fail "sentinel leaked into the derived set: $out"

# 5. The drift guard names the missing exports and the rebuild command.
T=$WORKDIR/drift
fixture_tree "$T"
surface "$T" $CORE "$NEW"
addon "$T/addon.node" $CORE
WORKTREE=$T
out=$(native_addon_export_drift "$T/addon.node" "native addon source" 2>&1) && rc=0 || rc=$?
[[ "$rc" -ne 0 ]] || fail "drift guard must fail on a deficient addon"
[[ "$out" == *"$NEW"* ]] || fail "drift guard must name the missing export: $out"
[[ "$out" == *"packages/natives run build"* ]] || fail "drift guard must name the rebuild command: $out"
[[ "$out" == *"$T/packages/natives/native/index.js"* ]] \
  || fail "drift guard must name the surface it derived from: $out"
addon "$T/addon.node" $CORE "$NEW"
native_addon_export_drift "$T/addon.node" "native addon source" >/dev/null 2>&1 \
  || fail "drift guard must pass a satisfying addon"
[[ -z "$(native_addon_export_drift "$T/addon.node" "native addon source" 2>&1)" ]] \
  || fail "drift guard must print nothing when there is no drift"

# 6. NATIVE_ADDON_EXPORTS is an explicit operator override that wins over the
#    derived set, and says so rather than silently weakening the guard.
T=$WORKDIR/override
fixture_tree "$T"
surface "$T" $CORE "$NEW"
addon "$T/addon.node" delta
WORKTREE=$T
native_addon_has_exports "$T/addon.node" >/dev/null \
  && fail "the derived set must still apply with no override"
rc=0
err=$(NATIVE_ADDON_EXPORTS=delta native_addon_has_exports "$T/addon.node" 2>&1 >/dev/null) || rc=$?
[[ "$rc" -eq 0 ]] || fail "override 'delta' must win over the derived set"
[[ "$err" == *NATIVE_ADDON_EXPORTS* ]] || fail "an override must announce itself: $err"
rc=0
out=$(NATIVE_ADDON_EXPORTS=epsilon native_addon_has_exports "$T/addon.node" 2>/dev/null) || rc=$?
[[ "$rc" -ne 0 && "$out" == "epsilon" ]] || fail "override 'epsilon' must be required: rc=$rc out=$out"

# 7. Refuse-to-copy is intact: an export-deficient source is refused, the
#    deficient destination is left alone, and the failure is explained.
T=$WORKDIR/refuse
fixture_tree "$T"
surface "$T" $CORE "$NEW"
dest="$T/packages/natives/native/fixture.node"
addon "$dest" $CORE
addon "$T/source.node" $CORE
before=$(sha256sum "$dest" | cut -d' ' -f1)
rc=0
out=$(NATIVE_ADDON_NAME=fixture.node NATIVE_ADDON_SOURCE="$T/source.node" WORKTREE=$T \
  provision_native_addon 2>&1) || rc=$?
[[ "$rc" -ne 0 ]] || fail "an export-deficient source must not be copied"
[[ "$out" == *"$NEW"* ]] || fail "refusal must name the missing export: $out"
[[ "$out" == *"packages/natives run build"* ]] || fail "refusal must name the rebuild command: $out"
[[ "$(sha256sum "$dest" | cut -d' ' -f1)" == "$before" ]] || fail "a refused copy must not touch the destination"

# 8. A satisfying source is still copied, and the post-copy check accepts it.
T=$WORKDIR/copy
fixture_tree "$T"
surface "$T" $CORE "$NEW"
dest="$T/packages/natives/native/fixture.node"
addon "$dest" $CORE
addon "$T/source.node" $CORE "$NEW"
NATIVE_ADDON_NAME=fixture.node NATIVE_ADDON_SOURCE="$T/source.node" WORKTREE=$T \
  provision_native_addon >/dev/null 2>&1 || fail "a satisfying source must be copied"
cmp -s "$T/source.node" "$dest" || fail "the source addon was not copied to $dest"

# 9. NO `__`-prefixed name is an ABI requirement — the rule the tree's own
#    loader applies to this same generated surface
#    (packages/natives/native/loader-state.js, getWorkspaceRequiredExports:
#    `!name.startsWith("__")`). The version sentinel is one instance of that
#    rule, covered by test 4; this case is a NON-sentinel one,
#    `__ompInstallTokioRuntime`, which the loader documents as best-effort: an
#    addon predating it is legitimate and falls back to napi-rs's default
#    runtime. Regression: the filter excluded only the sentinel regex, so the
#    required set carried `__ompInstallTokioRuntime` and this guard would have
#    refused an addon policy tolerates.
T=$WORKDIR/dunder
fixture_tree "$T"
surface "$T" $CORE "$NEW" __ompInstallTokioRuntime __fixtureInternalExport
addon "$T/addon.node" $CORE "$NEW"
WORKTREE=$T
out=$(native_addon_required_exports)
[[ "$(printf '%s\n' "$out" | grep -c '^__')" -eq 0 ]] \
  || fail "the derived set requires __-prefixed names: $(printf '%s\n' "$out" | grep '^__' | tr '\n' ' ')"
native_addon_has_exports "$T/addon.node" >/dev/null \
  || fail "an addon predating __ompInstallTokioRuntime must not be refused"

# 10. The refusal names the TRUE source of the requirement. The derived path
#     says so; an operator override must not be called derived, and must still
#     announce itself instead of being suppressed on this path.
T=$WORKDIR/drift-source
fixture_tree "$T"
surface "$T" $CORE "$NEW"
addon "$T/addon.node" gamma
WORKTREE=$T
rc=0
out=$(native_addon_export_drift "$T/addon.node" "native addon" 2>&1) || rc=$?
[[ "$rc" -ne 0 ]] || fail "drift guard must fail on an addon missing the derived set"
[[ "$out" == *"derived from $T/packages/natives/native/index.js"* ]] \
  || fail "the derived path must name the surface it derived from: $out"
rc=0
out=$(NATIVE_ADDON_EXPORTS=delta native_addon_export_drift "$T/addon.node" "native addon" 2>&1) || rc=$?
[[ "$rc" -ne 0 ]] || fail "an unsatisfied override must still refuse"
[[ "$out" == *delta* ]] || fail "the refusal must name the override's requirement: $out"
[[ "$out" == *"NATIVE_ADDON_EXPORTS is set"* ]] \
  || fail "the override must announce itself on the drift path: $out"
[[ "$out" != *"derived from"* ]] \
  || fail "an override-set requirement must not be reported as derived: $out"

# 11. The override announcement is emitted EXACTLY ONCE per provision. A single
#     drift call announcing is not the contract the operator sees: the provision
#     resolves the required set per CHECK, so a refusal that probes the
#     destination and then reports the drift announced the same override twice.
#     Only running provision_native_addon end to end can observe that.
T=$WORKDIR/provision-refuse-count
fixture_tree "$T"
surface "$T" $CORE "$NEW"
dest="$T/packages/natives/native/fixture.node"
addon "$dest" $CORE
WORKTREE=$T
rc=0
err=$(NATIVE_ADDON_EXPORTS=zzqVerifierAbsentExportqq NATIVE_ADDON_NAME=fixture.node \
  NATIVE_ADDON_SOURCE= provision_native_addon 2>&1 >/dev/null) || rc=$?
[[ "$rc" -ne 0 ]] || fail "an unsatisfiable override on a deficient destination must refuse"
[[ "$(printf '%s\n' "$err" | grep -c 'NATIVE_ADDON_EXPORTS is set')" -eq 1 ]] \
  || fail "a refusal must announce the override exactly once, got $(printf '%s\n' "$err" | grep -c 'NATIVE_ADDON_EXPORTS is set'): $err"

# 12. ... and exactly once on the copy path, which runs the probe plus the
#     source and destination drift checks — three resolutions before this was
#     threaded, so three announcements of one override.
T=$WORKDIR/provision-copy-count
fixture_tree "$T"
surface "$T" $CORE "$NEW"
dest="$T/packages/natives/native/fixture.node"
addon "$dest" $CORE
addon "$T/source.node" delta
WORKTREE=$T
rc=0
err=$(NATIVE_ADDON_EXPORTS=delta NATIVE_ADDON_NAME=fixture.node NATIVE_ADDON_SOURCE="$T/source.node" \
  provision_native_addon 2>&1 >/dev/null) || rc=$?
[[ "$rc" -eq 0 ]] || fail "a source satisfying the override must be copied (rc=$rc): $err"
cmp -s "$T/source.node" "$dest" || fail "the satisfying source was not copied to $dest"
[[ "$(printf '%s\n' "$err" | grep -c 'NATIVE_ADDON_EXPORTS is set')" -eq 1 ]] \
  || fail "a successful provision must announce the override exactly once, got $(printf '%s\n' "$err" | grep -c 'NATIVE_ADDON_EXPORTS is set'): $err"

echo ok
