#!/usr/bin/env bash
# Notification seam: system sender, addressed recipient, no borrowed identity.
# Fake hcom/actor/systemd-ops record their argv; nothing live is touched.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${NOTIFY_LIB:-$ROOT/dogfood/lib/system-notify}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"

BIN_DIR=$TMP/bin
mkdir -p "$BIN_DIR"

# Each fake appends its argv one argument per line and closes the call with
# @@end-call, so a literal "--" argument stays readable as an argument.
cat >"$BIN_DIR/hcom" <<'EOF'
#!/usr/bin/env bash
for arg in "$@"; do printf '%s\n' "$arg" >>"$HCOM_LOG"; done
printf '%s\n' '@@end-call' >>"$HCOM_LOG"
exit "${HCOM_EXIT:-0}"
EOF
cat >"$BIN_DIR/systemd-ops" <<'EOF'
#!/usr/bin/env bash
for arg in "$@"; do printf '%s\n' "$arg" >>"$OPS_LOG"; done
printf '%s\n' '@@end-call' >>"$OPS_LOG"
cat "$SCOPE_SHOW_FILE"
EOF
cat >"$BIN_DIR/actor" <<'EOF'
#!/usr/bin/env bash
for arg in "$@"; do printf '%s\n' "$arg" >>"$ACTOR_LOG"; done
printf '%s\n' '@@end-call' >>"$ACTOR_LOG"
if [[ ${1:-} == status ]]; then
  cat "$ACTOR_STATUS_FILE"
fi
exit 0
EOF
chmod +x "$BIN_DIR/hcom" "$BIN_DIR/systemd-ops" "$BIN_DIR/actor"

argv_of_call() {
  awk -v want="$2" 'BEGIN { n = 1 } $0 == "@@end-call" { n++; next } n == want { print }' "$1"
}

call_count() {
  awk '/^@@end-call$/ { n++ } END { print n + 0 }' "$1"
}

call_text() {
  argv_of_call "$1" "$2" | paste -sd ' ' -
}

assert_no_borrowed_sender() {
  if grep -qE -e '--from' -e '--as-instance' -e '--bigboss' "$1"; then
    fail "$2 recorded a borrowed sender: $(cat "$1")"
  fi
}

assert_addressed_send() {
  local log=$1 index=$2 recipient=$3
  local -a argv=()
  mapfile -t argv < <(argv_of_call "$log" "$index")
  [[ ${argv[0]:-} == send ]] || fail "call $index is not a send: ${argv[*]:-}"
  [[ ${argv[3]:-} == "@$recipient" ]] || fail "call $index is not addressed to @$recipient: ${argv[*]}"
}

LEAD_OK='{"ok":true,"data":{"coordination":{"lead":"hcom:midi"}}}'
ACTOR_ACTIVE='{"ok":true,"actor":{"name":"midi","lifecycle":"active"}}'
ACTOR_STOPPED='{"ok":true,"actor":{"name":"midi","lifecycle":"stopped"}}'

CASE=
SCOPE_SHOW=
ACTOR_STATUS=
start_case() {
  local name=$1 scope_show=$2 actor_status=$3
  CASE=$TMP/$name
  mkdir -p "$CASE"
  HCOM_LOG=$CASE/hcom.log
  OPS_LOG=$CASE/ops.log
  ACTOR_LOG=$CASE/actor.log
  : >"$HCOM_LOG"
  : >"$OPS_LOG"
  : >"$ACTOR_LOG"
  printf '%s\n' "$scope_show" >"$CASE/scope-show.json"
  printf '%s\n' "$actor_status" >"$CASE/actor-status.json"
  export HCOM_LOG OPS_LOG ACTOR_LOG
  export SCOPE_SHOW_FILE=$CASE/scope-show.json
  export ACTOR_STATUS_FILE=$CASE/actor-status.json
  export HCOM_BIN=$BIN_DIR/hcom
  export SYSTEMD_OPS_BIN=$BIN_DIR/systemd-ops
  export ACTOR_BIN=$BIN_DIR/actor
  export SYSTEMD_OPS_SCOPE_ROOT=$CASE
  unset HCOM_EXIT
  SCOPE_SHOW=$scope_show
  ACTOR_STATUS=$actor_status
}

