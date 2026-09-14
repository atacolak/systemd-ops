#!/usr/bin/env bash
# Notification seam: system sender, addressed recipient, no borrowed identity.
# Fake hcom and systemd-ops record their argv; nothing live is touched.
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
# Fake hcom serves send, the roster read, and the bus resume, each with its
# own exit knob, so one failing subcommand cannot mask another.
cat >"$BIN_DIR/hcom" <<'EOF'
#!/usr/bin/env bash
for arg in "$@"; do printf '%s\n' "$arg" >>"$HCOM_LOG"; done
printf '%s\n' '@@end-call' >>"$HCOM_LOG"
case ${1:-} in
  list)
    if [[ ${HCOM_LIST_EXIT:-0} -eq 0 && -r $ROSTER_FILE ]]; then cat "$ROSTER_FILE"; fi
    exit "${HCOM_LIST_EXIT:-0}"
    ;;
  send) exit "${HCOM_EXIT:-0}" ;;
  r) exit "${HCOM_R_EXIT:-0}" ;;
  *) exit 0 ;;
esac
EOF
cat >"$BIN_DIR/systemd-ops" <<'EOF'
#!/usr/bin/env bash
for arg in "$@"; do printf '%s\n' "$arg" >>"$OPS_LOG"; done
printf '%s\n' '@@end-call' >>"$OPS_LOG"
cat "$SCOPE_SHOW_FILE"
EOF
chmod +x "$BIN_DIR/hcom" "$BIN_DIR/systemd-ops"

argv_of_call() {
  awk -v want="$2" 'BEGIN { n = 1 } $0 == "@@end-call" { n++; next } n == want { print }' "$1"
}

call_count() {
  awk '/^@@end-call$/ { n++ } END { print n + 0 }' "$1"
}

call_text() {
  argv_of_call "$1" "$2" | paste -sd ' ' -
}

# Index (1-based) of the first call whose argv[0] is $2; empty when absent.
first_call_with() {
  awk -v want="$2" '
    BEGIN { n = 1 }
    $0 == "@@end-call" { if (have && head == want) { print n; exit } n++; have = 0; head = ""; next }
    !have { head = $0; have = 1 }
  ' "$1"
}

count_calls_with() {
  awk -v want="$2" '
    BEGIN { n = 1 }
    $0 == "@@end-call" { if (have && head == want) c++; n++; have = 0; head = ""; next }
    !have { head = $0; have = 1 }
  END { print c + 0 }
  ' "$1"
}

call_text_with() {
  call_text "$1" "$(first_call_with "$1" "$2")"
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
ROSTER_LIVE='[{"name":"midi","base_name":"midi","status":"listening"}]'
ROSTER_STOPPED='[{"name":"midi","base_name":"midi","status":"stopped"}]'
ROSTER_EXITED='[{"name":"midi","base_name":"midi","status":"exited"}]'
ROSTER_NO_LEAD='[{"name":"gina","base_name":"gina","status":"listening"}]'

CASE=
SCOPE_SHOW=
start_case() {
  local name=$1 scope_show=$2 roster=$3
  CASE=$TMP/$name
  mkdir -p "$CASE"
  HCOM_LOG=$CASE/hcom.log
  OPS_LOG=$CASE/ops.log
  : >"$HCOM_LOG"
  : >"$OPS_LOG"
  printf '%s\n' "$scope_show" >"$CASE/scope-show.json"
  printf '%s\n' "$roster" >"$CASE/roster.json"
  export HCOM_LOG OPS_LOG
  export SCOPE_SHOW_FILE=$CASE/scope-show.json
  export ROSTER_FILE=$CASE/roster.json
  export HCOM_BIN=$BIN_DIR/hcom
  export SYSTEMD_OPS_BIN=$BIN_DIR/systemd-ops
  export SYSTEMD_OPS_SCOPE_ROOT=$CASE
  unset HCOM_EXIT HCOM_LIST_EXIT HCOM_R_EXIT
  SCOPE_SHOW=$scope_show
}

# `source` alone must not resolve a lead, read the roster, or send anything.
start_case sourcing-only "$LEAD_OK" "$ROSTER_LIVE"
# shellcheck source=/dev/null
source "$LIB"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "sourcing the library sent a notification"
[[ $(call_count "$OPS_LOG") -eq 0 ]] || fail "sourcing the library resolved a scope"

notify() {
  set +e
  system_notify "$1" "$2" "$3" >"$TMP/stdout" 2>"$TMP/stderr"
  CODE=$?
  set -e
  OUT=$(cat "$TMP/stdout")
  ERR=$(cat "$TMP/stderr")
}

# Happy path: the roster is read, the live lead is left alone, and the exact
# delivery argv is addressed with no borrowed sender. The roster read precedes
# the send.
start_case happy-path "$LEAD_OK" "$ROSTER_LIVE"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "happy path returned $CODE: $ERR"
[[ $(first_call_with "$HCOM_LOG" send) -eq 2 ]] || fail "happy path did not read the roster first: $(cat "$HCOM_LOG")"
[[ $(call_text "$HCOM_LOG" 1) == "list --all --json" ]] || fail "happy path roster argv: $(call_text "$HCOM_LOG" 1)"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "happy path made $(count_calls_with "$HCOM_LOG" send) sends"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system omp-runtime @midi --intent inform -- build finished" ]] \
  || fail "unexpected delivery argv: $(call_text_with "$HCOM_LOG" send)"
assert_addressed_send "$HCOM_LOG" "$(first_call_with "$HCOM_LOG" send)" midi
assert_no_borrowed_sender "$HCOM_LOG" "happy path"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 0 ]] || fail "happy path woke a live lead"
start_case happy-path-scope "$LEAD_OK" "$ROSTER_LIVE"
notify omp-runtime inform "build finished"
[[ $(call_text "$OPS_LOG" 1) == "--json --manager user --scope-root $CASE --cwd $CASE scope show" ]] \
  || fail "unexpected scope resolution argv: $(call_text "$OPS_LOG" 1)"

