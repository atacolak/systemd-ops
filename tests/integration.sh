#!/usr/bin/env bash
# Live integration test: drives systemd-mcpd over stdio against a real,
# running systemd and asserts on the JSON replies with jq.
#
# Environment:
#   MCPD  command that runs the server binary
#         (default: target/release/systemd-mcpd)
#   HOST  command prefix that executes commands on the same systemd the
#         server talks to; empty means "right here"
#         (e.g. "docker exec sysd" when the server runs in a container)
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
[ "$names" = "list_units,failed_units,unit_properties,list_timers,list_sockets,list_unit_files,unit_dependencies,unit_security" ] ||
  fail "tools/list under units:read advertised: $names"
expect_error units:read unit_logs '{"unit":"ssh.service"}' "journal:read"
expect_error units:read boot_times '{}' "boot:read"
expect_error units:read unit_properties '{"unit":"x;bad"}' "not a valid unit name"

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

echo "== unit_properties"
tool units:read unit_properties '{"unit":"systemd-journald.service"}' |
  jq -e '.properties.ExecStart | length > 0' >/dev/null ||
  fail "journald properties have no ExecStart"

echo "== list_timers / list_sockets / list_unit_files"
tool units:read list_timers '{}' | jq -e 'type == "array"' >/dev/null ||
  fail "list_timers not an array"
tool units:read list_sockets '{}' |
  jq -e 'type == "array" and length > 0' >/dev/null ||
  fail "list_sockets empty (journald sockets should always exist)"
files=$(tool units:read list_unit_files '{}')
jq -e 'map(.unit_file) | index("systemd-journald.service")' <<<"$files" >/dev/null ||
  fail "journald missing from list_unit_files"
tool units:read list_unit_files '{"state":"static"}' |
  jq -e 'length > 0 and all(.[]; .state == "static")' >/dev/null ||
  fail "list_unit_files state filter leaked non-static rows"

echo "== unit_dependencies / unit_security"
tool units:read unit_dependencies '{"unit":"multi-user.target"}' |
  jq -e '.dependencies.After | length > 0' >/dev/null ||
  fail "multi-user.target has no After edges"
tool units:read unit_security '{"unit":"systemd-journald.service"}' |
  jq -e '.analysis | type == "array" and length > 0' >/dev/null ||
  fail "unit_security produced no analysis rows"

echo "== unit_logs (canary through a transient unit)"
$HOST systemd-run --unit=mcpd-canary.service --collect \
  /bin/sh -c 'echo mcpd-canary-marker' >/dev/null 2>&1
found=
for _ in $(seq 15); do
  if tool journal:read unit_logs '{"unit":"mcpd-canary.service"}' |
    jq -e '.entries | map(.message) | index("mcpd-canary-marker")' >/dev/null; then
    found=1
    break
  fi
  sleep 1
done
[ -n "$found" ] || fail "canary log line never appeared in unit_logs"
# The lines cap holds...
tool journal:read unit_logs '{"unit":"mcpd-canary.service","lines":3}' |
  jq -e '.entries | length <= 3' >/dev/null || fail "lines cap not applied"
# ...and the priority filter reaches journalctl: the canary logged at
# info, so emerg-only must be empty and debug-and-above must include it.
tool journal:read unit_logs '{"unit":"mcpd-canary.service","priority":0}' |
  jq -e '.entries | length == 0' >/dev/null || fail "priority=0 returned entries"
tool journal:read unit_logs '{"unit":"mcpd-canary.service","priority":7}' |
  jq -e '.entries | map(.message) | index("mcpd-canary-marker")' >/dev/null ||
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
$HOST bash -c 'printf "[Unit]\nDescription=systemd-mcpd write-path test unit\n[Service]\nExecStart=/bin/sleep 300\n[Install]\nWantedBy=multi-user.target\n" > /etc/systemd/system/mcpd-write-test.service && systemctl daemon-reload'

# Plan and apply in one session (plans are per-session; the first id is 1).
replies=$(rpc units:write \
  "$(call plan_change '{"action":"start","unit":"mcpd-write-test.service"}')" \
  "$(call apply_plan '{"plan":1}')")
planned=$(sed -n 1p <<<"$replies" | jq -r '.result.content[0].text')
applied=$(sed -n 2p <<<"$replies" | jq -r '.result.content[0].text')
jq -e '.plan == 1 and .current.active == "inactive" and .rollback == "stop"' \
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
jq -e '.current.unit_file_state == "disabled" and .predicted.unit_file_state == "enabled" and .rollback == "disable"' \
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
# units whose fragment lives in /etc/systemd/system — masking wants to
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

$HOST bash -c 'rm -f /etc/systemd/system/mcpd-write-test.service && systemctl daemon-reload'

echo "PASS: all live integration tests"