# `source` alone must not resolve a lead, wake an actor, or send anything.
start_case sourcing-only "$LEAD_OK" "$ACTOR_ACTIVE"
# shellcheck source=/dev/null
source "$LIB"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "sourcing the library sent a notification"
[[ $(call_count "$OPS_LOG") -eq 0 ]] || fail "sourcing the library resolved a scope"
[[ $(call_count "$ACTOR_LOG") -eq 0 ]] || fail "sourcing the library touched an actor"

notify() {
  set +e
  system_notify "$1" "$2" "$3" >"$TMP/stdout" 2>"$TMP/stderr"
  CODE=$?
  set -e
  OUT=$(cat "$TMP/stdout")
  ERR=$(cat "$TMP/stderr")
}

# Happy path: the exact delivery argv, addressed, no borrowed sender.
start_case happy-path "$LEAD_OK" "$ACTOR_ACTIVE"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "happy path returned $CODE: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 1 ]] || fail "happy path sent $(call_count "$HCOM_LOG") hcom calls"
[[ $(call_text "$HCOM_LOG" 1) == "send --as-system omp-runtime @midi --intent inform -- build finished" ]] \
  || fail "unexpected delivery argv: $(call_text "$HCOM_LOG" 1)"
assert_addressed_send "$HCOM_LOG" 1 midi
assert_no_borrowed_sender "$HCOM_LOG" "happy path"
start_case happy-path-scope "$LEAD_OK" "$ACTOR_ACTIVE"
notify omp-runtime inform "build finished"
[[ $(call_text "$OPS_LOG" 1) == "--json --manager user --scope-root $CASE --cwd $CASE scope show" ]] \
  || fail "unexpected scope resolution argv: $(call_text "$OPS_LOG" 1)"

# SCOPE_ROOT is the fallback when the systemd-ops-prefixed name is unset.
start_case scope-root-fallback "$LEAD_OK" "$ACTOR_ACTIVE"
unset SYSTEMD_OPS_SCOPE_ROOT
SCOPE_ROOT=$CASE
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "SCOPE_ROOT fallback returned $CODE: $ERR"
[[ $(call_text "$OPS_LOG" 1) == "--json --manager user --scope-root $CASE --cwd $CASE scope show" ]] \
  || fail "SCOPE_ROOT fallback argv: $(call_text "$OPS_LOG" 1)"

# Intent vocabulary: ack passes through, notice is refused.
start_case intent-ack "$LEAD_OK" "$ACTOR_ACTIVE"
notify cpa-upstream-watch ack "digest ready"
[[ $CODE -eq 0 ]] || fail "ack intent returned $CODE: $ERR"
[[ $(call_text "$HCOM_LOG" 1) == "send --as-system cpa-upstream-watch @midi --intent ack -- digest ready" ]] \
  || fail "unexpected ack argv: $(call_text "$HCOM_LOG" 1)"

start_case intent-unknown "$LEAD_OK" "$ACTOR_ACTIVE"
notify omp-runtime notice "digest ready"
[[ $CODE -ne 0 ]] || fail "unknown intent was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "unknown intent reached hcom"

# Source ids: the operation-stem convention is accepted, reserved and
# out-of-charset ids are refused.
start_case source-stem "$LEAD_OK" "$ACTOR_ACTIVE"
notify systemd-ops:runtime-run inform "generation published"
[[ $CODE -eq 0 ]] || fail "operation-stem source id returned $CODE: $ERR"
[[ $(call_text "$HCOM_LOG" 1) == "send --as-system systemd-ops:runtime-run @midi --intent inform -- generation published" ]] \
  || fail "unexpected operation-stem argv: $(call_text "$HCOM_LOG" 1)"

