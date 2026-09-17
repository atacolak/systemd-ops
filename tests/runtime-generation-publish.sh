#!/usr/bin/env bash
# READY compose snapshots a detached candidate generation and does not
# flip generations/current or the omp install. The flip lives in
# runtime-promote and rewrites published-generation.json only then.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DRIVER=${RUNTIME_RUN:-$ROOT/dogfood/drivers/runtime-run}
LIB=${WRAPPER_LIB:-$ROOT/dogfood/lib/automation-wrapper}
PROMOTE=${RUNTIME_PROMOTE:-$ROOT/dogfood/lib/runtime-promote}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
command -v git >/dev/null || fail "git required"
[[ -r $DRIVER ]] || fail "missing $DRIVER"
[[ -r $LIB ]] || fail "missing $LIB"

grep -q 'publish_runtime_generation' "$DRIVER" \
  || fail "runtime-run is missing publish_runtime_generation"
python3 - "$DRIVER" <<'PY' || fail "publish does not precede mark_processed"
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text()
pub = text.find('if ! publish_runtime_generation "$upstream_generation" "$post_head"')
mark = text.find('mark_processed "$post_input" ready')
chk = text.find('write_output_checkpoint "$post_input"')
if pub < 0 or mark < 0 or chk < 0:
    raise SystemExit(1)
if not (pub < mark < chk):
    raise SystemExit(1)
PY

LIVE_OMP=$(readlink -f /home/sf/.bun/bin/omp 2>/dev/null || true)

TMP=$(mktemp -d)
COMPOSE=$TMP/compose
trap 'git -C "$COMPOSE" worktree prune >/dev/null 2>&1 || true; rm -rf "$TMP"' EXIT

GENS=$TMP/generations
BINDIR=$TMP/bin
STATE=$TMP/state
mkdir -p "$COMPOSE/packages/coding-agent/scripts" \
  "$COMPOSE/packages/natives/native" \
  "$BINDIR" "$STATE"

git -C "$COMPOSE" init -q
git -C "$COMPOSE" config user.email proof@example
git -C "$COMPOSE" config user.name proof
git -C "$COMPOSE" config core.autocrlf false

cat >"$COMPOSE/packages/coding-agent/scripts/ompalt" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == --help ]]; then
  echo ompalt-help-fast-path
  exit 0
fi
if [[ "${1:-}" == --smoke-test ]]; then
  echo ompalt-smoke
  exit 0
fi
echo ompalt-v1
exit 0
EOF
chmod 0755 "$COMPOSE/packages/coding-agent/scripts/ompalt"
printf 'node_modules\n*.node\ntarget/\npackages/coding-agent/src/export/html/tool-views.generated.js\npackages/stats/dist/\n' >"$COMPOSE/.gitignore"
mkdir -p "$COMPOSE/packages/coding-agent/src/export/html" \
  "$COMPOSE/packages/stats/dist/client" \
  "$COMPOSE/target/debug"
echo generated-js >"$COMPOSE/packages/coding-agent/src/export/html/tool-views.generated.js"
echo stats-client >"$COMPOSE/packages/stats/dist/client/index.js"
echo rust-target >"$COMPOSE/target/debug/omp"
git -C "$COMPOSE" add packages .gitignore
git -C "$COMPOSE" commit -qm 'v1'

rev1=$(git -C "$COMPOSE" rev-parse HEAD)
mkdir -p "$COMPOSE/node_modules/pkg" "$COMPOSE/packages/natives/native"
echo payload-v1 >"$COMPOSE/node_modules/pkg/index.js"
echo native-v1 >"$COMPOSE/packages/natives/native/pi_natives.linux-x64-modern.node"

ln -s "$COMPOSE/packages/coding-agent/scripts/ompalt" "$BINDIR/omp"
[[ $(readlink -f "$BINDIR/omp") == "$COMPOSE/packages/coding-agent/scripts/ompalt" ]] \
  || fail "fixture omp did not resolve into compose worktree"

export RUNTIME_RUN_LIB=1
export WRAPPER_LIB=$LIB
export WORKTREE=$COMPOSE
export GENERATIONS_ROOT=$GENS
export OMP_INSTALL=$BINDIR/omp
export STATE_DIR=$STATE
# shellcheck source=/dev/null
source "$DRIVER"

initial_omp=$(readlink -f "$BINDIR/omp")
publish_runtime_generation gen-1 "$rev1" || fail "first compose failed"
[[ -d $GENS/$rev1 ]] || fail "first generation directory missing"
[[ -f $GENS/$rev1/node_modules/pkg/index.js ]] || fail "node_modules payload was not copied"
[[ -f $GENS/$rev1/packages/natives/native/pi_natives.linux-x64-modern.node ]] \
  || fail "untracked .node payload was not copied"
[[ -f $GENS/$rev1/packages/coding-agent/src/export/html/tool-views.generated.js ]] \
  || fail "ignored tool-views.generated.js was dropped"
[[ -f $GENS/$rev1/packages/stats/dist/client/index.js ]] \
  || fail "ignored packages/stats/dist was dropped"
