#!/usr/bin/env bash
# Notification seam: system sender, addressed recipient, no borrowed identity.
# Fake hcom and systemd-ops record their argv; nothing live is touched.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SELF=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
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
# Fake hcom serves send, the roster read, and the bus resume, each with its own
# exit and sleep knob, so one failing or hung subcommand cannot mask another.
# Fake systemd-ops serves `scope show` with an exit knob of its own.
cat >"$BIN_DIR/hcom" <<'EOF'
#!/usr/bin/env bash
for arg in "$@"; do printf '%s\n' "$arg" >>"$HCOM_LOG"; done
printf '%s\n' '@@end-call' >>"$HCOM_LOG"
case ${1:-} in
  list)
    sleep "${HCOM_LIST_SLEEP:-0}"
    if [[ ${HCOM_LIST_EXIT:-0} -eq 0 && -r $ROSTER_FILE ]]; then cat "$ROSTER_FILE"; fi
    exit "${HCOM_LIST_EXIT:-0}"
    ;;
  send)
    sleep "${HCOM_SEND_SLEEP:-0}"
    exit "${HCOM_EXIT:-0}"
    ;;
  r)
    sleep "${HCOM_R_SLEEP:-0}"
    exit "${HCOM_R_EXIT:-0}"
    ;;
  *) exit 0 ;;
esac
EOF
cat >"$BIN_DIR/systemd-ops" <<'EOF'
#!/usr/bin/env bash
for arg in "$@"; do printf '%s\n' "$arg" >>"$OPS_LOG"; done
printf '%s\n' '@@end-call' >>"$OPS_LOG"
if [[ ${OPS_EXIT:-0} -ne 0 ]]; then exit "${OPS_EXIT:-0}"; fi
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
ROSTER_NULL_NAME='[{"name":null,"base_name":"gina","status":"stopped"},{"name":"midi","base_name":"midi","status":"stopped"}]'
ROSTER_NUMERIC_NAME='[{"name":123,"base_name":"gina","status":"stopped"},{"name":"midi","base_name":"midi","status":"stopped"}]'

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
  unset HCOM_EXIT HCOM_LIST_EXIT HCOM_R_EXIT HCOM_LIST_SLEEP HCOM_R_SLEEP HCOM_SEND_SLEEP OPS_EXIT HCOM_TIMEOUT HCOM_SEND_TIMEOUT
  SCOPE_SHOW=$scope_show
}

# `source` alone must not resolve a lead, read the roster, or send anything.
start_case sourcing-only "$LEAD_OK" "$ROSTER_LIVE"
# shellcheck source=/dev/null
source "$LIB"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "sourcing the library sent a notification"
[[ $(call_count "$OPS_LOG") -eq 0 ]] || fail "sourcing the library resolved a scope"

notify_args() {
  set +e
  system_notify "$@" >"$TMP/stdout" 2>"$TMP/stderr"
  CODE=$?
  set -e
  OUT=$(cat "$TMP/stdout")
  ERR=$(cat "$TMP/stderr")
}

notify() {
  notify_args "$1" "$2" "$3"
}

# Runs the notify call in a fresh shell whose PATH holds only the binaries in
# $NO_TIMEOUT_BIN, which does not include `timeout`, so the seam cannot use a
# timeout binary that the host has. The call itself is the same as notify.
notify_without_timeout() {
  set +e
  env PATH="$NO_TIMEOUT_BIN" bash -c '
    source "$1" || exit 3
    shift
    declare -F system_notify >/dev/null || exit 3
    system_notify "$@"
  ' _ "$LIB" "$@" >"$TMP/stdout" 2>"$TMP/stderr"
  CODE=$?
  set -e
  OUT=$(cat "$TMP/stdout")
  ERR=$(cat "$TMP/stderr")
}

