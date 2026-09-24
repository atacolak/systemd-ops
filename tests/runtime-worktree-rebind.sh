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

# --- omp-session lib: bucket formula, recovered marker, GC guards ---
SESSION_LIB=${SESSION_LIB:-$ROOT/dogfood/lib/omp-session}
[[ -r $SESSION_LIB ]] || fail "missing $SESSION_LIB"
export HOME=$TMP/fake-home
mkdir -p "$HOME"
export OMP_SESSIONS_ROOT=$TMP/sessions
# shellcheck source=/dev/null
source "$SESSION_LIB"

mkdir -p "$HOME/workspace/worktrees/omp-automation/managed-omp-pr-1-it-1"
got=$(omp_session_bucket_for_cwd "$HOME/workspace/worktrees/omp-automation/managed-omp-pr-1-it-1")
[[ $got == "$OMP_SESSIONS_ROOT/-workspace-worktrees-omp-automation-managed-omp-pr-1-it-1" ]] \
  || fail "home-relative bucket: $got"

mkdir -p "$TMP/tmpwork"
got=$(TMPDIR=$TMP omp_session_bucket_for_cwd "$TMP/tmpwork")
[[ $got == "$OMP_SESSIONS_ROOT/-tmp-tmpwork" ]] || fail "tmp-root bucket: $got"

mkdir -p "$TMP/not-tmp" "$TMP/outside"
got=$(TMPDIR=$TMP/not-tmp omp_session_bucket_for_cwd "$TMP/outside")
# ${rel//[\\/:]/-} does not replace / on bash 5.2 (the slash is the
# pattern delimiter). Expected names are all hyphens.
rel=${TMP#/}
rel=${rel//\//-}
rel=${rel//:/-}
want="$OMP_SESSIONS_ROOT/--${rel}-outside--"
[[ $got == "$want" ]] || fail "legacy absolute bucket: $got want $want"

worktree_path_is_recovered "$TMP/cap-recovered-20260907T230900Z" \
  || fail "recovered marker not seen"
worktree_path_is_recovered "$TMP/cap-recovered-20260907T230900Z-recovered-20260912T203151Z" \
  || fail "stacked recovered marker not seen"
! worktree_path_is_recovered "$TMP/cap" || fail "plain path has a marker"
! worktree_path_is_recovered "$TMP/cap-recovered-20260907" || fail "partial stamp has a marker"

# exclusive-bucket GC (pr-run attempt): deletes, no-op when missing,
# refuses the ambiguous $HOME bucket
att=$HOME/workspace/worktrees/omp-automation/managed-omp-pr-9-it-9
mkdir -p "$att"
att_bucket=$(omp_session_bucket_for_cwd "$att")
mkdir -p "$att_bucket/2026-09-22T00-00-00-000Z_proof"
echo log >"$att_bucket/2026-09-22T00-00-00-000Z_proof/0.bash.log"
gc_exclusive_session_bucket "$att" || fail "exclusive-bucket GC failed"
[[ ! -e $att_bucket ]] || fail "attempt bucket survived GC"
gc_exclusive_session_bucket "$att" || fail "missing bucket was not a no-op"
! gc_exclusive_session_bucket "$HOME" 2>/dev/null || fail "ambiguous bucket was not refused"

# recovered-bucket GC: deletes recovered, spares original, no-op when missing
old=$HOME/worktrees/omp/cap/nested-lsp-roots-recovered-20260907T230900Z
mkdir -p "$old"
bucket=$(omp_session_bucket_for_cwd "$old")
mkdir -p "$bucket/2026-09-07T23-55-52-747Z_proof"
echo log >"$bucket/2026-09-07T23-55-52-747Z_proof/2.bash.log"
gc_recovered_tree_session_bucket "$old" || fail "recovered-bucket GC failed"
[[ ! -e $bucket ]] || fail "recovered-tree bucket survived GC"

keep=$HOME/worktrees/omp/cap/nested-lsp-roots
mkdir -p "$keep"
keep_bucket=$(omp_session_bucket_for_cwd "$keep")
mkdir -p "$keep_bucket"
echo transcript >"$keep_bucket/keep.jsonl"
gc_recovered_tree_session_bucket "$keep" || fail "original-tree GC returned failure"
[[ -f $keep_bucket/keep.jsonl ]] || fail "original-tree bucket was deleted"

gc_recovered_tree_session_bucket "$HOME/worktrees/omp/cap/gone-recovered-20260907T230900Z" \
  || fail "missing bucket was not a no-op"

# --- wiring pins: session GC lives in pr-run and capability-run only ---
CAP_DRIVER=${CAP_DRIVER:-$ROOT/dogfood/drivers/capability-run}
PR_DRIVER=${PR_DRIVER:-$ROOT/dogfood/drivers/pr-run}
grep -q 'gc_recovered_tree_session_bucket' "$CAP_DRIVER" \
  || fail "capability-run does not GC recovered-tree session buckets"
grep -q 'gc_exclusive_session_bucket' "$PR_DRIVER" \
  || fail "pr-run does not GC the attempt session bucket"
if grep -q 'gc_recovered_tree_session_bucket\|gc_exclusive_session_bucket\|--session-dir\|--no-session' "$DRIVER"; then
  fail "runtime-run gained session GC outside the epic scope"
fi

echo "runtime-worktree-rebind ok"
