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
  # $MCPD is deliberately unquoted: it may carry a command prefix.
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
[ "$names" = "list_units,failed_units,unit_properties" ] ||
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

echo "== boot_times / critical_chain"
# Some hosts never finish startup — GitHub's runner VMs keep a unit
# activating forever — and systemd is honest about it, so we are too.
# Test whichever truth the host tells: real timings where bootup
# finished, the clean error where it didn't.
if $HOST systemd-analyze time >/dev/null 2>&1; then
  tool boot:read boot_times '{}' |
    jq -e '.total_usec > 0 and .userspace_usec > 0' >/dev/null ||
    fail "boot_times implausible"
  tool boot:read critical_chain '{}' |
    jq -e '.chain | length > 0 and all(.[]; has("unit") and has("depth"))' >/dev/null ||
    fail "critical_chain empty or malformed"
else
  echo "   (bootup unfinished on this host - asserting the clean-error path)"
  expect_error boot:read boot_times '{}' "not yet finished"
  expect_error boot:read critical_chain '{}' "not yet finished"
fi

echo "PASS: all live integration tests"