# Runs the notify call in a fresh shell under exactly the locale in $1, so a
# guard that quietly depends on the ambient collation cannot look correct here.
# A fresh shell also leaves the suite's own locale variables untouched.
notify_under_locale() {
  local locale=$1
  shift
  set +e
  env LC_ALL="$locale" bash -c '
    source "$1" || exit 3
    shift
    declare -F system_notify >/dev/null || exit 3
    system_notify "$@"
  ' _ "$LIB" "$@" >"$TMP/stdout" 2>"$TMP/stderr"
  CODE=$?
  set -e
  OUT=$(cat "$TMP/stdout")
  ERR=$(cat "$TMP/stderr")
}

# Argument count: exactly three arguments are required, and an argv refusal
# makes no hcom call at all.
start_case argc-zero "$LEAD_OK" "$ROSTER_LIVE"
notify_args
[[ $CODE -eq 2 ]] || fail "empty argv returned $CODE, want 2"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "empty argv reached hcom"

start_case argc-two "$LEAD_OK" "$ROSTER_LIVE"
notify_args omp-runtime inform
[[ $CODE -eq 2 ]] || fail "two-argument call returned $CODE, want 2"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "two-argument call reached hcom"

start_case argc-four "$LEAD_OK" "$ROSTER_LIVE"
notify_args omp-runtime inform "build finished" extra
[[ $CODE -eq 2 ]] || fail "four-argument call returned $CODE, want 2"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "four-argument call reached hcom"

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

# A failed scope resolution is refused, names the scope root it tried, and
# reaches no hcom call.
start_case scope-show-failure "$LEAD_OK" "$ROSTER_LIVE"
export OPS_EXIT=1
notify omp-runtime inform "build finished"
unset OPS_EXIT
[[ $CODE -eq 2 ]] || fail "failed scope show returned $CODE, want 2"
[[ $ERR == *"$CASE"* ]] || fail "failed scope show did not name the scope root: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "failed scope show reached hcom"

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

# The seam accepts only what HCOM accepts: no dot, and at most 50 characters.
# A refused id never reaches hcom.
start_case source-dot "$LEAD_OK" "$ROSTER_LIVE"
notify omp.runtime inform "generation published"
[[ $CODE -eq 2 ]] || fail "dotted source id returned $CODE, want 2"
[[ $ERR == *"[a-z0-9_:-]"* ]] || fail "dotted source id stderr: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "dotted source id reached hcom"

