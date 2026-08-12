#!/usr/bin/env bash
# Prove which backend answered list_units, then hold both backends'
# output shapes against each other.
#
# The fallback is not observable from the output, so a passing test on
# a systemd >= 258 host does not show that varlink answered: the reply
# could have come from the systemctl fallback. Each backend is made the
# only one possible:
#
#   CLI only      the varlink socket is renamed, so connect() fails.
#                 Needs root on the target systemd; /run is a tmpfs, so
#                 an interrupted run costs the socket until reboot.
#   varlink only  the server runs with an empty PATH, so it cannot
#                 execute systemctl. Nothing on disk is touched, which
#                 is why this is not done by moving /usr/bin/systemctl:
#                 an interrupted run must not leave a host without it.
set -euo pipefail

MCPD=${MCPD:?set MCPD to the server command}
# Set but empty means "run commands here"; the colonless form accepts
# that and still catches a caller who forgot the variable entirely.
HOST=${HOST?set HOST to the command prefix reaching the systemd host, empty for this machine}
# The same server, run so that no executable is reachable on PATH.
# Override where the plain prefix cannot carry the environment, e.g.
# MCPD_NO_PATH="docker exec -i -e PATH=/nonexistent sysd /usr/local/bin/systemd-mcpd".
MCPD_NO_PATH=${MCPD_NO_PATH:-env PATH=/nonexistent $MCPD}
SOCKET=/run/systemd/io.systemd.Manager

fail() { echo "FAIL: $*" >&2; exit 1; }

request() { # request <tool> <args-json> -> one tools/call request line
  printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' "$1" "$2"
}

call_tool() { # call_tool <scopes> <tool> <args-json> -> inner result text
  request "$2" "$3" | $MCPD --grant "$1" | jq -r '.result.content[0].text'
}

# The same call with systemctl unreachable: only varlink can answer.
call_tool_no_path() { # call_tool_no_path <scopes> <tool> <args-json> -> raw reply
  request "$2" "$3" | $MCPD_NO_PATH --grant "$1"
}

list_units() { call_tool units:read list_units '{}'; }

# A long-lived canary both runs must see. The name is unique per run:
# a leftover unit from an earlier run would let this one satisfy the
# assertions below even if starting it failed.
CANARY_UNIT="mcpd-diff-canary-$$-${RANDOM}.service"
# Captures go in a per-run directory: fixed /tmp names collide with
# files an earlier run left behind under a different user.
WORK=$(mktemp -d)

# Restore the socket and drop the canary however this script exits. The
# assertions below abort on failure, and the window where the socket is
# renamed must not outlive the run.
cleanup() {
  if [ -e "$SOCKET.hidden" ]; then
    $HOST rm -f "$SOCKET"
    $HOST mv "$SOCKET.hidden" "$SOCKET"
  fi
  $HOST systemctl stop "$CANARY_UNIT" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# stderr into a variable, not a file: see the note in integration.sh.
if ! canary_err=$($HOST systemd-run --unit="$CANARY_UNIT" --collect \
    /bin/sleep 300 2>&1 >/dev/null); then
  fail "could not start canary unit: $canary_err"
fi

# 1. Force the CLI backend: hide the varlink socket.
$HOST mv "$SOCKET" "$SOCKET.hidden"
list_units >"$WORK/cli.json"
$HOST mv "$SOCKET.hidden" "$SOCKET"

# 1b. A live varlink failure that isn't a missing socket: a plain file
#     at the socket path fails connect() with ENOTSOCK for every caller
#     (chmod tricks wouldn't work, since root bypasses file permissions).
#     The fallback must still answer.
$HOST mv "$SOCKET" "$SOCKET.hidden"
$HOST touch "$SOCKET"
list_units | jq -e 'type == "array" and length > 0' >/dev/null ||
  fail "list_units did not fall back on a dead varlink socket path"
$HOST rm "$SOCKET"
$HOST mv "$SOCKET.hidden" "$SOCKET"

# 2. Force the varlink backend: run the server with an empty PATH, so
#    systemctl cannot be executed. If list_units still answers, only
#    the socket could have answered.
call_tool_no_path units:read list_units '{}' |
  jq -r '.result.content[0].text' >"$WORK/varlink.json"
jq -e 'type == "array" and length > 0' "$WORK/varlink.json" >/dev/null ||
  fail "list_units did not answer over varlink with systemctl unreachable"
# boot_times must also answer; Manager.Describe is its only possible
# source here...
call_tool_no_path boot:read boot_times '{}' |
  jq -e '.result.content[0].text | fromjson | .total_usec > 0' >/dev/null ||
  fail "boot_times did not answer over varlink Describe"
# ...and a systemctl-backed tool must fail, proving the server cannot
# shell out behind our back.
call_tool_no_path units:read unit_properties '{"unit":"systemd-journald.service"}' |
  jq -e '.result.isError' >/dev/null ||
  fail "unit_properties succeeded with systemctl unreachable"

for f in "$WORK/cli.json" "$WORK/varlink.json"; do
  jq -e --arg u "$CANARY_UNIT" 'map(.unit) | index($u)' "$f" >/dev/null ||
    fail "canary unit missing from $f"
done

# Same shape from either backend, the module's stated contract.
diff <(jq -S 'map(keys) | unique' "$WORK/cli.json") \
  <(jq -S 'map(keys) | unique' "$WORK/varlink.json") ||
  fail "backends disagree on row shape"

echo "PASS: varlink proof and differential"
