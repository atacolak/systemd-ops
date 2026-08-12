#!/usr/bin/env bash
# Live integration test: drives systemd-mcpd over stdio against a real,
# running systemd and asserts on the JSON replies with jq.
#
# Environment:
#   MCPD  command that runs the server binary
#         (default: target/release/systemd-mcpd)
#   HOST  command prefix that executes commands on the same systemd the
#         server talks to; empty means "right here"
#         (e.g. "docker exec -i sysd" when the server runs in a
#         container). It must forward stdin: some commands below pipe
#         content in, and a prefix that drops stdin writes empty files.
#
# Needs jq, and enough privilege to read the system journal and create
# transient units (root, or a root docker exec into a container).
set -euo pipefail

MCPD=${MCPD:-target/release/systemd-mcpd}
HOST=${HOST:-}

fail() { echo "FAIL: $*" >&2; exit 1; }

rpc() { # rpc <scopes> <request-lines...> -> raw replies, one per line
  local scopes=$1
  shift
  # $MCPD is left unquoted: it may carry a command prefix.
  printf '%s\n' "$@" | $MCPD --grant "$scopes"
}

call() { # call <tool> <args-json> -> one tools/call request line
  printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"%s","arguments":%s}}' "$1" "$2"
}

tool() { # tool <scopes> <tool> <args-json> -> inner result JSON, or fail
  local reply
  reply=$(rpc "$1" "$(call "$2" "$3")")
  [ "$(jq -r '.result.isError' <<<"$reply")" = false ] ||
    fail "$2 errored: $(jq -r '.result.content[0].text' <<<"$reply")"
  jq -r '.result.content[0].text' <<<"$reply"
}

expect_error() { # expect_error <scopes> <tool> <args-json> <substring>
  local reply text
  reply=$(rpc "$1" "$(call "$2" "$3")")
  text=$(jq -r '.error.message // .result.content[0].text' <<<"$reply")
  grep -qF "$4" <<<"$text" || fail "$2: expected '$4' in: $text"
}

echo "== scope gating"
names=$(rpc units:read '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' |
  jq -r '.result.tools | map(.name) | join(",")')
[ "$names" = "list_units,failed_units,unit_properties,list_timers,list_sockets,list_unit_files,unit_dependencies,unit_security,unit_log_control" ] ||
  fail "tools/list under units:read advertised: $names"
expect_error units:read unit_logs '{"unit":"ssh.service"}' "journal:read"
expect_error units:read boot_times '{}' "boot:read"
expect_error units:read unit_properties '{"unit":"x;bad"}' "not a valid unit name"

echo "== protocol eras (2026-07-28 and initialize-based)"
# A modern request declares its version and the client's capabilities in
# _meta; there is no handshake. The reply must carry the era's fields.
META='"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}'
discover=$(rpc units:read "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server/discover\",\"params\":{$META}}")
jq -e '.result.resultType == "complete"
       and (.result.supportedVersions | index("2026-07-28"))
       and (.result.ttlMs >= 0) and (.result.cacheScope == "public")
       and (.result._meta["io.modelcontextprotocol/serverInfo"].name == "systemd-mcpd")' \
  <<<"$discover" >/dev/null || fail "server/discover malformed: $discover"
# A probe that has not picked a version yet is still answered.
rpc units:read '{"jsonrpc":"2.0","id":1,"method":"server/discover"}' |
  jq -e '.result.supportedVersions | index("2026-07-28")' >/dev/null ||
  fail "server/discover refused a probe that declared no version"
# Tool results carry the discriminator and the reply as real JSON,
# identical to the text block clients have always received.
modern=$(rpc units:read \
  "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"list_units\",\"arguments\":{\"pattern\":\"systemd-journald.service\"},$META}}")
jq -e '.result.resultType == "complete" and .result.isError == false
       and (.result.structuredContent | length == 1)
       and (.result.structuredContent == (.result.content[0].text | fromjson))' \
  <<<"$modern" >/dev/null || fail "modern tools/call malformed: $modern"
# An unknown version is refused with the list of what is supported...
rpc units:read '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1900-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}' |
  jq -e '.error.code == -32022 and (.error.data.supported | index("2026-07-28"))' >/dev/null ||
  fail "unsupported version was not refused with -32022 and a supported list"
