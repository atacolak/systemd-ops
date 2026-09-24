#!/usr/bin/env bash
# Midi canary ACK is the only promote. Throwaway scope; no live units.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
TMP=$(mktemp -d)
SCOPE=$TMP/scope
STEM=managed-omp-runtime-integrator
HOME_DIR=$SCOPE/.systemd-ops/operations/$STEM
STATE_DIR=$HOME_DIR/state
DEPLOY=$STATE_DIR/deployment.json
PUBLISHED=$STATE_DIR/published-generation.json
GENS=$TMP/generations
UNIT_DIR=$TMP/xdg/systemd/user
UNIT=$UNIT_DIR/$STEM.service
AGENT=$TMP/agents
NOTIFY_LOG=$TMP/notify.argv
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { jq -e '.ok == true' <<<"$1" >/dev/null || fail "expected ok: $1"; }
rejected() { jq -e '.ok == false' <<<"$1" >/dev/null || fail "expected rejection: $1"; }

command -v jq >/dev/null || fail "jq required"
command -v sha256sum >/dev/null || fail "sha256sum required"
[[ -x $BIN ]] || fail "missing $BIN"

PREV_SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
CAND_SHA=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
PREV_COMPOSE=cccccccccccccccccccccccccccccccccccccccc
CAND_COMPOSE=dddddddddddddddddddddddddddddddddddddddd
PREV_LABEL=omp@aaaaaaaa
CAND_LABEL=omp@bbbbbbbb
FIRST_PREV=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
FIRST_CAND=ffffffffffffffffffffffffffffffffffffffff
FIRST_PREV_COMPOSE=1111111111111111111111111111111111111111
FIRST_CAND_COMPOSE=2222222222222222222222222222222222222222
UNSET_PREV=3333333333333333333333333333333333333333
UNSET_CAND=4444444444444444444444444444444444444444
UNSET_PREV_COMPOSE=5555555555555555555555555555555555555555
UNSET_CAND_COMPOSE=6666666666666666666666666666666666666666

LIVE_GENS=/home/sf/worktrees/omp/generations
LIVE_OMP=/home/sf/.bun/bin/omp
live_gens_before=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_before=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)

mkdir -p "$HOME_DIR" "$UNIT_DIR" "$SCOPE/.systemd-ops" "$AGENT/.omp/agents" "$TMP/bin" "$STATE_DIR"
printf '%s\n' 'runtime-integrator role' >"$AGENT/.omp/agents/runtime-integrator.md"

cat >"$SCOPE/.systemd-ops/scope.toml" <<EOF
[scope]
id = "proof"
owned = ["managed-*"]

[automation]
agent_root = "$AGENT"
EOF

cat >"$HOME_DIR/automation.toml" <<'EOF'
version = 1
agent = "runtime-integrator"
EOF

cat >"$UNIT" <<EOF
# Managed by systemd-ops
[Unit]
Description=runtime integrator
[Service]
Type=oneshot
ExecStart=/bin/true
WorkingDirectory=$SCOPE
EOF

cat >"$TMP/bin/systemctl" <<EOF
#!/bin/sh
while [ "\${1:-}" = --user ]; do shift; done
case " \$* " in
  *" list-unit-files "*)
    printf '%s\n' '[{"unit_file":"$STEM.service","state":"static","preset":"enabled"}]'
    ;;
  *" list-units "*|*" list-timers "*)
    printf '%s\n' '[]'
    ;;
  *" show "*)
    printf 'LoadState=loaded\nActiveState=inactive\nSubState=dead\nFragmentPath=%s\n' "$UNIT"
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

write_ompalt() {
  local dest=$1
  mkdir -p "$dest/packages/coding-agent/scripts"
  cat >"$dest/packages/coding-agent/scripts/ompalt" <<'SCRIPT'
#!/bin/sh
echo ompalt-fixture
exit 0
SCRIPT
  chmod 755 "$dest/packages/coding-agent/scripts/ompalt"
}

