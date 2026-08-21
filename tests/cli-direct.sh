#!/usr/bin/env bash
# Direct CLI proofs. No MCP process. Disposable units only.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
STATE=$(mktemp -d)
CFG=$STATE/config.toml
export SYSTEMD_OPS_STATE_DIR=$STATE
cat >"$CFG" <<EOF
manager = "user"
write_prefix = "managed-*"
plan_ttl_secs = 120
EOF

fail() { echo "FAIL: $*" >&2; exit 1; }
json() { "$BIN" --json --manager user --write-prefix 'managed-*' --config "$CFG" "$@"; }

ok() {
  local out=$1
  echo "$out" | jq -e '.ok == true' >/dev/null || fail "expected ok: $out"
}
err_code() {
  local out=$1 want=$2
  echo "$out" | jq -e --arg c "$want" '.ok == false and .error.code == $c' >/dev/null \
    || fail "expected error $want: $out"
}

command -v jq >/dev/null || fail "jq required"
[[ -x $BIN ]] || fail "missing $BIN"

pgrep -af '[s]ystemd-ops-mcp' >/dev/null && fail "systemd-ops-mcp should not be running for CLI tests" || true

out=$(json inspect list-operations --pattern 'managed-*')
ok "$out"

out=$(json control plan --action stop --unit bluetooth.service || true)
err_code "$out" forbidden

STEM=managed-cli-tmp-$$
UNIT=$STEM.service
USER_DIR=$HOME/.config/systemd/user
trap 'systemctl --user stop "$UNIT" 2>/dev/null || true
      systemctl --user disable "$UNIT" 2>/dev/null || true
      rm -f "$USER_DIR/$UNIT" "$USER_DIR/$STEM.timer"
      systemctl --user daemon-reload
      rm -rf "$STATE"' EXIT

create=$(json author plan-create --spec "$(jq -n --arg u "$STEM" '{
  unit: $u,
  kind: "oneshot",
  title: "CLI tmp",
  purpose: "direct-cli proof",
  tags: ["test"],
  exec: {path: "/bin/true", argv: []},
  enabled: false,
  start_now: false
}')")
ok "$create"
token=$(echo "$create" | jq -r '.data.plan_token')
[[ $token == v1.* ]] || fail "token shape: $token"

cross=$(json control apply --plan-token "$token" || true)
echo "$cross" | jq -e '.ok == false' >/dev/null || fail "control applied author token: $cross"

applied=$(json author apply --plan-token "$token")
ok "$applied"
[[ -f $USER_DIR/$UNIT ]] || fail "unit file not written"
grep -q 'managed: systemd-ops 1' "$USER_DIR/$UNIT" || fail "missing managed marker"

plan=$(json control plan --action start --unit "$UNIT")
ok "$plan"
ctoken=$(echo "$plan" | jq -r '.data.plan_token')

cross2=$(json author apply --plan-token "$ctoken" || true)
echo "$cross2" | jq -e '.ok == false' >/dev/null || fail "author applied control token: $cross2"

tampered=${ctoken%?}a
bad=$(json control apply --plan-token "$tampered" || true)
echo "$bad" | jq -e '.ok == false' >/dev/null || fail "tampered token applied: $bad"

started=$(json control apply --plan-token "$ctoken")
ok "$started"

retire=$(json author plan-retire --unit "$STEM")
ok "$retire"
rtoken=$(echo "$retire" | jq -r '.data.plan_token')
json author apply --plan-token "$rtoken" >/dev/null
[[ ! -f $USER_DIR/$UNIT ]] || fail "retired unit still on disk"

echo "cli-direct ok"
