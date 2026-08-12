#!/usr/bin/env bash
# Prove which backend answered list_units, then hold both backends'
# output shapes against each other.
#
# The fallback is not observable from the output, so a passing test on
# a systemd >= 258 host does not show that varlink answered — the reply
# could have come from the systemctl fallback. Each backend is made the
# only one possible: the varlink socket is renamed to force the CLI,
# then systemctl is removed so only the socket can answer. Needs root
# on the target systemd, reached via $HOST.
set -euo pipefail

MCPD=${MCPD:?set MCPD to the server command}
HOST=${HOST:?set HOST to the command prefix reaching the systemd host}
SOCKET=/run/systemd/io.systemd.Manager

fail() { echo "FAIL: $*" >&2; exit 1; }

call_tool() { # call_tool <scopes> <tool> <args-json> -> inner result text
  printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' "$2" "$3" |
    $MCPD --grant "$1" | jq -r '.result.content[0].text'
}

list_units() { call_tool units:read list_units '{}'; }

# A long-lived canary both runs must see.
$HOST systemd-run --unit=mcpd-diff-canary.service /bin/sleep 300 >/dev/null 2>&1

# 1. Force the CLI backend: hide the varlink socket.
$HOST mv "$SOCKET" "$SOCKET.hidden"
list_units >/tmp/mcpd-cli.json
$HOST mv "$SOCKET.hidden" "$SOCKET"

# 1b. A live varlink failure that isn't a missing socket: a plain file
#     at the socket path fails connect() with ENOTSOCK for every caller
#     (chmod tricks wouldn't work — root bypasses file permissions).
#     The fallback must still answer.
$HOST mv "$SOCKET" "$SOCKET.hidden"
$HOST touch "$SOCKET"
list_units | jq -e 'type == "array" and length > 0' >/dev/null ||
  fail "list_units did not fall back on a dead varlink socket path"
$HOST rm "$SOCKET"
$HOST mv "$SOCKET.hidden" "$SOCKET"

# 2. Force the varlink backend: remove systemctl entirely. If
#    list_units still answers, only the socket could have answered.
$HOST mv /usr/bin/systemctl /usr/bin/systemctl.hidden
list_units >/tmp/mcpd-varlink.json
# boot_times must also answer — its own varlink path (Manager.Describe)
# is the only possible source without systemctl...
call_tool boot:read boot_times '{}' |
  jq -e '.total_usec > 0' >/dev/null ||
  fail "boot_times did not answer over varlink Describe"
# ...and a systemctl-backed tool must now fail, proving the server
# cannot shell out behind our back.
printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"unit_properties","arguments":{"unit":"systemd-journald.service"}}}\n' |
  $MCPD --grant units:read | jq -e '.result.isError' >/dev/null ||
  fail "unit_properties succeeded with systemctl removed"
$HOST mv /usr/bin/systemctl.hidden /usr/bin/systemctl

for f in /tmp/mcpd-cli.json /tmp/mcpd-varlink.json; do
  jq -e 'map(.unit) | index("mcpd-diff-canary.service")' "$f" >/dev/null ||
    fail "canary unit missing from $f"
done

# Same shape from either backend — the module's stated contract.
diff <(jq -S 'map(keys) | unique' /tmp/mcpd-cli.json) \
  <(jq -S 'map(keys) | unique' /tmp/mcpd-varlink.json) ||
  fail "backends disagree on row shape"

$HOST systemctl stop mcpd-diff-canary.service >/dev/null 2>&1 || true

echo "PASS: varlink proof and differential"
