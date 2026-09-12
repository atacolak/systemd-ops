#!/usr/bin/env bash
# Publish-by-generation hop: READY compose snapshots a detached generation
# before checkpoint. Never touches the live ~/.bun/bin/omp.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DRIVER=${RUNTIME_RUN:-$ROOT/dogfood/drivers/runtime-run}
LIB=${WRAPPER_LIB:-$ROOT/dogfood/lib/automation-wrapper}

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
echo ompalt-v1
EOF
chmod 0755 "$COMPOSE/packages/coding-agent/scripts/ompalt"
printf 'node_modules\n*.node\n' >"$COMPOSE/.gitignore"
git -C "$COMPOSE" add packages .gitignore
git -C "$COMPOSE" commit -qm 'v1'

rev1=$(git -C "$COMPOSE" rev-parse HEAD)
mkdir -p "$COMPOSE/node_modules/pkg"
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

publish_runtime_generation gen-1 "$rev1" || fail "first publish failed"
[[ -d $GENS/$rev1 ]] || fail "first generation directory missing"
[[ -L $GENS/current ]] || fail "current symlink missing"
[[ $(readlink -f "$GENS/current") == $(readlink -f "$GENS/$rev1") ]] \
  || fail "current does not point at first generation"
[[ $(readlink -f "$BINDIR/omp") == $(readlink -f "$GENS/current/packages/coding-agent/scripts/ompalt") ]] \
  || fail "omp install did not resolve into generations/current"
[[ -f $GENS/$rev1/node_modules/pkg/index.js ]] || fail "node_modules payload was not copied"
[[ -f $GENS/$rev1/packages/natives/native/pi_natives.linux-x64-modern.node ]] \
  || fail "untracked .node payload was not copied"
jq -e --arg gen gen-1 --arg rev "$rev1" --arg path "$(readlink -f "$GENS/$rev1")" '
  .version==1
  and .generation==$gen
  and .output_revision==$rev
  and .path==$path
  and (.published_at|test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T"))
' "$STATE/published-generation.json" >/dev/null \
  || fail "published-generation.json mismatch after first publish"

echo compose-advanced >"$COMPOSE/packages/coding-agent/scripts/ompalt"
[[ $(cat "$GENS/$rev1/packages/coding-agent/scripts/ompalt") == *ompalt-v1* ]] \
  || fail "compose worktree advance mutated the published generation"
git -C "$COMPOSE" checkout -- packages/coding-agent/scripts/ompalt

publish_runtime_generation gen-1 "$rev1" || fail "idempotent republish failed"
count=$(find "$GENS" -mindepth 1 -maxdepth 1 -type d | wc -l)
[[ $count -eq 1 ]] || fail "idempotent republish created extra generation dirs ($count)"

foreign=$TMP/foreign-omp
ln -s /bin/true "$foreign"
OMP_INSTALL=$foreign
if publish_runtime_generation gen-x "$rev1"; then
  fail "unexpected omp target was treated as repointable"
fi
[[ $(readlink -f "$BINDIR/omp") == $(readlink -f "$GENS/current/packages/coding-agent/scripts/ompalt") ]] \
  || fail "failed publish mutated the fixture omp install"
OMP_INSTALL=$BINDIR/omp

revs=("$rev1")
for n in 2 3 4; do
  echo "ompalt-v$n" >"$COMPOSE/packages/coding-agent/scripts/ompalt"
  git -C "$COMPOSE" add packages/coding-agent/scripts/ompalt
  git -C "$COMPOSE" commit -qm "v$n"
  rev=$(git -C "$COMPOSE" rev-parse HEAD)
  revs+=("$rev")
  echo "payload-v$n" >"$COMPOSE/node_modules/pkg/index.js"
  publish_runtime_generation "gen-$n" "$rev" || fail "publish $n failed"
done

gen_dirs=$(find "$GENS" -mindepth 1 -maxdepth 1 -type d | wc -l)
[[ $gen_dirs -le 3 ]] || fail "prune left $gen_dirs generation directories, want <=3"
current=$(readlink -f "$GENS/current")
[[ -d $current ]] || fail "current target was pruned"
[[ $current == $(readlink -f "$GENS/${revs[3]}") ]] \
  || fail "current is not the newest generation"
[[ -d $GENS/${revs[0]} ]] && fail "oldest generation was not pruned"
jq -e --arg gen gen-4 --arg rev "${revs[3]}" '
  .generation==$gen and .output_revision==$rev
' "$STATE/published-generation.json" >/dev/null \
  || fail "published-generation.json did not track newest publish"
[[ $(readlink -f "$BINDIR/omp") == $(readlink -f "$GENS/current/packages/coding-agent/scripts/ompalt") ]] \
  || fail "omp install drifted off generations/current after prune"

if [[ -n "${LIVE_OMP:-}" ]]; then
  now=$(readlink -f /home/sf/.bun/bin/omp 2>/dev/null || true)
  [[ "$now" == "$LIVE_OMP" ]] || fail "proof mutated live ~/.bun/bin/omp ($LIVE_OMP -> $now)"
fi

echo "runtime-generation-publish ok"
