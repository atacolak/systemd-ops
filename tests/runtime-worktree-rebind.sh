#!/usr/bin/env bash
# runtime-run must honor the rebound WorkingDirectory. A hardcoded
# WORKTREE default re-targets the dirty canonical tree every tick.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DRIVER=${RUNTIME_RUN:-$ROOT/dogfood/drivers/runtime-run}
LIB=${WRAPPER_LIB:-$ROOT/dogfood/lib/automation-wrapper}

fail() { echo "FAIL: $*" >&2; exit 1; }
[[ -r $DRIVER && -r $LIB ]] || fail "missing $DRIVER or $LIB"

python3 - "$DRIVER" <<'PY' || fail "runtime-run still hardcodes WORKTREE"
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text().splitlines()
found = None
for line in text:
    if line.startswith("WORKTREE="):
        found = line
        break
if found is None:
    raise SystemExit("no WORKTREE assignment")
if "/home/sf/worktrees/omp/runtime" in found and "$PWD" not in found:
    raise SystemExit(found)
if "${WORKTREE:-$PWD}" not in found:
    raise SystemExit(found)
PY

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
CANON=$TMP/runtime
RECOVERED=$TMP/runtime-recovered-20260912T222954Z
mkdir -p "$CANON" "$RECOVERED"
echo dirty >"$CANON/README.md"
echo clean >"$RECOVERED/README.md"

# Simulate the unit after rebind: WorkingDirectory is the recovered tree,
# Environment=WORKTREE is empty. The driver must bind WORKTREE to $PWD.
got=$(cd "$RECOVERED" && env -u WORKTREE bash -c 'source /dev/stdin; printf %s "$WORKTREE"' <<<"$(sed -n '1,5p' "$DRIVER")")
[[ "$got" == "$RECOVERED" ]] || fail "unbound WORKTREE resolved to '$got', want recovered cwd $RECOVERED"

# Explicit WORKTREE still wins (tests / manual override).
got=$(cd "$RECOVERED" && WORKTREE="$CANON" bash -c 'source /dev/stdin; printf %s "$WORKTREE"' <<<"$(sed -n '1,5p' "$DRIVER")")
[[ "$got" == "$CANON" ]] || fail "explicit WORKTREE was ignored: '$got'"

# Fresh unit cwd is the canonical compose tree; default stays that path.
got=$(cd "$CANON" && env -u WORKTREE bash -c 'source /dev/stdin; printf %s "$WORKTREE"' <<<"$(sed -n '1,5p' "$DRIVER")")
[[ "$got" == "$CANON" ]] || fail "fresh-unit cwd did not become WORKTREE: '$got'"

# shellcheck source=/dev/null
source "$LIB"
nested=$TMP/nested-lsp-roots-recovered-20260912T195934Z-recovered-20260912T203151Z-recovered-20260912T205514Z
got=$(replacement_worktree_path "$nested")
base=$(basename -- "$got")
[[ "$base" == nested-lsp-roots-recovered-* ]] \
  || fail "nested recovery named '$base'"
[[ "$base" != *-recovered-*-recovered-* ]] \
  || fail "recovery still stacked suffixes: '$base'"
canon_got=$(replacement_worktree_path "$TMP/runtime")
[[ $(basename -- "$canon_got") == runtime-recovered-* ]] \
  || fail "canonical recovery named '$(basename -- "$canon_got")'"

echo "runtime-worktree-rebind ok"
