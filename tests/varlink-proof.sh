#!/usr/bin/env bash
# Prove which backend answered list_units, then hold both backends'
# output shapes against each other.
#
# The fallback is deliberately invisible, so a passing test on a
# systemd >= 258 host doesn't show varlink worked — it might have
# silently fallen back to systemctl. So we make each backend the only
# one possible: rename the varlink socket to force the CLI, then remove
# systemctl so only the socket could answer. Needs root on the target
# systemd, reached via $HOST.
set -euo pipefail

MCPD=${MCPD:?set MCPD to the server command}
HOST=${HOST:?set HOST to the command prefix reaching the systemd host}
SOCKET=/run/systemd/io.systemd.Manager

fail() { echo "FAIL: $*" >&2; exit 1; }

list_units() {
  printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_units","arguments":{}}}\n' |
    $MCPD --grant units:read | jq -r '.result.content[0].text'
}

# A long-lived canary both runs must see.
$HOST systemd-run --unit=mcpd-diff-canary.service /bin/sleep 300 >/dev/null 2>&1

# 1. Force the CLI backend: hide the varlink socket.
$HOST mv "$SOCKET" "$SOCKET.hidden"
list_units >/tmp/mcpd-cli.json
$HOST mv "$SOCKET.hidden" "$SOCKET"

# 2. Force the varlink backend: remove systemctl entirely. If
#    list_units still answers, only the socket could have answered.
$HOST mv /usr/bin/systemctl /usr/bin/systemctl.hidden
list_units >/tmp/mcpd-varlink.json
# ...and the systemctl-backed tool must now fail, proving the server
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