start_case source-length-max "$LEAD_OK" "$ROSTER_LIVE"
printf -v source_id '%*s' 50 ""
source_id=${source_id// /a}
notify "$source_id" inform "generation published"
[[ $CODE -eq 0 ]] || fail "50-character source id returned $CODE: $ERR"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system $source_id @midi --intent inform -- generation published" ]] \
  || fail "unexpected 50-character argv: $(call_text_with "$HCOM_LOG" send)"

start_case source-length-over "$LEAD_OK" "$ROSTER_LIVE"
printf -v source_id '%*s' 51 ""
source_id=${source_id// /a}
notify "$source_id" inform "generation published"
[[ $CODE -eq 2 ]] || fail "51-character source id returned $CODE, want 2"
[[ $ERR == *"1-50"* ]] || fail "51-character source id stderr does not name the length cap: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "51-character source id reached hcom"

# Every shape the operation-stem convention uses is still accepted, so the
# charset is not narrowed by accident while it is made collation-independent.
accepted_index=0
for accepted_id in omp-runtime systemd-ops:runtime-run a9-b_c:d 9z a a_b overseer2; do
  accepted_index=$((accepted_index + 1))
  start_case "source-accepted-$accepted_index" "$LEAD_OK" "$ROSTER_LIVE"
  notify "$accepted_id" inform "generation published"
  [[ $CODE -eq 0 ]] || fail "source id '$accepted_id' returned $CODE, want 0: $ERR"
  [[ $(call_text_with "$HCOM_LOG" send) == "send --as-system $accepted_id @midi --intent inform -- generation published" ]] \
    || fail "unexpected argv for source id '$accepted_id': $(call_text_with "$HCOM_LOG" send)"
done

# A non-ASCII source id is refused before any hcom call, because HCOM's
# charset is [A-Za-z0-9_-:] and does not accept it. The case runs under three
# collations on purpose: the ambient one, a UTF-8 locale pinned through LC_ALL,
# and C. A [a-z] range is not a fixed charset: under a UTF-8 collation it also
# matches accented lowercase letters, so only a check that ignores the
# collation at all passes all three. The payload is written as byte escapes
# rather than as \u00e9: bash resolves \u through the ambient locale, so an
# exported LC_ALL=C turns that escape into the ASCII text omp\u00E9 and every
# collation below receives a string that is not accented at all.
start_case source-non-ascii "$LEAD_OK" "$ROSTER_LIVE"
non_ascii_source=$'omp\xc3\xa9'
notify "$non_ascii_source" inform "generation published"
[[ $CODE -eq 2 ]] || fail "non-ASCII source id under the ambient locale returned $CODE, want 2: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "non-ASCII source id under the ambient locale reached hcom"

UTF8_LOCALE=
for candidate in en_US.UTF-8 en_US.utf8; do
  if locale -a 2>/dev/null | grep -qxF "$candidate"; then
    UTF8_LOCALE=$candidate
    break
  fi
done

if [[ -n $UTF8_LOCALE ]]; then
  notify_under_locale "$UTF8_LOCALE" "$non_ascii_source" inform "generation published"
  [[ $CODE -eq 2 ]] || fail "non-ASCII source id under LC_ALL=$UTF8_LOCALE returned $CODE, want 2: $ERR"
  [[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "non-ASCII source id under LC_ALL=$UTF8_LOCALE reached hcom"
else
  echo "note: no en_US.UTF-8 locale on this host; the non-ASCII source id ran under the ambient and C collations" >&2
fi

notify_under_locale C "$non_ascii_source" inform "generation published"
[[ $CODE -eq 2 ]] || fail "non-ASCII source id under LC_ALL=C returned $CODE, want 2: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "non-ASCII source id under LC_ALL=C reached hcom"

# The control for that case. A bracket expression is expanded through the
# collation in force, so a library whose charset is a bracket range accepts the
# accented id wherever the collation is UTF-8, and the case above then stops
# refusing it. That regression is visible only when the non-ASCII payload
# reaches the call as its real bytes: with the case written as $'\u00e9', an
# exported LC_ALL=C degraded the escape to the ASCII text omp\u00E9 and the run
# stayed green against a bracket-range library. The scratch copy is patched in
# $TMP and the tracked library is not touched. The nested run is marked so it
# does not recurse into this control.
if [[ -n ${NOTIFY_PROVENANCE_SCRATCH_LIB:-} ]]; then
  : # nested control run: the library under test is already the scratch copy
elif [[ -n $UTF8_LOCALE ]]; then
  SCRATCH_LIB=$TMP/bracket-charset-system-notify
  sed -e "s/local ascii_lower='[a-z]*'/local ascii_lower='a-z'/" \
    -e "s/local ascii_digit='[0-9]*'/local ascii_digit='0-9'/" "$LIB" >"$SCRATCH_LIB"
  grep -q "local ascii_lower='a-z'" "$SCRATCH_LIB" || fail "the scratch library did not take the bracket-range charset"
  grep -q "local ascii_digit='0-9'" "$SCRATCH_LIB" || fail "the scratch library did not take the bracket-range digits"
  set +e
  env LC_ALL=C NOTIFY_LIB="$SCRATCH_LIB" NOTIFY_PROVENANCE_SCRATCH_LIB=1 \
    bash "$SELF" >"$TMP/control-stdout" 2>"$TMP/control-stderr"
  control_code=$?
  set -e
  [[ $control_code -ne 0 ]] \
    || fail "the suite passed against a bracket-range charset library under LC_ALL=C"
  grep -q "non-ASCII source id" "$TMP/control-stderr" \
    || fail "the bracket-range control failed, but not on the non-ASCII source id: $(cat "$TMP/control-stderr")"
  echo "note: the bracket-range control failed under LC_ALL=C on the non-ASCII source id, as required" >&2
else
  echo "note: no UTF-8 locale on this host; the bracket-range control was not run" >&2
fi

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

# The lead handle is checked with the same care as the source id: a handle
# whose fourth character is accented is not an hcom handle and never reaches
# hcom. jq decodes the \u00e9 escape, so the case is written in plain ASCII.
start_case lead-non-ascii '{"ok":true,"data":{"coordination":{"lead":"hcom:mid\u00e9"}}}' "$ROSTER_LIVE"
notify omp-runtime inform "build finished"
[[ $CODE -eq 2 ]] || fail "non-ASCII lead handle under the ambient locale returned $CODE, want 2: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "non-ASCII lead handle under the ambient locale reached hcom"

if [[ -n $UTF8_LOCALE ]]; then
  notify_under_locale "$UTF8_LOCALE" omp-runtime inform "build finished"
  [[ $CODE -eq 2 ]] || fail "non-ASCII lead handle under LC_ALL=$UTF8_LOCALE returned $CODE, want 2: $ERR"
  [[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "non-ASCII lead handle under LC_ALL=$UTF8_LOCALE reached hcom"
fi

notify_under_locale C omp-runtime inform "build finished"
[[ $CODE -eq 2 ]] || fail "non-ASCII lead handle under LC_ALL=C returned $CODE, want 2: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 0 ]] || fail "non-ASCII lead handle under LC_ALL=C reached hcom"

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

# One odd row cannot mask a later matching one: a row whose name is null is
# skipped, not an abort of the whole roster match.
start_case wake-null-row "$LEAD_OK" "$ROSTER_NULL_NAME"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "null roster name returned $CODE: $ERR"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 1 ]] || fail "null roster name masked the wake"
[[ $(call_text "$HCOM_LOG" 2) == "r midi --go" ]] || fail "null roster name wake call: $(call_text "$HCOM_LOG" 2)"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system omp-runtime @midi --intent inform -- build finished" ]] \
  || fail "null roster name delivery argv: $(call_text_with "$HCOM_LOG" send)"

# A name of an unexpected type is skipped the same way: the match must not
# abort on it and lose the later row that does name the lead.
start_case wake-numeric-row-name "$LEAD_OK" "$ROSTER_NUMERIC_NAME"
notify omp-runtime inform "build finished"
[[ $CODE -eq 0 ]] || fail "non-string roster name returned $CODE: $ERR"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 1 ]] || fail "non-string roster name masked the wake"
[[ $(call_text "$HCOM_LOG" 2) == "r midi --go" ]] || fail "non-string roster name wake call: $(call_text "$HCOM_LOG" 2)"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system omp-runtime @midi --intent inform -- build finished" ]] \
  || fail "non-string roster name delivery argv: $(call_text_with "$HCOM_LOG" send)"

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

# A hung hcom cannot withhold the notification: the roster read is bounded by
# HCOM_TIMEOUT, and a timeout counts as a failure like any other. The elapsed
# check is skipped exactly where `timeout` is unavailable, which is where the
# seam deliberately falls back to the unbounded call.
start_case wake-roster-hang "$LEAD_OK" "$ROSTER_STOPPED"
export HCOM_TIMEOUT=1 HCOM_LIST_SLEEP=10
SECONDS=0
notify omp-runtime inform "build finished"
ROSTER_HANG_SECONDS=$SECONDS
unset HCOM_TIMEOUT HCOM_LIST_SLEEP
[[ $CODE -eq 0 ]] || fail "hung roster read returned $CODE: $ERR"
[[ -z $ERR ]] || fail "hung roster read was not silent: $ERR"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 0 ]] || fail "hung roster read still attempted a wake"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "hung roster read blocked delivery"
if command -v timeout >/dev/null 2>&1; then
  [[ $ROSTER_HANG_SECONDS -lt 6 ]] || fail "hung roster read took ${ROSTER_HANG_SECONDS}s, HCOM_TIMEOUT did not bound it"