# SCOPE_ROOT is the fallback when the systemd-ops-prefixed name is unset.
start_case scope-root-fallback "$LEAD_OK" "$ROSTER_LIVE"
unset SYSTEMD_OPS_SCOPE_ROOT
SCOPE_ROOT=$CASE
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "SCOPE_ROOT fallback returned $CODE: $ERR"
[[ $(call_text "$OPS_LOG" 1) == "--json --manager user --scope-root $CASE --cwd $CASE scope show" ]] \
  || fail "SCOPE_ROOT fallback argv: $(call_text "$OPS_LOG" 1)"

# Intent vocabulary: ack passes through, notice is refused.
start_case intent-ack "$LEAD_OK" "$ROSTER_LIVE"
notify cpa-upstream-watch ack "digest ready"
[[ $CODE -eq 0 ]] || fail "ack intent returned $CODE: $ERR"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system cpa-upstream-watch @midi --intent ack -- digest ready" ]] \
  || fail "unexpected ack argv: $(call_text_with "$HCOM_LOG" send)"

start_case intent-unknown "$LEAD_OK" "$ROSTER_LIVE"
notify omp-runtime notice "digest ready"
[[ $CODE -ne 0 ]] || fail "unknown intent was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "unknown intent reached hcom"

# Source ids: the operation-stem convention is accepted, reserved and
# out-of-charset ids are refused.
start_case source-stem "$LEAD_OK" "$ROSTER_LIVE"
notify systemd-ops:runtime-run inform "generation published"
[[ $CODE -eq 0 ]] || fail "operation-stem source id returned $CODE: $ERR"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system systemd-ops:runtime-run @midi --intent inform -- generation published" ]] \
  || fail "unexpected operation-stem argv: $(call_text_with "$HCOM_LOG" send)"

start_case source-overseer "$LEAD_OK" "$ROSTER_LIVE"
notify overseer inform "generation published"
[[ $CODE -ne 0 ]] || fail "overseer source id was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "overseer source id reached hcom"

start_case source-charset "$LEAD_OK" "$ROSTER_LIVE"
notify "Omp Runtime" inform "generation published"
[[ $CODE -ne 0 ]] || fail "out-of-charset source id was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "out-of-charset source id reached hcom"

# Unresolvable lead: refused, and specifically not broadcast.
start_case lead-missing '{"ok":true,"data":{"coordination":{}}}' "$ROSTER_LIVE"
notify omp-runtime inform "build finished"
[[ $CODE -ne 0 ]] || fail "unbound lead was accepted"
[[ $ERR == *"no coordination.lead bound in $CASE"* ]] || fail "unbound lead stderr: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "unbound lead reached hcom"
if grep -q '^send$' "$HCOM_LOG"; then fail "unbound lead attempted a broadcast"; fi

start_case lead-null '{"ok":true,"data":{"coordination":{"lead":null}}}' "$ROSTER_LIVE"
notify omp-runtime inform "build finished"
[[ $CODE -ne 0 ]] || fail "null lead was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "null lead reached hcom"

start_case lead-short '{"ok":true,"data":{"coordination":{"lead":"hcom:mid"}}}' "$ROSTER_LIVE"
notify omp-runtime inform "build finished"
[[ $CODE -ne 0 ]] || fail "three-letter lead handle was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "three-letter lead handle reached hcom"

start_case lead-not-hcom '{"ok":true,"data":{"coordination":{"lead":"midi"}}}' "$ROSTER_LIVE"
notify omp-runtime inform "build finished"
[[ $CODE -ne 0 ]] || fail "non-hcom lead handle was accepted"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "non-hcom lead handle reached hcom"

