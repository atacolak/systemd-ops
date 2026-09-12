#!/usr/bin/env bash
# Unbound lead-session operator report. No systemd mutation.
# Does not require SYSTEMD_OPS_OPERATION; --unit is the address.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
TMP=$(mktemp -d)
SCOPE=$TMP/scope
STEM=managed-proof-watch
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { jq -e '.ok == true' <<<"$1" >/dev/null || fail "expected ok: $1"; }
rejected() { jq -e '.ok == false' <<<"$1" >/dev/null || fail "expected rejection: $1"; }

command -v jq >/dev/null || fail "jq required"
[[ -x $BIN ]] || fail "missing $BIN"

mkdir -p "$SCOPE/.systemd-ops"
cat >"$SCOPE/.systemd-ops/scope.toml" <<'EOF'
[scope]
id = "proof"
owned = ["managed-proof-*"]
EOF

xdg=$TMP/xdg
unit_dir=$xdg/systemd/user
mkdir -p "$unit_dir"
cat >"$unit_dir/$STEM.service" <<'EOF'
[Service]
ExecStart=/bin/true
EOF
export XDG_CONFIG_HOME=$xdg

# Bound automation_report still requires SYSTEMD_OPS_OPERATION.
bound_missing=$(env -u SYSTEMD_OPS_OPERATION -u SYSTEMD_OPS_SCOPE_ROOT \
  "$BIN" --json --manager user --scope-root "$SCOPE" \
  automation report --headline x --summary '["x"]' --outcome ready || true)
rejected "$bound_missing"
jq -e '.error.message | contains("SYSTEMD_OPS_OPERATION is required")' <<<"$bound_missing" >/dev/null \
  || fail "bound report should still require SYSTEMD_OPS_OPERATION: $bound_missing"

# operator report is stem-addressed from an unbound session.
json() { env -u SYSTEMD_OPS_OPERATION -u SYSTEMD_OPS_SCOPE_ROOT \
  "$BIN" --json --manager user --scope-root "$SCOPE" "$@"; }

missing=$(json operator report --headline "x" --summary '["x"]' --outcome ready || true)
rejected "$missing"
jq -e '.error.message | contains("missing --unit")' <<<"$missing" >/dev/null \
  || fail "report without --unit: $missing"

unowned=$(json operator report --unit managed-cpa-farm-cliproxy-watch \
  --headline "x" --summary '["x"]' --outcome ready || true)
rejected "$unowned"
jq -e '.error.message | contains("restricted to owned stems")' <<<"$unowned" >/dev/null \
  || fail "unowned report: $unowned"

no_iter=$(json operator report --unit "$STEM" --headline "x" --summary '["x"]' --outcome ready || true)
rejected "$no_iter"
jq -e '.error.message | contains("active iteration")' <<<"$no_iter" >/dev/null \
  || fail "report without iteration: $no_iter"

start=$(json operator iteration-start --unit "$STEM")
ok "$start"
iteration=$(jq -er '.data.iteration_id' <<<"$start")

blocked_no_route=$(json operator report --unit "$STEM" --headline "newer tag" \
  --summary '["paged the lead"]' --outcome blocked || true)
rejected "$blocked_no_route"

report=$(json operator report --unit "$STEM" --headline "newer GitHub tag" \
  --summary '["paged the lead; upgrade is theirs"]' --outcome blocked --route lead)
ok "$report"
jq -e '
  .data.reported == true and
  .data.unit == "managed-proof-watch" and
  .data.operator.headline == "newer GitHub tag" and
  .data.operator.outcome == "blocked" and
  .data.operator.route == "lead" and
  (.data.operator.active_iteration.reported_at | type == "string")
' <<<"$report" >/dev/null || fail "blocked report state: $report"

finish=$(json operator iteration-finish --unit "$STEM" --iteration "$iteration" --exit-code 0)
ok "$finish"
jq -e '
  .data.reconsolidated == true and
  .data.operator.iterations[0].outcome == "blocked" and
  .data.operator.iterations[0].route == "lead" and
  .data.operator.iterations[0].headline == "newer GitHub tag"
' <<<"$finish" >/dev/null || fail "blocked reconsolidation: $finish"

# --cwd discovers the scope when --scope-root is omitted.
start=$(env -u SYSTEMD_OPS_OPERATION -u SYSTEMD_OPS_SCOPE_ROOT \
  "$BIN" --json --manager user --cwd "$SCOPE" operator iteration-start --unit "$STEM")
ok "$start"
iteration=$(jq -er '.data.iteration_id' <<<"$start")
# Leftover bound env must not steal the --unit address.
report=$(SYSTEMD_OPS_OPERATION=managed-proof-other \
  env -u SYSTEMD_OPS_SCOPE_ROOT \
  "$BIN" --json --manager user --cwd "$SCOPE" operator report --unit "$STEM" \
  --headline "ready after cutover" --summary '["installed"]' --outcome ready)
ok "$report"
jq -e '.data.unit == "managed-proof-watch" and .data.operator.outcome == "ready"' <<<"$report" >/dev/null \
  || fail "cwd/unbound report: $report"

echo "operator-report ok"
