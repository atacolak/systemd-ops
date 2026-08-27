#!/usr/bin/env bash
# Environment-bound autonomous CLI proofs. No systemd mutation.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
TMP=$(mktemp -d)
SCOPE=$TMP/scope
STEM=managed-proof-agent
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { jq -e '.ok == true' <<<"$1" >/dev/null || fail "expected ok: $1"; }
rejected() { jq -e '.ok == false' <<<"$1" >/dev/null || fail "expected rejection: $1"; }
json() { "$BIN" --json --manager user --scope-root "$SCOPE" "$@"; }

command -v jq >/dev/null || fail "jq required"
[[ -x $BIN ]] || fail "missing $BIN"
mkdir -p "$SCOPE/.systemd-ops"
cat >"$SCOPE/.systemd-ops/scope.toml" <<'EOF'
[scope]
id = "proof"
owned = ["managed-proof-*"]

[[watch]]
operation = "managed-external-watch"
EOF
export SYSTEMD_OPS_SCOPE_ROOT=$SCOPE
export SYSTEMD_OPS_OPERATION=$STEM

xdg=$TMP/xdg
unit_dir=$xdg/systemd/user
mkdir -p "$unit_dir"
cat >"$unit_dir/$STEM.service" <<'EOF'
[Service]
ExecStart=/bin/true
EOF
export XDG_CONFIG_HOME=$xdg
missing=$(env -u SYSTEMD_OPS_OPERATION "$BIN" --json --manager user --scope-root "$SCOPE" automation context || true)
rejected "$missing"
jq -e '.error.message | contains("SYSTEMD_OPS_OPERATION is required")' <<<"$missing" >/dev/null || fail "missing binding error: $missing"

outside=$(SYSTEMD_OPS_OPERATION=managed-external-watch json automation context || true)
rejected "$outside"
jq -e '.error.message | contains("restricted to owned stems")' <<<"$outside" >/dev/null || fail "watched binding error: $outside"

cross=$(json automation report --unit managed-proof-other --headline nope --summary '["nope"]' || true)
rejected "$cross"
jq -e '.error.message | contains("unknown argument")' <<<"$cross" >/dev/null || fail "cross-operation argument accepted: $cross"

start=$(json operator iteration-start --unit "$STEM")
ok "$start"
iteration=$(jq -er '.data.iteration_id' <<<"$start")

out=$(json automation report --headline '' --summary '["one"]' || true)
rejected "$out"
out=$(json automation report --headline $'one\nline' --summary '["one"]' || true)
rejected "$out"

long_headline=$(printf 'x%.0s' {1..81})
out=$(json automation report --headline "$long_headline" --summary '["one"]' || true)
rejected "$out"
out=$(json automation report --headline valid --summary '[]' || true)
rejected "$out"
out=$(json automation report --headline valid --summary '[" "]' || true)
rejected "$out"
out=$(json automation report --headline valid --summary $'["one\ntwo"]' || true)
rejected "$out"
out=$(json automation report --headline valid --summary '["1","2","3","4","5","6"]' || true)
rejected "$out"
long_summary=$(printf 'x%.0s' {1..281})
out=$(json automation report --headline valid --summary "[\"$long_summary\"]" || true)
rejected "$out"
long_activity=$(printf 'x%.0s' {1..201})
out=$(json automation activity --text "$long_activity" || true)
rejected "$out"

report=$(json automation report --headline "proof is current" --summary '["first paragraph","second paragraph"]')
ok "$report"
jq -e '
  .data.reported == true and
  .data.operator.headline == "proof is current" and
  .data.operator.body == "first paragraph\n\nsecond paragraph" and
  (.data.operator.updated_at | type == "string") and
  (.data.operator.active_iteration.reported_at | type == "string") and
  (.data.operator.basis_revision | startswith("sha256:")) and
  .data.operator.basis_revision == .data.definition_revision
' <<<"$report" >/dev/null || fail "report state mismatch: $report"
jq -e '
  (.data.operator.headline | length) <= 80 and
  ([.data.operator.body | split("\n\n")[] | length <= 280] | all)
' <<<"$report" >/dev/null || fail "report exceeded concise limits: $report"

finish=$(json operator iteration-finish --unit "$STEM" --iteration "$iteration" --exit-code 0)
ok "$finish"
jq -e '
  .data.reconsolidated == true and
  .data.operator.iterations[0].headline == "proof is current" and
  .data.operator.iterations[0].summary == "first paragraph\n\nsecond paragraph"
' <<<"$finish" >/dev/null || fail "report did not reconsolidate: $finish"

start=$(json operator iteration-start --unit "$STEM")
iteration=$(jq -er '.data.iteration_id' <<<"$start")
activity=$(json automation activity --text "notable milestone")
ok "$activity"
jq -e '.data.appended == true and .data.operator.active_iteration.reported_at == null' <<<"$activity" >/dev/null || fail "activity marked report: $activity"
jq -e '(.data.operator.activity[-1].text | length) <= 200' <<<"$activity" >/dev/null || fail "activity exceeded concise limit: $activity"
finish=$(json operator iteration-finish --unit "$STEM" --iteration "$iteration" --exit-code 0)
ok "$finish"
jq -e '.data.reconsolidated == false' <<<"$finish" >/dev/null || fail "activity alone reconsolidated: $finish"

echo "automation-cli ok"