seed_promoted_and_candidate() {
  local prev=$1 cand=$2 prev_compose=$3 cand_compose=$4
  rm -rf "$GENS"
  mkdir -p "$GENS"
  write_ompalt "$GENS/$prev"
  write_ompalt "$GENS/$cand"
  ln -s "$GENS/$prev" "$GENS/current"
  ln -sf "$GENS/$prev/packages/coding-agent/scripts/ompalt" "$TMP/bin/omp"
  jq -n \
    --arg generation "$prev_compose" \
    --arg output_revision "$prev" \
    --arg published_at "2026-01-01T00:00:00Z" \
    --arg path "$(readlink -f "$GENS/$prev")" \
    '{version:1,generation:$generation,output_revision:$output_revision,published_at:$published_at,path:$path}' \
    >"$PUBLISHED"
  jq -n \
    --arg generations_root "$(readlink -f "$GENS")" \
    --arg prev_gen "$prev_compose" \
    --arg prev_rev "$prev" \
    --arg cand_gen "$cand_compose" \
    --arg cand_rev "$cand" \
    '{
      version: 1,
      generations_root: $generations_root,
      promoted: {
        generation: $prev_gen,
        output_revision: $prev_rev,
        promoted_at: "2026-01-01T00:00:00Z"
      },
      candidate: {
        generation: $cand_gen,
        output_revision: $cand_rev,
        composed_at: "2026-01-02T00:00:00Z",
        phase: "candidate"
      }
    }' >"$DEPLOY"
}

current_sha() {
  basename "$(readlink -f "$GENS/current")"
}

published_sha() {
  jq -er '.output_revision' "$PUBLISHED"
}

assert_pointers() {
  local sha=$1 label=$2
  [[ $(current_sha) == "$sha" ]] || fail "$label: current is $(current_sha), expected $sha"
  [[ $(published_sha) == "$sha" ]] || fail "$label: published-generation.json is $(published_sha), expected $sha"
  jq -e --arg sha "$sha" '.output_revision == $sha' "$PUBLISHED" >/dev/null \
    || fail "$label: published-generation.json drifted"
}

assert_phase_candidate() {
  local label=$1
  jq -e '.candidate.phase == "candidate"' "$DEPLOY" >/dev/null \
    || fail "$label: candidate.phase is $(jq -er '.candidate.phase' "$DEPLOY")"
  jq -e '.candidate.phase != "rolling_out"' "$DEPLOY" >/dev/null \
    || fail "$label: wrote rolling_out"
  if grep -q '"rolling_out"' "$DEPLOY"; then
    fail "$label: deployment.json contains rolling_out"
  fi
}

export PATH=$TMP/bin:$PATH
export HOME=$TMP
export XDG_CONFIG_HOME=$TMP/xdg
export SYSTEMD_OPS_STATE_DIR=$TMP/state
export SYSTEMD_OPS_SCOPE_ROOT=$SCOPE
export SYSTEMD_OPS_OPERATION=$STEM
export SYSTEM_NOTIFY_LIB=$TMP/bin/notify-stub
export OMP_INSTALL=$TMP/bin/omp
export GENERATIONS_ROOT=$GENS
export RUNTIME_PROMOTE=$ROOT/dogfood/lib/runtime-promote

json() {
  "$BIN" --json --manager user --write-prefix 'managed-*' --scope-root "$SCOPE" "$@"
}

ack() {
  json automation plan-deploy-canary-ack --spec "$1"
}

apply_token() {
  json automation apply --plan-token "$1"
}

apply_token_without_omp_install() {
  env -u OMP_INSTALL "$BIN" --json --manager user --write-prefix 'managed-*' --scope-root "$SCOPE" automation apply --plan-token "$1"
}

ack_apply() {
  local planned
  planned=$(ack "$1")
  ok "$planned"
  apply_token "$(jq -er '.data.plan_token' <<<"$planned")"
}

midi_pass_spec() {
  local generation=$1 observed=$2
  jq -nc --arg gen "$generation" --arg obs "$observed" '{
    version: 1,
    generation: $gen,
    actor: "midi",
    result: "pass",
    observed_generation: $obs,
    continuity: {actor: "midi", session_id: "sess-canary-1"}
  }'
}

midi_fail_spec() {
  jq -nc --arg gen "$CAND_SHA" '{
    version: 1,
    generation: $gen,
    actor: "midi",
    result: "fail",
    observed_generation: $gen,
    continuity: {actor: "midi", session_id: "sess-canary-1"}
  }'
}

midi_recovered_spec() {
  local sha=$1
  jq -nc --arg gen "$sha" '{
    version: 1,
    generation: $gen,
    actor: "midi",
    result: "recovered",
    observed_generation: $gen,
    continuity: {actor: "midi", session_id: "sess-recovered-1"}
  }'
}