[[ ! -e $GENS/$rev1/target/debug/omp ]] || fail "target/ leaked into the generation"

[[ ! -e $GENS/current && ! -L $GENS/current ]] \
  || fail "READY compose moved generations/current"
[[ $(readlink -f "$BINDIR/omp") == "$initial_omp" ]] \
  || fail "READY compose moved the omp install"
[[ ! -f $STATE/published-generation.json ]] \
  || fail "READY compose rewrote published-generation.json"
jq -e --arg gen gen-1 --arg rev "$rev1" '
  .candidate.phase=="candidate"
  and .candidate.generation==$gen
  and .candidate.output_revision==$rev
' "$STATE/deployment.json" >/dev/null \
  || fail "deployment.json did not record phase=candidate after READY compose"

python3 - "$DRIVER" <<'PY' || fail "READY compose calls prune_runtime_generations"
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text()
start = text.find("publish_runtime_generation() {")
marker = '\n}\n\nif [[ "${RUNTIME_RUN_LIB:-}" == 1 ]]'
end = text.find(marker, start)
if start < 0 or end < 0:
    raise SystemExit(1)
if "prune_runtime_generations" in text[start:end]:
    raise SystemExit(1)
PY

echo compose-advanced >"$COMPOSE/packages/coding-agent/scripts/ompalt"
[[ $(cat "$GENS/$rev1/packages/coding-agent/scripts/ompalt") == *ompalt-v1* ]] \
  || fail "compose worktree advance mutated the published generation"
git -C "$COMPOSE" checkout -- packages/coding-agent/scripts/ompalt

publish_runtime_generation gen-1 "$rev1" || fail "idempotent recompose failed"
count=$(find "$GENS" -mindepth 1 -maxdepth 1 -type d | wc -l)
[[ $count -eq 1 ]] || fail "idempotent recompose created extra generation dirs ($count)"
[[ ! -e $GENS/current && ! -L $GENS/current ]] \
  || fail "idempotent recompose moved generations/current"
[[ ! -f $STATE/published-generation.json ]] \
  || fail "idempotent recompose rewrote published-generation.json"

[[ -r $PROMOTE ]] || fail "missing $PROMOTE"
# shellcheck source=/dev/null
source "$PROMOTE"
promote_runtime_generation gen-1 "$rev1" || fail "first promote failed"
[[ -L $GENS/current ]] || fail "promote did not create current"
[[ $(readlink -f "$GENS/current") == $(readlink -f "$GENS/$rev1") ]] \
  || fail "promote did not point current at first generation"
[[ $(readlink -f "$BINDIR/omp") == $(readlink -f "$GENS/current/packages/coding-agent/scripts/ompalt") ]] \
  || fail "promote did not resolve omp install into generations/current"
jq -e --arg gen gen-1 --arg rev "$rev1" --arg path "$(readlink -f "$GENS/$rev1")" '
  .version==1
  and .generation==$gen
  and .output_revision==$rev
  and .path==$path
  and (.published_at|test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T"))
' "$STATE/published-generation.json" >/dev/null \
  || fail "published-generation.json mismatch after first promote"

hollow=$TMP/hollow
mkdir -p "$hollow/packages/coding-agent/scripts"
if copy_untracked_runtime_payload "$hollow" "$GENS/$rev1"; then
  fail "copy_untracked_runtime_payload succeeded without src node_modules"
fi
[[ -d $GENS/$rev1/node_modules ]] || fail "failed copy removed dest node_modules"
before_current=$(readlink -f "$GENS/current")
before_omp=$(readlink -f "$BINDIR/omp")
before_published=$(cat "$STATE/published-generation.json")
WORKTREE=$hollow
if publish_runtime_generation gen-hollow "$rev1"; then
  fail "compose succeeded from a node_modules-less worktree"
fi
[[ $(readlink -f "$GENS/current") == "$before_current" ]] \
  || fail "failed compose flipped generations/current"
[[ $(readlink -f "$BINDIR/omp") == "$before_omp" ]] \
  || fail "failed compose repointed omp"
[[ $(cat "$STATE/published-generation.json") == "$before_published" ]] \
  || fail "failed compose rewrote published-generation.json"
WORKTREE=$COMPOSE

cat >"$COMPOSE/packages/coding-agent/scripts/ompalt" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == --help ]]; then
  echo ompalt-help-fast-path
  exit 0
fi
echo cannot import tool-views.generated.js >&2
exit 1
EOF
chmod 0755 "$COMPOSE/packages/coding-agent/scripts/ompalt"
git -C "$COMPOSE" add packages/coding-agent/scripts/ompalt
git -C "$COMPOSE" commit -qm 'broken-smoke'
rev_broken=$(git -C "$COMPOSE" rev-parse HEAD)
before_current=$(readlink -f "$GENS/current")
before_omp=$(readlink -f "$BINDIR/omp")
before_published=$(cat "$STATE/published-generation.json")
if publish_runtime_generation gen-broken "$rev_broken"; then
  fail "compose succeeded when generation --smoke-test failed"