fi

# A hung resume is bounded the same way, and it cannot block delivery either.
start_case wake-resume-hang "$LEAD_OK" "$ROSTER_STOPPED"
export HCOM_TIMEOUT=1 HCOM_R_SLEEP=10
SECONDS=0
notify omp-runtime inform "build finished"
RESUME_HANG_SECONDS=$SECONDS
unset HCOM_TIMEOUT HCOM_R_SLEEP
[[ $CODE -eq 0 ]] || fail "hung resume returned $CODE: $ERR"
[[ -z $ERR ]] || fail "hung resume was not silent: $ERR"
[[ $(count_calls_with "$HCOM_LOG" r) -eq 1 ]] || fail "hung resume was not attempted"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "hung resume blocked delivery"
if command -v timeout >/dev/null 2>&1; then
  [[ $RESUME_HANG_SECONDS -lt 6 ]] || fail "hung resume took ${RESUME_HANG_SECONDS}s, HCOM_TIMEOUT did not bound it"
fi

# A hung delivery is bounded by HCOM_SEND_TIMEOUT, which is a knob of its own
# with a larger default than HCOM_TIMEOUT because a legitimate send can take
# longer than a roster read. A killed send is a delivery failure, not a silent
# retry: the seam returns the timeout status and stderr names the timeout and
# the recipient, so a caller can tell a timeout from an hcom error. HCOM may
# already have accepted a message whose send is killed, which is what a bound
# costs here.
start_case send-hang "$LEAD_OK" "$ROSTER_LIVE"
export HCOM_SEND_TIMEOUT=1 HCOM_SEND_SLEEP=10
SECONDS=0
notify omp-runtime inform "build finished"
SEND_HANG_SECONDS=$SECONDS
unset HCOM_SEND_TIMEOUT HCOM_SEND_SLEEP
[[ $CODE -eq 124 ]] || fail "hung send returned $CODE, want 124: $ERR"
[[ $ERR == *"@midi"* ]] || fail "hung send did not name the recipient: $ERR"
[[ $ERR == *"timed out"* ]] || fail "hung send did not report a timeout: $ERR"
[[ $ERR == *"1s"* ]] || fail "hung send did not name the timeout: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 2 ]] || fail "hung send made $(call_count "$HCOM_LOG") hcom calls"
[[ $(count_calls_with "$HCOM_LOG" send) -eq 1 ]] || fail "hung send was not attempted exactly once"
if command -v timeout >/dev/null 2>&1; then
  [[ $SEND_HANG_SECONDS -lt 6 ]] || fail "hung send took ${SEND_HANG_SECONDS}s, HCOM_SEND_TIMEOUT did not bound it"