assert_fail_ack_effects() {
  local label=$1 reason=$2
  assert_pointers "$PREV_SHA" "$label"
  assert_phase_candidate "$label"
  jq -e '.canary.result == "fail"' "$DEPLOY" >/dev/null \
    || fail "$label: canary.result is not fail: $(cat "$DEPLOY")"
  jq -e --arg sha "$PREV_SHA" --arg label "$PREV_LABEL" '
    .recovery.target.sha == $sha and .recovery.target.label == $label
  ' "$DEPLOY" >/dev/null \
    || fail "$label: recovery.target mismatch: $(cat "$DEPLOY")"
  [[ -f $NOTIFY_LOG ]] || fail "$label: notify stub was not invoked"
  grep -qx 'omp-runtime' "$NOTIFY_LOG" || fail "$label: notify source is not omp-runtime: $(cat "$NOTIFY_LOG")"
  grep -qx inform "$NOTIFY_LOG" || fail "$label: notify intent is not inform: $(cat "$NOTIFY_LOG")"
  grep -q "$CAND_LABEL" "$NOTIFY_LOG" || fail "$label: notify missing failed candidate $CAND_LABEL: $(cat "$NOTIFY_LOG")"
  grep -q "$PREV_LABEL" "$NOTIFY_LOG" || fail "$label: notify message missing $PREV_LABEL: $(cat "$NOTIFY_LOG")"
  grep -q "$PREV_SHA" "$NOTIFY_LOG" || fail "$label: notify message missing recovery sha: $(cat "$NOTIFY_LOG")"
  grep -Fq "$reason" "$NOTIFY_LOG" || fail "$label: notify missing refuse reason '$reason': $(cat "$NOTIFY_LOG")"
}

seed_promoted_and_candidate "$PREV_SHA" "$CAND_SHA" "$PREV_COMPOSE" "$CAND_COMPOSE"
assert_pointers "$PREV_SHA" "seed"
rm -f "$NOTIFY_LOG"

applied=$(ack_apply "$(midi_fail_spec)")
ok "$applied"
assert_fail_ack_effects "result=fail" "result=fail"

rm -f "$NOTIFY_LOG"
applied=$(ack_apply "$(jq -nc --arg gen "$CAND_SHA" '{
  version: 1,
  generation: $gen,
  actor: "hera",
  result: "pass",
  observed_generation: $gen,
  continuity: {actor: "midi", session_id: "sess-canary-1"}
}')")
ok "$applied"
assert_fail_ack_effects "actor not midi" "actor is not midi"

rm -f "$NOTIFY_LOG"
applied=$(ack_apply "$(jq -nc --arg gen "$CAND_SHA" --arg obs "$PREV_SHA" '{
  version: 1,
  generation: $gen,
  actor: "midi",
  result: "pass",
  observed_generation: $obs,
  continuity: {actor: "midi", session_id: "sess-canary-1"}
}')")
ok "$applied"
assert_fail_ack_effects "observed_generation mismatch" "observed_generation mismatch"

rm -f "$NOTIFY_LOG"
applied=$(ack_apply "$(jq -nc --arg gen "$CAND_SHA" '{
  version: 1,
  generation: $gen,
  actor: "midi",
  result: "pass",
  observed_generation: $gen,
  continuity: {actor: "midi", session_id: ""}
}')")
ok "$applied"
assert_fail_ack_effects "empty continuity" "empty continuity"

before_current=$(readlink -f "$GENS/current")
before_published=$(cat "$PUBLISHED")
before_deploy=$(cat "$DEPLOY")
pass_hold=$(ack "$(midi_pass_spec "$CAND_SHA" "$CAND_SHA")" || true)
rejected "$pass_hold"
[[ $(cat "$PUBLISHED") == "$before_published" ]] || fail "pass-before-recovered rewrote published-generation.json"
[[ $(readlink -f "$GENS/current") == "$before_current" ]] || fail "pass-before-recovered flipped current"
[[ $(cat "$DEPLOY") == "$before_deploy" ]] || fail "pass-before-recovered mutated deployment.json"
assert_phase_candidate "pass-before-recovered"
assert_pointers "$PREV_SHA" "pass-before-recovered"