start_case source-overseer "$LEAD_OK" "$ACTOR_ACTIVE"
notify overseer inform "generation published"
[[ $CODE -ne 0 ]] || fail "overseer source id was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "overseer source id reached hcom"

start_case source-charset "$LEAD_OK" "$ACTOR_ACTIVE"
notify "Omp Runtime" inform "generation published"
[[ $CODE -ne 0 ]] || fail "out-of-charset source id was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "out-of-charset source id reached hcom"

# Unresolvable lead: refused, and specifically not broadcast.
start_case lead-missing '{"ok":true,"data":{"coordination":{}}}' "$ACTOR_ACTIVE"
notify omp-runtime inform "build finished"
[[ $CODE -ne 0 ]] || fail "unbound lead was accepted"
[[ $ERR == *"no coordination.lead bound in $CASE"* ]] || fail "unbound lead stderr: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "unbound lead reached hcom"
if grep -q '^send$' "$HCOM_LOG"; then fail "unbound lead attempted a broadcast"; fi

start_case lead-null '{"ok":true,"data":{"coordination":{"lead":null}}}' "$ACTOR_ACTIVE"
notify omp-runtime inform "build finished"
[[ $CODE -ne 0 ]] || fail "null lead was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "null lead reached hcom"

start_case lead-short '{"ok":true,"data":{"coordination":{"lead":"hcom:mid"}}}' "$ACTOR_ACTIVE"
notify omp-runtime inform "build finished"
[[ $CODE -ne 0 ]] || fail "three-letter lead handle was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "three-letter lead handle reached hcom"

start_case lead-not-hcom '{"ok":true,"data":{"coordination":{"lead":"midi"}}}' "$ACTOR_ACTIVE"
notify omp-runtime inform "build finished"
[[ $CODE -ne 0 ]] || fail "non-hcom lead handle was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "non-hcom lead handle reached hcom"

# A failed delivery is reported as a failure.
start_case send-failure "$LEAD_OK" "$ACTOR_ACTIVE"
export HCOM_EXIT=4
notify omp-runtime inform "build finished"
unset HCOM_EXIT
[[ $CODE -eq 4 ]] || fail "failed send returned $CODE, want 4"
[[ $ERR == *"@midi"* ]] || fail "failed send did not name the recipient: $ERR"

# Wake policy: a stopped lead is restored once, a live lead is left alone,
# and a missing actor binary is skipped silently.
start_case wake-stopped "$LEAD_OK" "$ACTOR_STOPPED"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "wake path returned $CODE: $ERR"
[[ $(call_count "$ACTOR_LOG") -eq 2 ]] || fail "wake path made $(call_count "$ACTOR_LOG") actor calls"
[[ $(call_text "$ACTOR_LOG" 1) == "status midi" ]] || fail "wake path first call: $(call_text "$ACTOR_LOG" 1)"
[[ $(call_text "$ACTOR_LOG" 2) == "r midi" ]] || fail "wake path second call: $(call_text "$ACTOR_LOG" 2)"
[[ $(call_count "$HCOM_LOG") -eq 1 ]] || fail "wake path did not deliver"

start_case wake-active "$LEAD_OK" "$ACTOR_ACTIVE"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "live lead path returned $CODE: $ERR"
[[ $(call_count "$ACTOR_LOG") -eq 1 ]] || fail "live lead was woken: $(call_text "$ACTOR_LOG" 2)"
[[ $(call_text "$ACTOR_LOG" 1) == "status midi" ]] || fail "live lead status call: $(call_text "$ACTOR_LOG" 1)"

start_case actor-absent "$LEAD_OK" "$ACTOR_ACTIVE"
export ACTOR_BIN=$CASE/absent-actor
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "absent actor binary returned $CODE: $ERR"
[[ -z $ERR ]] || fail "absent actor binary was not silent: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 1 ]] || fail "absent actor binary blocked delivery"

echo "notify-provenance ok"