# ...a missing required _meta field is invalid params...
rpc units:read '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}' |
  jq -e '.error.code == -32602' >/dev/null ||
  fail "missing clientCapabilities was not rejected"
# ...and the handshake era still works, unchanged.
rpc units:read '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}' |
  jq -e '.result.protocolVersion == "2025-11-25" and .result.serverInfo.name == "systemd-mcpd"
         and (.result | has("resultType") | not)' >/dev/null ||
  fail "legacy initialize reply wrong"
rpc units:read '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' |
  jq -e '(.result | has("resultType") | not) and (.result | has("ttlMs") | not)' >/dev/null ||
  fail "legacy tools/list leaked modern era fields"

echo "== list_units / failed_units"
units=$(tool units:read list_units '{}')
jq -e 'type == "array" and length > 0' <<<"$units" >/dev/null ||
  fail "list_units empty or not an array"
jq -e 'all(.[]; has("unit") and has("load") and has("active") and has("sub") and has("description"))' \
  <<<"$units" >/dev/null || fail "list_units rows missing keys"
jq -e 'map(.unit) | index("systemd-journald.service")' <<<"$units" >/dev/null ||
  fail "systemd-journald.service not in list_units"
tool units:read failed_units '{}' | jq -e 'type == "array"' >/dev/null ||
  fail "failed_units not an array"
# The name glob narrows the reply and keeps what it should. It runs
# after whichever backend answered, so this covers both.
matched=$(tool units:read list_units '{"pattern":"systemd-*.service"}')
jq -e --argjson all "$(jq length <<<"$units")" \
  'length > 0 and length < $all and all(.[]; (.unit | startswith("systemd-")) and (.unit | endswith(".service")))' \
  <<<"$matched" >/dev/null || fail "list_units pattern filter wrong: $(jq -c 'map(.unit)' <<<"$matched")"
jq -e 'map(.unit) | index("systemd-journald.service")' <<<"$matched" >/dev/null ||
  fail "journald lost by the systemd-*.service pattern"
# Both filters at once, and a pattern matching nothing is an empty
# answer rather than an error.
tool units:read list_units '{"pattern":"*.socket","state":"active"}' |
  jq -e 'all(.[]; (.unit | endswith(".socket")) and .active == "active")' >/dev/null ||
  fail "list_units state+pattern filters leaked rows"
tool units:read list_units '{"pattern":"zzz-no-such-unit-zzz*"}' |
  jq -e 'length == 0' >/dev/null || fail "no-match pattern did not return an empty array"
expect_error units:read list_units '{"pattern":""}' "pattern must be"

echo "== unit_properties"
tool units:read unit_properties '{"unit":"systemd-journald.service"}' |
  jq -e '.properties.ExecStart | length > 0' >/dev/null ||
  fail "journald properties have no ExecStart"
# A selection returns those properties and nothing else; the full set
# runs to some 200 keys.
tool units:read unit_properties \
  '{"unit":"systemd-journald.service","properties":["ActiveState","FragmentPath"]}' |
  jq -e '.properties | keys == ["ActiveState","FragmentPath"]' >/dev/null ||
  fail "unit_properties selection returned other keys"
expect_error units:read unit_properties \
  '{"unit":"systemd-journald.service","properties":["NoSuchProperty"]}' \
  "none of the requested properties"