fi
[[ $(readlink -f "$GENS/current") == "$before_current" ]] \
  || fail "failed load postcondition flipped generations/current"
[[ $(readlink -f "$BINDIR/omp") == "$before_omp" ]] \
  || fail "failed load postcondition repointed omp"
[[ $(cat "$STATE/published-generation.json") == "$before_published" ]] \
  || fail "failed load postcondition rewrote published-generation.json"
git -C "$COMPOSE" worktree remove --force "$GENS/$rev_broken" >/dev/null 2>&1 \
  || rm -rf "$GENS/$rev_broken"
cat >"$COMPOSE/packages/coding-agent/scripts/ompalt" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == --help || "${1:-}" == --smoke-test ]]; then
  echo ompalt-ok
  exit 0
fi
echo ompalt-ok
exit 0
EOF
chmod 0755 "$COMPOSE/packages/coding-agent/scripts/ompalt"
git -C "$COMPOSE" add packages/coding-agent/scripts/ompalt
git -C "$COMPOSE" commit -qm 'restore-smoke'

foreign=$TMP/foreign-omp
ln -s /bin/true "$foreign"
OMP_INSTALL=$foreign
if publish_runtime_generation gen-x "$rev1"; then
  fail "unexpected omp target was treated as repointable"
fi
[[ $(readlink -f "$BINDIR/omp") == $(readlink -f "$GENS/current/packages/coding-agent/scripts/ompalt") ]] \
  || fail "failed compose mutated the fixture omp install"
[[ $(cat "$STATE/published-generation.json") == "$before_published" ]] \
  || fail "failed compose from a foreign omp rewrote published-generation.json"
OMP_INSTALL=$BINDIR/omp

promoted_current=$(readlink -f "$GENS/current")
published_before=$(cat "$STATE/published-generation.json")
revs=("$rev1")
for n in 2 3 4; do
  cat >"$COMPOSE/packages/coding-agent/scripts/ompalt" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == --help || "\${1:-}" == --smoke-test ]]; then
  echo ompalt-v$n
  exit 0
fi
echo ompalt-v$n
exit 0
EOF
  chmod 0755 "$COMPOSE/packages/coding-agent/scripts/ompalt"
  git -C "$COMPOSE" add packages/coding-agent/scripts/ompalt
  git -C "$COMPOSE" commit -qm "v$n"
  rev=$(git -C "$COMPOSE" rev-parse HEAD)
  revs+=("$rev")
  echo "payload-v$n" >"$COMPOSE/node_modules/pkg/index.js"
  publish_runtime_generation "gen-$n" "$rev" || fail "compose $n failed"
  [[ $(readlink -f "$GENS/current") == "$promoted_current" ]] \
    || fail "READY compose $n moved generations/current"
  [[ $(readlink -f "$BINDIR/omp") == $(readlink -f "$GENS/current/packages/coding-agent/scripts/ompalt") ]] \
    || fail "READY compose $n moved the omp install"
  [[ $(cat "$STATE/published-generation.json") == "$published_before" ]] \
    || fail "READY compose $n rewrote published-generation.json"
  jq -e --arg gen "gen-$n" --arg rev "$rev" '
    .candidate.phase=="candidate"
    and .candidate.generation==$gen
    and .candidate.output_revision==$rev
  ' "$STATE/deployment.json" >/dev/null \
    || fail "deployment.json did not record phase=candidate after compose $n"
done

gen_dirs=$(find "$GENS" -mindepth 1 -maxdepth 1 -type d | wc -l)
[[ $gen_dirs -eq 4 ]] || fail "compose dropped or pruned generation directories ($gen_dirs)"
[[ -d $GENS/${revs[0]} ]] || fail "oldest generation is missing after later composes"
[[ $promoted_current == $(readlink -f "$GENS/${revs[0]}") ]] \
  || fail "current left the previous promoted generation"

promote_runtime_generation gen-4 "${revs[3]}" || fail "promote of newest candidate failed"
current=$(readlink -f "$GENS/current")
[[ $current == $(readlink -f "$GENS/${revs[3]}") ]] \
  || fail "promote did not move current to the newest generation"
jq -e --arg gen gen-4 --arg rev "${revs[3]}" '
  .generation==$gen and .output_revision==$rev
' "$STATE/published-generation.json" >/dev/null \
  || fail "published-generation.json did not track newest promote"
[[ $(readlink -f "$BINDIR/omp") == $(readlink -f "$GENS/current/packages/coding-agent/scripts/ompalt") ]] \
  || fail "omp install drifted off generations/current after promote"

if [[ -n "${LIVE_OMP:-}" ]]; then
  now=$(readlink -f /home/sf/.bun/bin/omp 2>/dev/null || true)
  [[ "$now" == "$LIVE_OMP" ]] || fail "proof mutated live ~/.bun/bin/omp ($LIVE_OMP -> $now)"
fi

echo "runtime-generation-publish ok"
