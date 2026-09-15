#!/usr/bin/env bash
# automation release-park: the documented operator release path for an
# operational park. No systemd mutation; --unit is the address.
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
mkdir -p "$xdg/systemd/user"
cat >"$xdg/systemd/user/$STEM.service" <<'EOF'
[Service]
ExecStart=/bin/true
EOF
export XDG_CONFIG_HOME=$xdg

json() { env -u SYSTEMD_OPS_OPERATION -u SYSTEMD_OPS_SCOPE_ROOT \
  "$BIN" --json --manager user --scope-root "$SCOPE" "$@"; }

STATE=$SCOPE/.systemd-ops/operations/$STEM/state
mkdir -p "$STATE"

plant_park() {
  jq -nc '{version:1,input_fingerprint:"fp-1",last_kind:"crash",parked_at:"2026-09-15T05:51:30Z"}' \
    >"$STATE/operational-park.json"
  jq -nc '{version:1,input_fingerprint:"fp-1",failures:2,last_kind:"crash",last_at:"2026-09-15T05:51:30Z"}' \
    >"$STATE/failure-budget.json"
}

# A park whose blocker is its own record: release clears both, keeps the budget.
plant_park
jq -nc '{version:2,id:"b1",input_fingerprint:"fp-1",kind:"iteration-failed",route:"parent",at:"2026-09-15T05:51:30Z",code:0,summary:"parked after 2 identical crash failures on unchanged input"}' \
  >"$STATE/blocker.json"
# A failing CLI still prints its error envelope and exits non-zero; tolerate the
# exit so the assertion, not the shell, reports the mismatch.
out=$(json automation release-park --unit "$STEM" || true)
ok "$out"
jq -e '.data.released == true and .data.blocker_cleared == true
    and .data.park.last_kind == "crash"' <<<"$out" >/dev/null \
  || fail "release reply: $out"
[[ ! -e $STATE/operational-park.json ]] || fail "release left the park sidecar"
[[ ! -e $STATE/blocker.json ]] || fail "release left the park blocker"
[[ -e $STATE/failure-budget.json ]] || fail "release reset the failure budget"
jq -e '.activity[-1].text | contains("released operational park")' \
  "$STATE/operator.json" >/dev/null || fail "release left no audit activity"

# Idempotent: no park is a success with released:false.
out=$(json automation release-park --unit "$STEM" || true)
ok "$out"
jq -e '.data.released == false' <<<"$out" >/dev/null || fail "no-park release: $out"

# A semantic blocker is not the park's record and survives the release.
plant_park
jq -nc '{version:2,id:"b2",input_fingerprint:"fp-1",kind:"semantic-blocked",route:"lead",at:"2026-09-15T06:00:00Z",summary:"real semantic block"}' \
  >"$STATE/blocker.json"
out=$(json automation release-park --unit "$STEM" || true)
ok "$out"
jq -e '.data.released == true and .data.blocker_cleared == false' <<<"$out" >/dev/null \
  || fail "semantic blocker release reply: $out"
jq -e '.kind == "semantic-blocked"' "$STATE/blocker.json" >/dev/null \
  || fail "release cleared a semantic blocker"

# A symlinked blocker is refused before the park is removed: the refusal leaves
# the park sidecar in place so the release can be retried, and the CLI exits
# non-zero.
plant_park
rm -f "$STATE/blocker.json"
real_blocker=$TMP/real-blocker.json
jq -nc '{version:2,id:"b3",input_fingerprint:"fp-1",kind:"iteration-failed",route:"parent",at:"2026-09-15T05:51:30Z",code:0,summary:"parked after 2 identical crash failures on unchanged input"}' \
  >"$real_blocker"
ln -s "$real_blocker" "$STATE/blocker.json"
code=0
out=$(json automation release-park --unit "$STEM") || code=$?
[[ $code -ne 0 ]] || fail "a symlinked blocker release exited 0"
rejected "$out"
jq -e '.error.message | contains("symlink")' <<<"$out" >/dev/null \
  || fail "symlinked blocker release: $out"
[[ -e $STATE/operational-park.json ]] || fail "a refused release removed the park sidecar"
[[ -L $STATE/blocker.json ]] || fail "a refused release removed the blocker symlink"
rm -f "$STATE/blocker.json"

# A park fingerprint longer than the activity-text bound cannot be audited, so
# the release is refused before the park is removed and the CLI exits non-zero.
long_fp=$(printf 'f%.0s' {1..1001})
[[ ${#long_fp} -eq 1001 ]] || fail "long fingerprint fixture is ${#long_fp} chars"
jq -nc --arg fp "$long_fp" \
  '{version:1,input_fingerprint:$fp,last_kind:"crash",parked_at:"2026-09-15T05:51:30Z"}' \
  >"$STATE/operational-park.json"
code=0
out=$(json automation release-park --unit "$STEM") || code=$?
[[ $code -ne 0 ]] || fail "an over-long park fingerprint release exited 0"
rejected "$out"
jq -e '.error.message | contains("activity.text")' <<<"$out" >/dev/null \
  || fail "over-long park fingerprint release: $out"
[[ -e $STATE/operational-park.json ]] || fail "a refused release removed the park sidecar"

# Guards: an unowned stem and a missing --unit are refused.
out=$(json automation release-park --unit managed-cpa-farm-cliproxy-watch || true)
rejected "$out"
jq -e '.error.message | contains("restricted to owned stems")' <<<"$out" >/dev/null \
  || fail "unowned release: $out"
out=$(json automation release-park || true)
rejected "$out"
jq -e '.error.message | contains("missing --unit")' <<<"$out" >/dev/null \
  || fail "missing --unit: $out"

echo "park-release ok"