echo "== list_timers / list_sockets / list_unit_files"
timers=$(tool units:read list_timers '{}')
jq -e 'type == "array"' <<<"$timers" >/dev/null || fail "list_timers not an array"
# Timestamps are RFC 3339, not raw microseconds, and the fields
# systemctl fills in inconsistently (left, passed) are gone.
jq -e 'all(.[]; has("unit") and has("activates") and has("next") and has("last")
             and (has("left") | not) and (has("passed") | not)
             and (.next == null or (.next | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T.*Z$"))))' \
  <<<"$timers" >/dev/null || fail "list_timers rows malformed: $(jq -c '.[0]' <<<"$timers")"
tool units:read list_timers '{"pattern":"zzz-no-such-timer-zzz*"}' |
  jq -e 'length == 0' >/dev/null || fail "list_timers pattern not applied"
tool units:read list_sockets '{}' |
  jq -e 'type == "array" and length > 0' >/dev/null ||
  fail "list_sockets empty (journald sockets should always exist)"
tool units:read list_sockets '{"pattern":"*journald*"}' |
  jq -e 'length > 0 and all(.[]; .unit | contains("journald"))' >/dev/null ||
  fail "list_sockets pattern filter wrong"
files=$(tool units:read list_unit_files '{}')
jq -e 'map(.unit_file) | index("systemd-journald.service")' <<<"$files" >/dev/null ||
  fail "journald missing from list_unit_files"
tool units:read list_unit_files '{"state":"static"}' |
  jq -e 'length > 0 and all(.[]; .state == "static")' >/dev/null ||
  fail "list_unit_files state filter leaked non-static rows"
# The glob matches the unit file name, which is the key this reply uses.
tool units:read list_unit_files '{"pattern":"systemd-journald.*"}' |
  jq -e 'length > 0 and all(.[]; .unit_file | startswith("systemd-journald."))' >/dev/null ||
  fail "list_unit_files pattern filter wrong"

echo "== unit_dependencies / unit_security / unit_log_control"
tool units:read unit_dependencies '{"unit":"multi-user.target"}' |
  jq -e '.dependencies.After | length > 0' >/dev/null ||
  fail "multi-user.target has no After edges"
tool units:read unit_security '{"unit":"systemd-journald.service"}' |
  jq -e '.analysis | type == "array" and length > 0' >/dev/null ||
  fail "unit_security produced no analysis rows"
# unit_log_control needs a running service that declares BusName= and
# implements LogControl1 over D-Bus (journald serves its log control
# over varlink, which systemctl does not use here). Which such service
# runs differs per host; probe for one.
lc_unit=
for u in systemd-logind.service systemd-resolved.service systemd-networkd.service systemd-timesyncd.service; do
  if $HOST systemctl service-log-level "$u" >/dev/null 2>&1; then
    lc_unit=$u
    break
  fi
done
if [ -n "$lc_unit" ]; then
  tool units:read unit_log_control "{\"unit\":\"$lc_unit\"}" |
    jq -e '(.log_level | length > 0) and (.log_target | length > 0)' >/dev/null ||
    fail "unit_log_control returned empty values for $lc_unit"
else
  echo "   (no LogControl1 service on this host - asserting the clean-error path)"
  expect_error units:read unit_log_control '{"unit":"systemd-journald.service"}' "BusName"
fi

echo "== unit_logs (canary through a transient unit)"
# Unit name and marker are unique per run. A fixed marker lets a journal
# entry from an earlier run satisfy the unfiltered assertion below while
# failing the time-windowed one, which reads as a filter bug that isn't.
CANARY_ID="$$-${RANDOM}"
CANARY_UNIT="mcpd-canary-${CANARY_ID}.service"
CANARY_MARKER="mcpd-canary-marker-${CANARY_ID}"
WRITE_UNIT=/etc/systemd/system/mcpd-write-test.service

# Drop the units and the unit file however this script exits; the
# assertions below abort on failure, and a leaked unit file would make
# the next run start from a different state.
cleanup() {
  $HOST systemctl stop "$CANARY_UNIT" >/dev/null 2>&1 || true
  $HOST systemctl stop mcpd-write-test.service >/dev/null 2>&1 || true
  $HOST systemctl unmask mcpd-write-test.service >/dev/null 2>&1 || true
  $HOST systemctl disable mcpd-write-test.service >/dev/null 2>&1 || true
  $HOST rm -f "$WRITE_UNIT"
  $HOST systemctl daemon-reload >/dev/null 2>&1 || true
}
trap cleanup EXIT
# stderr is captured into a variable, not a file: with HOST as an ssh
# prefix the redirect would happen on this machine rather than in the
# guest, and a fixed path collides across users and runs.
# /bin/echo with the marker as an argument, not /bin/sh -c "echo ...":
# ssh rebuilds the remote command through a shell, which would drop the
# quoting and echo nothing.
if ! canary_err=$($HOST systemd-run --unit="$CANARY_UNIT" --collect \
    /bin/echo "$CANARY_MARKER" 2>&1 >/dev/null); then
  fail "could not start canary unit: $canary_err"
fi
found=
for _ in $(seq 15); do
  if tool journal:read unit_logs "{\"unit\":\"$CANARY_UNIT\"}" |
    jq -e --arg m "$CANARY_MARKER" '.entries | map(.message) | index($m)' >/dev/null; then
    found=1
    break
  fi
  sleep 1
done
[ -n "$found" ] || fail "canary log line never appeared in unit_logs"
# The lines cap holds...
tool journal:read unit_logs "{\"unit\":\"$CANARY_UNIT\",\"lines\":3}" |
  jq -e '.entries | length <= 3' >/dev/null || fail "lines cap not applied"
# ...the query filters reach journalctl: a recent time window, the
# current boot, and a message pattern must all still find the canary,
# and a future window must not...
tool journal:read unit_logs \
  "{\"unit\":\"$CANARY_UNIT\",\"since\":\"-2min\",\"boot\":0,\"grep\":\"$CANARY_MARKER\"}" |
  jq -e --arg m "$CANARY_MARKER" '.entries | map(.message) | index($m)' >/dev/null ||
  fail "since/boot/grep filters lost the canary"
tool journal:read unit_logs "{\"unit\":\"$CANARY_UNIT\",\"since\":\"+1min\"}" |
  jq -e '.entries | length == 0' >/dev/null || fail "future since window returned entries"
# A filter that matches nothing is an empty result, not an error:
# journalctl exits 1 for "no entries matched", which must not surface
# as a tool failure. `tool` fails the run if isError is true.
tool journal:read unit_logs "{\"unit\":\"$CANARY_UNIT\",\"grep\":\"zzz-no-such-string-zzz\"}" |
  jq -e '.entries | length == 0' >/dev/null || fail "no-match grep was not an empty result"
# ...while a failure that writes to stderr still reports isError.
expect_error journal:read unit_logs "{\"unit\":\"$CANARY_UNIT\",\"boot\":999}" "journalctl"
tool journal:read list_boots '{}' |
  jq -e 'type == "array" and length >= 1' >/dev/null || fail "list_boots empty"
# ...and the priority filter reaches journalctl: the canary logged at
# info, so emerg-only must be empty and debug-and-above must include it.
tool journal:read unit_logs "{\"unit\":\"$CANARY_UNIT\",\"priority\":0}" |
  jq -e '.entries | length == 0' >/dev/null || fail "priority=0 returned entries"
tool journal:read unit_logs "{\"unit\":\"$CANARY_UNIT\",\"priority\":7}" |
  jq -e --arg m "$CANARY_MARKER" '.entries | map(.message) | index($m)' >/dev/null ||
  fail "priority=7 lost the canary"

echo "== boot_times / critical_chain / boot_blame"
# Some hosts never finish startup (GitHub's runner VMs keep a unit
# activating indefinitely), and systemd reports that as an error.
# Assert whichever behavior the host exhibits: real timings where
# bootup finished, the error where it did not.
if $HOST systemd-analyze time >/dev/null 2>&1; then
  tool boot:read boot_times '{}' |
    jq -e '.total_usec > 0 and .userspace_usec > 0' >/dev/null ||
    fail "boot_times implausible"
  tool boot:read critical_chain '{}' |
    jq -e '.chain | length > 0 and all(.[]; has("unit") and has("depth"))' >/dev/null ||
    fail "critical_chain empty or malformed"
  tool boot:read critical_chain '{"unit":"systemd-journald.service"}' |
    jq -e '.chain | length > 0' >/dev/null ||
    fail "critical_chain with explicit unit came back empty"
  tool boot:read boot_blame '{}' |
    jq -e '.blame | length > 0 and all(.[]; has("unit") and has("time"))' >/dev/null ||
    fail "boot_blame empty or malformed"
  # A truncated answer says so: the limit caps the list, the total
  # reports how many units were measured.
  tool boot:read boot_blame '{"limit":1}' |
    jq -e '.returned == 1 and (.blame | length) == 1 and .total >= 1' >/dev/null ||
    fail "boot_blame limit not applied or total missing"
else
  echo "   (bootup unfinished on this host - asserting the clean-error path)"
  expect_error boot:read boot_times '{}' "not yet finished"
  expect_error boot:read critical_chain '{}' "not yet finished"
  expect_error boot:read critical_chain '{"unit":"systemd-journald.service"}' "not yet finished"
  # blame, unlike time and critical-chain, can answer on an unfinished
  # boot (it lists whatever already started). Both behaviors were
  # observed on real runners; accept either.
  reply=$(rpc boot:read "$(call boot_blame '{}')")
  if [ "$(jq -r '.result.isError' <<<"$reply")" = false ]; then
    jq -r '.result.content[0].text' <<<"$reply" |
      jq -e '.blame | length > 0 and all(.[]; has("unit") and has("time"))' >/dev/null ||
      fail "boot_blame table malformed on unfinished-boot host"
  else
    jq -r '.result.content[0].text' <<<"$reply" | grep -qF "not yet finished" ||
      fail "boot_blame errored with something other than not-yet-finished"
  fi
fi

echo "== write path (plan/apply)"
wnames=$(rpc units:write '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' |
  jq -r '.result.tools | map(.name) | join(",")')
[ "$wnames" = "plan_change,apply_plan" ] ||
  fail "tools/list under units:write advertised: $wnames"
expect_error units:read plan_change '{"action":"start","unit":"x.service"}' "units:write"
expect_error units:write apply_plan '{"plan":424242}' "unknown plan"

# A disposable unit to operate on, with an [Install] section so the
# enablement actions have something to work with.
# Content goes over stdin: a redirect inside the command would be
# performed by whichever shell ssh hands the string to, not in the guest.
printf '[Unit]\nDescription=systemd-mcpd write-path test unit\n[Service]\nExecStart=/bin/sleep 300\n[Install]\nWantedBy=multi-user.target\n' |
  $HOST tee "$WRITE_UNIT" >/dev/null
# systemd reads a zero-length unit file as masked, which surfaces much
# later as a confusing "is masked" failure. Catch it here instead.
$HOST test -s "$WRITE_UNIT" ||
  fail "unit file is empty: does HOST forward stdin? (docker exec needs -i)"
$HOST systemctl daemon-reload

# Plan and apply in one session (plans are per-session; the first id is 1).
replies=$(rpc units:write \
  "$(call plan_change '{"action":"start","unit":"mcpd-write-test.service"}')" \
  "$(call apply_plan '{"plan":1}')")
planned=$(sed -n 1p <<<"$replies" | jq -r '.result.content[0].text')
applied=$(sed -n 2p <<<"$replies" | jq -r '.result.content[0].text')
jq -e '.plan == 1 and .current.active == "inactive" and .rollback.action == "stop"' \
  <<<"$planned" >/dev/null || fail "plan_change reply malformed: $planned"
jq -e '.applied == true and .diff.active.before == "inactive" and .diff.active.after == "active"' \
  <<<"$applied" >/dev/null || fail "apply_plan diff malformed: $applied"
# The world must have actually changed, per an independent witness.
[ "$($HOST systemctl is-active mcpd-write-test.service)" = "active" ] ||
  fail "unit not active after apply"

# Stale plans are refused: plan against a state, change that state
# out-of-band, apply must refuse. Needs one long-lived session.
coproc SRV { $MCPD --grant units:write; }
printf '%s\n' "$(call plan_change '{"action":"stop","unit":"mcpd-write-test.service"}')" >&"${SRV[1]}"
read -t 30 -r planned2 <&"${SRV[0]}" || fail "no reply from server (plan)"
plan_id=$(jq -r '.result.content[0].text | fromjson | .plan' <<<"$planned2")
$HOST systemctl stop mcpd-write-test.service || fail "out-of-band stop failed"
printf '%s\n' "$(call apply_plan "{\"plan\":$plan_id}")" >&"${SRV[1]}"
read -t 30 -r stale <&"${SRV[0]}" || fail "no reply from server (apply)"
jq -r '.result.content[0].text' <<<"$stale" | grep -qF "stale" ||
  fail "stale plan was not refused: $stale"
kill "$SRV_PID" 2>/dev/null || true

echo "== write path (enablement)"
replies=$(rpc units:write \
  "$(call plan_change '{"action":"enable","unit":"mcpd-write-test.service"}')" \
  "$(call apply_plan '{"plan":1}')")
en_planned=$(sed -n 1p <<<"$replies" | jq -r '.result.content[0].text')
en_applied=$(sed -n 2p <<<"$replies" | jq -r '.result.content[0].text')
jq -e '.current.unit_file_state == "disabled" and .predicted.unit_file_state == "enabled" and .rollback.action == "disable"' \
  <<<"$en_planned" >/dev/null || fail "enable plan malformed: $en_planned"
jq -e '.diff.unit_file_state.before == "disabled" and .diff.unit_file_state.after == "enabled" and (.changes | length > 0)' \
  <<<"$en_applied" >/dev/null || fail "enable apply diff malformed: $en_applied"
[ "$($HOST systemctl is-enabled mcpd-write-test.service)" = "enabled" ] ||
  fail "unit not enabled after apply"
# ...and back, exercising the reported rollback action.
rpc units:write \
  "$(call plan_change '{"action":"disable","unit":"mcpd-write-test.service"}')" \
  "$(call apply_plan '{"plan":1}')" >/dev/null
[ "$($HOST systemctl is-enabled mcpd-write-test.service)" = "disabled" ] ||
  fail "unit not disabled after rollback apply"
# Stale enablement plan: plan mask (recording unit_file_state
# "disabled"), enable the unit out-of-band, apply must refuse. The
# out-of-band change is enable rather than mask because mask fails for
# units whose fragment lives in /etc/systemd/system, because masking wants to
# place its /dev/null symlink at that exact path.
coproc SRV2 { $MCPD --grant units:write; }
printf '%s\n' "$(call plan_change '{"action":"mask","unit":"mcpd-write-test.service"}')" >&"${SRV2[1]}"
read -t 30 -r m_planned <&"${SRV2[0]}" || fail "no reply from server (mask plan)"
m_id=$(jq -r '.result.content[0].text | fromjson | .plan' <<<"$m_planned")
$HOST systemctl enable mcpd-write-test.service >/dev/null 2>&1 ||
  fail "out-of-band enable failed"
printf '%s\n' "$(call apply_plan "{\"plan\":$m_id}")" >&"${SRV2[1]}"
read -t 30 -r m_stale <&"${SRV2[0]}" || fail "no reply from server (mask apply)"
jq -r '.result.content[0].text' <<<"$m_stale" | grep -qF "stale" ||
  fail "stale enablement plan was not refused: $m_stale"
kill "$SRV2_PID" 2>/dev/null || true
$HOST systemctl disable mcpd-write-test.service >/dev/null 2>&1 ||
  fail "cleanup disable failed"

$HOST rm -f "$WRITE_UNIT"
$HOST systemctl daemon-reload

echo "== write path (log tuning)"
# Bad values are protocol errors before anything runs.
expect_error units:write plan_change '{"action":"log-level","unit":"systemd-journald.service","value":"chatty"}' "unknown log level"
expect_error units:write plan_change '{"action":"log-level","unit":"systemd-journald.service"}' "requires a value"
expect_error units:write plan_change '{"action":"start","unit":"x.service","value":"debug"}' "only accepted for"
if [ -n "$lc_unit" ]; then
  # Tune the probed service to debug, then restore it with the
  # rollback value the plan reported.
  replies=$(rpc units:write \
    "$(call plan_change "{\"action\":\"log-level\",\"unit\":\"$lc_unit\",\"value\":\"debug\"}")" \
    "$(call apply_plan '{"plan":1}')")
  ll_planned=$(sed -n 1p <<<"$replies" | jq -r '.result.content[0].text')
  ll_applied=$(sed -n 2p <<<"$replies" | jq -r '.result.content[0].text')
  prev_level=$(jq -r '.current.log_level' <<<"$ll_planned")
  [ -n "$prev_level" ] || fail "log-level plan recorded no current level"
  jq -e '.predicted.log_level == "debug" and .rollback.action == "log-level" and .rollback.value != null' \
    <<<"$ll_planned" >/dev/null || fail "log-level plan malformed: $ll_planned"
  jq -e '.diff.log_level.after == "debug"' <<<"$ll_applied" >/dev/null ||
    fail "log-level apply diff malformed: $ll_applied"
  [ "$($HOST systemctl service-log-level "$lc_unit")" = "debug" ] ||
    fail "$lc_unit log level not debug after apply"
  rpc units:write \
    "$(call plan_change "{\"action\":\"log-level\",\"unit\":\"$lc_unit\",\"value\":\"$prev_level\"}")" \
    "$(call apply_plan '{"plan":1}')" >/dev/null
  [ "$($HOST systemctl service-log-level "$lc_unit")" = "$prev_level" ] ||
    fail "$lc_unit log level not restored to $prev_level"
else
  # No LogControl1 service: the plan itself must surface systemd's
  # BusName error as a tool error.
  expect_error units:write plan_change '{"action":"log-level","unit":"systemd-journald.service","value":"debug"}' "BusName"
fi

echo "PASS: all live integration tests"