fi

# A send that finishes inside its bound is delivered as before, with the same
# argv, so the bound does not interfere with an ordinary notification.
start_case send-fast "$LEAD_OK" "$ROSTER_LIVE"
export HCOM_SEND_TIMEOUT=1
notify omp-runtime inform "build finished"
unset HCOM_SEND_TIMEOUT
[[ $CODE -eq 0 ]] || fail "fast send under HCOM_SEND_TIMEOUT=1 returned $CODE: $ERR"
[[ -z $ERR ]] || fail "fast send under HCOM_SEND_TIMEOUT=1 wrote to stderr: $ERR"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system omp-runtime @midi --intent inform -- build finished" ]] \
  || fail "fast send argv under HCOM_SEND_TIMEOUT=1: $(call_text_with "$HCOM_LOG" send)"

# The two bounds are independent. A slow roster read is cut short by
# HCOM_TIMEOUT while the send keeps its own, larger budget, and a slow send is
# cut short by HCOM_SEND_TIMEOUT while the roster read keeps HCOM_TIMEOUT.
start_case bounds-roster-slow "$LEAD_OK" "$ROSTER_LIVE"
export HCOM_TIMEOUT=1 HCOM_LIST_SLEEP=10 HCOM_SEND_TIMEOUT=30
SECONDS=0
notify omp-runtime inform "build finished"
BOUNDS_ROSTER_SECONDS=$SECONDS
unset HCOM_TIMEOUT HCOM_LIST_SLEEP HCOM_SEND_TIMEOUT
[[ $CODE -eq 0 ]] || fail "slow roster read under two bounds returned $CODE: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 2 ]] || fail "slow roster read under two bounds made $(call_count "$HCOM_LOG") hcom calls"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system omp-runtime @midi --intent inform -- build finished" ]] \
  || fail "slow roster read under two bounds changed the delivery argv: $(call_text_with "$HCOM_LOG" send)"