# A failed delivery is reported as a failure.
start_case send-failure "$LEAD_OK" "$ROSTER_LIVE"
export HCOM_EXIT=4
notify omp-runtime inform "build finished"
unset HCOM_EXIT
[[ $CODE -eq 4 ]] || fail "failed send returned $CODE, want 4"
[[ $ERR == *"@midi"* ]] || fail "failed send did not name the recipient: $ERR"

# Wake policy: the HCOM roster decides. A lead that is not live is resumed once
# through the bus; a live or absent lead is left alone; every wake failure is
# best effort and never blocks the delivery.
start_case wake-stopped "$LEAD_OK" "$ROSTER_STOPPED"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "wake path returned $CODE: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 3 ]] || fail "wake path made $(call_count "$HCOM_LOG") hcom calls"
[[ $(call_text "$HCOM_LOG" 1) == "list --all --json" ]] || fail "wake path first call: $(call_text "$HCOM_LOG" 1)"
[[ $(call_text "$HCOM_LOG" 2) == "r midi --go" ]] || fail "wake path second call: $(call_text "$HCOM_LOG" 2)"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 1 ]] || fail "wake path made $(count_calls_with "$HCOM_LOG" r) wake calls"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system omp-runtime @midi --intent inform -- build finished" ]] \
  || fail "wake path delivery argv: $(call_text_with "$HCOM_LOG" send)"

start_case wake-exited "$LEAD_OK" "$ROSTER_EXITED"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "exited lead returned $CODE: $ERR"
[[ $(call_text "$HCOM_LOG" 2) == "r midi --go" ]] || fail "exited lead wake call: $(call_text "$HCOM_LOG" 2)"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 1 ]] || fail "exited lead made $(count_calls_with "$HCOM_LOG" r) wake calls"

start_case wake-listening "$LEAD_OK" "$ROSTER_LIVE"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "live lead path returned $CODE: $ERR"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 0 ]] || fail "live lead was woken: $(call_text_with "$HCOM_LOG" r)"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "live lead blocked delivery"

start_case wake-not-on-roster "$LEAD_OK" "$ROSTER_NO_LEAD"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "lead absent from the roster returned $CODE: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 2 ]] || fail "lead absent from the roster made $(call_count "$HCOM_LOG") hcom calls"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 0 ]] || fail "a lead absent from the roster was woken"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "a lead absent from the roster blocked delivery"

# The roster row is matched on base_name or on a name ending in -<name>.
start_case wake-tagged-name "$LEAD_OK" '[{"name":"omp-midi","status":"stopped"}]'
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "tagged lead row returned $CODE: $ERR"
[[ $(call_text "$HCOM_LOG" 2) == "r midi --go" ]] || fail "tagged lead row wake call: $(call_text "$HCOM_LOG" 2)"

# A roster row with no status at all is treated as not live.
start_case wake-empty-status "$LEAD_OK" '[{"name":"midi","base_name":"midi"}]'
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "empty roster status returned $CODE: $ERR"
[[ $(call_text "$HCOM_LOG" 2) == "r midi --go" ]] || fail "empty roster status wake call: $(call_text "$HCOM_LOG" 2)"

# A failed resume is not a failed notification.
start_case wake-resume-failure "$LEAD_OK" "$ROSTER_STOPPED"
export HCOM_R_EXIT=3
notify omp-runtime inform "build finished"
unset HCOM_R_EXIT
[[ $CODE -eq 0 ]] || fail "failed resume returned $CODE: $ERR"
[[ -z $ERR ]] || fail "failed resume was not silent: $ERR"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 1 ]] || fail "failed resume was not attempted exactly once"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "failed resume blocked delivery"

# A failed or unreadable roster leaves the send unchanged and wakes nobody.
start_case wake-roster-failure "$LEAD_OK" "$ROSTER_STOPPED"
export HCOM_LIST_EXIT=7
notify omp-runtime inform "build finished"
unset HCOM_LIST_EXIT
[[ $CODE -eq 0 ]] || fail "failed roster read returned $CODE: $ERR"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 0 ]] || fail "failed roster read still attempted a wake"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "failed roster read blocked delivery"

start_case wake-roster-missing "$LEAD_OK" "$ROSTER_STOPPED"
export ROSTER_FILE=$CASE/absent-roster.json
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "missing roster returned $CODE: $ERR"
[[ -z $ERR ]] || fail "missing roster was not silent: $ERR"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 0 ]] || fail "missing roster still attempted a wake"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "missing roster blocked delivery"

start_case wake-roster-garbage "$LEAD_OK" 'not json at all'
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "unparsable roster returned $CODE: $ERR"
[[ -z $ERR ]] || fail "unparsable roster was not silent: $ERR"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 0 ]] || fail "unparsable roster still attempted a wake"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "unparsable roster blocked delivery"

echo "notify-provenance ok"