applied=$(ack_apply "$(midi_recovered_spec "$PREV_SHA")")
ok "$applied"
assert_pointers "$PREV_SHA" "recovered"
assert_phase_candidate "recovered"
jq -e '
  .recovery.midi_alive_at != null and
  (.recovery.midi_alive_at | type == "string") and
  (.recovery.midi_alive_at | length > 0) and
  (.recovery.midi_alive_at | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T"))
' "$DEPLOY" >/dev/null \
  || fail "recovered did not record recovery.midi_alive_at: $(cat "$DEPLOY")"
jq -e --arg sha "$PREV_SHA" --arg label "$PREV_LABEL" '
  .recovery.target.sha == $sha and .recovery.target.label == $label
' "$DEPLOY" >/dev/null \
  || fail "recovered rewrote recovery.target: $(cat "$DEPLOY")"

applied=$(ack_apply "$(midi_pass_spec "$CAND_SHA" "$CAND_SHA")")
ok "$applied"
assert_pointers "$CAND_SHA" "pass after recovered"
jq -e --arg sha "$CAND_SHA" --arg gen "$CAND_COMPOSE" '
  .candidate.phase == "rolling_out" and
  .candidate.output_revision == $sha and
  .canary.result == "pass" and
  .promoted.output_revision == $sha
' "$DEPLOY" >/dev/null \
  || fail "pass after recovered did not roll out: $(cat "$DEPLOY")"
jq -e --arg sha "$CAND_SHA" --arg gen "$CAND_COMPOSE" '
  .output_revision == $sha and .generation == $gen
' "$PUBLISHED" >/dev/null \
  || fail "published-generation.json after pass: $(cat "$PUBLISHED")"

after_pass_current=$(readlink -f "$GENS/current")
after_pass_published=$(cat "$PUBLISHED")
after_pass_deploy=$(cat "$DEPLOY")
applied=$(ack_apply "$(midi_pass_spec "$CAND_SHA" "$CAND_SHA")")
ok "$applied"
[[ $(readlink -f "$GENS/current") == "$after_pass_current" ]] || fail "second pass flipped current"
[[ $(cat "$PUBLISHED") == "$after_pass_published" ]] || fail "second pass rewrote published-generation.json"
[[ $(jq -c '{phase:.candidate.phase,sha:.candidate.output_revision,pub:(.promoted.output_revision)}' "$DEPLOY") == \
   $(jq -c '{phase:.candidate.phase,sha:.candidate.output_revision,pub:(.promoted.output_revision)}' <<<"$after_pass_deploy") ]] \
  || fail "second pass mutated promote pointers: $(cat "$DEPLOY")"

seed_promoted_and_candidate "$FIRST_PREV" "$FIRST_CAND" "$FIRST_PREV_COMPOSE" "$FIRST_CAND_COMPOSE"
assert_pointers "$FIRST_PREV" "first-try seed"
applied=$(ack_apply "$(midi_pass_spec "$FIRST_CAND" "$FIRST_CAND")")
ok "$applied"
assert_pointers "$FIRST_CAND" "first-try pass"
jq -e --arg sha "$FIRST_CAND" --arg gen "$FIRST_CAND_COMPOSE" '
  .candidate.phase == "rolling_out" and
  .candidate.output_revision == $sha and
  .canary.result == "pass"
' "$DEPLOY" >/dev/null \
  || fail "first-try pass did not promote: $(cat "$DEPLOY")"
jq -e --arg sha "$FIRST_CAND" --arg gen "$FIRST_CAND_COMPOSE" '
  .output_revision == $sha and .generation == $gen
' "$PUBLISHED" >/dev/null \
  || fail "first-try published-generation.json: $(cat "$PUBLISHED")"

seed_promoted_and_candidate "$UNSET_PREV" "$UNSET_CAND" "$UNSET_PREV_COMPOSE" "$UNSET_CAND_COMPOSE"
assert_pointers "$UNSET_PREV" "pass without parent OMP_INSTALL seed"
before_published=$(cat "$PUBLISHED")
planned=$(ack "$(midi_pass_spec "$UNSET_CAND" "$UNSET_CAND")")
ok "$planned"
applied=$(apply_token_without_omp_install "$(jq -er '.data.plan_token' <<<"$planned")" || true)
ok "$applied"
[[ $(cat "$PUBLISHED") != "$before_published" ]] \
  || fail "pass without parent OMP_INSTALL did not rewrite published-generation.json"
assert_pointers "$UNSET_CAND" "pass without parent OMP_INSTALL"
jq -e --arg sha "$UNSET_CAND" --arg gen "$UNSET_CAND_COMPOSE" '
  .output_revision == $sha and .generation == $gen
' "$PUBLISHED" >/dev/null \
  || fail "published-generation.json after pass without parent OMP_INSTALL: $(cat "$PUBLISHED")"
live_omp_mid=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)
[[ $live_omp_mid == "$live_omp_before" ]] || fail "pass without parent OMP_INSTALL mutated live omp"

live_gens_after=$(readlink -f "$LIVE_GENS/current" 2>/dev/null || true)
live_omp_after=$(readlink -f "$LIVE_OMP" 2>/dev/null || true)
[[ $live_gens_after == "$live_gens_before" ]] || fail "mutated live generations/current"
[[ $live_omp_after == "$live_omp_before" ]] || fail "mutated live omp install"

if grep -nE 'recycle_actor|github.com|api.github' "$ROOT/src/automation.rs" "$ROOT/src/bin/systemd_ops.rs"; then
  fail "src gained GitHub or recycle_actor calls"
fi
if grep -nE 'recycle_actor|hcom r' "$ROOT/src/automation.rs" "$ROOT/src/bin/systemd_ops.rs"; then
  fail "src gained recycle_actor or hcom r"
fi

echo "deploy-canary ok"