if command -v timeout >/dev/null 2>&1; then
  [[ $BOUNDS_ROSTER_SECONDS -lt 6 ]] || fail "slow roster read took ${BOUNDS_ROSTER_SECONDS}s, HCOM_TIMEOUT=1 did not bound it"
fi

start_case bounds-send-slow "$LEAD_OK" "$ROSTER_LIVE"
export HCOM_TIMEOUT=30 HCOM_SEND_TIMEOUT=1 HCOM_SEND_SLEEP=10
SECONDS=0
notify omp-runtime inform "build finished"
BOUNDS_SEND_SECONDS=$SECONDS
unset HCOM_TIMEOUT HCOM_SEND_TIMEOUT HCOM_SEND_SLEEP
[[ $CODE -eq 124 ]] || fail "slow send under a large HCOM_TIMEOUT returned $CODE, want 124: $ERR"
[[ $(call_count "$HCOM_LOG") -eq 2 ]] || fail "slow send under a large HCOM_TIMEOUT made $(call_count "$HCOM_LOG") hcom calls"
[[ $(call_text "$HCOM_LOG" 1) == "list --all --json" ]] || fail "slow send under a large HCOM_TIMEOUT skipped the roster read: $(call_text "$HCOM_LOG" 1)"
if command -v timeout >/dev/null 2>&1; then
  [[ $BOUNDS_SEND_SECONDS -lt 6 ]] || fail "slow send took ${BOUNDS_SEND_SECONDS}s, HCOM_SEND_TIMEOUT=1 did not bound it"
fi

# Where no `timeout` binary is on PATH the seam keeps the unbounded call rather
# than skipping it. The roster read below sleeps past its HCOM_TIMEOUT and is
# still allowed to finish: the call is made, and nothing kills it.
NO_TIMEOUT_BIN=$TMP/no-timeout-bin
mkdir -p "$NO_TIMEOUT_BIN"
for tool in bash env sleep cat jq; do
  tool_path=$(command -v "$tool") || fail "missing $tool for the no-timeout PATH"
  ln -s "$tool_path" "$NO_TIMEOUT_BIN/$tool"
done
[[ ! -e $NO_TIMEOUT_BIN/timeout ]] || fail "the no-timeout PATH still holds a timeout binary"

start_case no-timeout-fallback "$LEAD_OK" "$ROSTER_LIVE"
export HCOM_TIMEOUT=1 HCOM_LIST_SLEEP=2
SECONDS=0
notify_without_timeout omp-runtime inform "build finished"
NO_TIMEOUT_SECONDS=$SECONDS
unset HCOM_TIMEOUT HCOM_LIST_SLEEP
[[ $CODE -eq 0 ]] || fail "the unbounded fallback returned $CODE: $ERR"
[[ -z $ERR ]] || fail "the unbounded fallback wrote to stderr: $ERR"
[[ $NO_TIMEOUT_SECONDS -ge 2 ]] \
  || fail "the roster read stopped after ${NO_TIMEOUT_SECONDS}s with no timeout binary on PATH"
[[ $(call_text "$HCOM_LOG" 1) == "list --all --json" ]] || fail "the unbounded fallback skipped the roster read: $(call_text "$HCOM_LOG" 1)"
[[ $(call_text_with "$HCOM_LOG" send) == "send --as-system omp-runtime @midi --intent inform -- build finished" ]] \
  || fail "the unbounded fallback delivery argv: $(call_text_with "$HCOM_LOG" send)"

echo "notify-provenance ok"
