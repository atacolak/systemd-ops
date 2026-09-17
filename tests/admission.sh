#!/usr/bin/env bash
# Existing-capability plan-admit. Throwaway scope; no unit create.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${SYSTEMD_OPS_BIN:-$ROOT/target/debug/systemd-ops}
TMP=$(mktemp -d)
SCOPE=$TMP/scope
CAP=existing-cap
STEM=managed-omp-cap-$CAP
HOME_DIR=$SCOPE/.systemd-ops/operations/$STEM
DESIRED=$HOME_DIR/state/desired-inputs.json
UNIT_DIR=$TMP/xdg/systemd/user
UNIT=$UNIT_DIR/$STEM.service
AGENT=$TMP/agents
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { jq -e '.ok == true' <<<"$1" >/dev/null || fail "expected ok: $1"; }
rejected() { jq -e '.ok == false' <<<"$1" >/dev/null || fail "expected rejection: $1"; }
file_hash() { sha256sum "$1" | awk '{print $1}'; }

command -v jq >/dev/null || fail "jq required"
command -v sha256sum >/dev/null || fail "sha256sum required"
[[ -x $BIN ]] || fail "missing $BIN"

mkdir -p "$HOME_DIR" "$UNIT_DIR" "$SCOPE/.systemd-ops" "$AGENT/.omp/agents" "$TMP/bin"
printf '%s\n' 'capability-maintainer role' >"$AGENT/.omp/agents/capability-maintainer.md"

cat >"$SCOPE/.systemd-ops/scope.toml" <<EOF
[scope]
id = "proof"
owned = ["managed-omp-cap-*"]

[automation]
agent_root = "$AGENT"
EOF

cat >"$HOME_DIR/automation.toml" <<'EOF'
version = 1
agent = "capability-maintainer"
parent = "managed-omp-runtime-integrator"

[observation]
exec = "observe-cap"
EOF

cat >"$SCOPE/.systemd-ops/observe-cap" <<'EOF'
#!/bin/sh
printf '%s\n' '{"version":1,"world_fingerprint":"world-admit","generation":null}'
EOF
chmod 755 "$SCOPE/.systemd-ops/observe-cap"

cat >"$UNIT" <<EOF
# Managed by systemd-ops
[Unit]
Description=$CAP
[Service]
Type=oneshot
ExecStart=/bin/true
WorkingDirectory=$SCOPE
EOF

cat >"$TMP/bin/systemctl" <<EOF
#!/bin/sh
while [ "\${1:-}" = --user ]; do shift; done
case " \$* " in
  *" list-unit-files "*)
    printf '%s\n' '[{"unit_file":"$STEM.service","state":"static","preset":"enabled"}]'
    ;;
  *" list-units "*|*" list-timers "*)
    printf '%s\n' '[]'
    ;;
  *" show "*)
    printf 'LoadState=loaded\nActiveState=inactive\nSubState=dead\nFragmentPath=%s\n' "$UNIT"
    ;;
  *)
    printf '%s\n' '[]'
    ;;
esac
EOF
chmod 755 "$TMP/bin/systemctl"

export PATH=$TMP/bin:$PATH
export XDG_CONFIG_HOME=$TMP/xdg
export SYSTEMD_OPS_STATE_DIR=$TMP/state
export SYSTEMD_OPS_SCOPE_ROOT=$SCOPE
export SYSTEMD_OPS_OPERATION=$STEM

json() {
  "$BIN" --json --manager user --write-prefix 'managed-*' --scope-root "$SCOPE" "$@"
}

admit() {
  json automation plan-admit --spec "$1"
}

apply_token() {
  local token=$1
  json automation apply --plan-token "$token"
}

admit_apply() {
  local planned applied
  planned=$(admit "$1")
  ok "$planned"
  apply_token "$(jq -er '.data.plan_token' <<<"$planned")"
}

COMMIT_REV=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
COMMIT_SPEC=$(jq -nc --arg rev "$COMMIT_REV" --arg cap "$CAP" '{
  version: 1,
  originating_work: "omp-tytn.1",
  artifact_revision: $rev,
  capability: $cap
}')
DELTA_SHA=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
DELTA_SPEC=$(jq -nc --arg cap "$CAP" --arg sha "$DELTA_SHA" '{
  version: 1,
  originating_work: "omp-tytn.2",
  capability: $cap,
  artifact_kind: "worktree-delta",
  base: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  delta: [{path: "src/foo.rs", sha256: $sha}]
}')

unit_hash=$(file_hash "$UNIT")
meta_hash=$(file_hash "$HOME_DIR/automation.toml")
[[ ! -e $DESIRED ]] || fail "desired-inputs.json existed before admit"

before=$(json automation observe)
ok "$before"
before_fp=$(jq -er '.data.observation.input_fingerprint' <<<"$before")

applied=$(admit_apply "$COMMIT_SPEC")
ok "$applied"
[[ -f $DESIRED ]] || fail "commit admit did not write desired-inputs.json"
jq -e --arg rev "$COMMIT_REV" '
  .version == 1 and
  (.inputs | length) == 1 and
  .inputs[0].originating_work == "omp-tytn.1" and
  .inputs[0].artifact_kind == "commit" and
  .inputs[0].artifact_revision == $rev and
  .inputs[0].artifact_identity == $rev and
  (.inputs[0].admitted_at | type == "string") and
  (.inputs[0] | has("base") | not) and
  (.inputs[0] | has("delta") | not)
' "$DESIRED" >/dev/null || fail "commit desired-inputs: $(cat "$DESIRED")"
commit_bytes=$(cat "$DESIRED")
commit_hash=$(file_hash "$DESIRED")

after=$(json automation observe)
ok "$after"
after_fp=$(jq -er '.data.observation.input_fingerprint' <<<"$after")
[[ $after_fp != "$before_fp" ]] || fail "observe did not move input_fingerprint after commit admit"

dup=$(admit_apply "$COMMIT_SPEC")
ok "$dup"
[[ $(cat "$DESIRED") == "$commit_bytes" ]] || fail "duplicate commit admit rewrote desired-inputs"
[[ $(file_hash "$DESIRED") == "$commit_hash" ]] || fail "duplicate commit admit changed desired-inputs hash"
dup_obs=$(json automation observe)
ok "$dup_obs"
[[ $(jq -er '.data.observation.input_fingerprint' <<<"$dup_obs") == "$after_fp" ]] \
  || fail "duplicate commit admit moved input_fingerprint"

applied=$(admit_apply "$DELTA_SPEC")
ok "$applied"
jq -e --arg sha "$DELTA_SHA" '
  .version == 1 and
  (.inputs | length) == 2 and
  .inputs[1].originating_work == "omp-tytn.2" and
  .inputs[1].artifact_kind == "worktree-delta" and
  .inputs[1].base == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" and
  .inputs[1].delta == [{path: "src/foo.rs", sha256: $sha}] and
  (.inputs[1].artifact_identity | test("^[0-9a-f]{64}$")) and
  (.inputs[1].admitted_at | type == "string") and
  (.inputs[1] | has("artifact_revision") | not)
' "$DESIRED" >/dev/null || fail "worktree-delta desired-inputs: $(cat "$DESIRED")"
delta_bytes=$(cat "$DESIRED")
delta_hash=$(file_hash "$DESIRED")
[[ $delta_hash != "$commit_hash" ]] || fail "worktree-delta admit left desired-inputs unchanged"

delta_obs=$(json automation observe)
ok "$delta_obs"
delta_fp=$(jq -er '.data.observation.input_fingerprint' <<<"$delta_obs")
[[ $delta_fp != "$after_fp" ]] || fail "observe did not move input_fingerprint after worktree-delta admit"

dup=$(admit_apply "$DELTA_SPEC")
ok "$dup"
[[ $(cat "$DESIRED") == "$delta_bytes" ]] || fail "duplicate worktree-delta admit rewrote desired-inputs"
[[ $(file_hash "$DESIRED") == "$delta_hash" ]] || fail "duplicate worktree-delta admit changed hash"
dup_obs=$(json automation observe)
ok "$dup_obs"
[[ $(jq -er '.data.observation.input_fingerprint' <<<"$dup_obs") == "$delta_fp" ]] \
  || fail "duplicate worktree-delta admit moved input_fingerprint"

refuse_unchanged() {
  local spec=$1 label=$2
  local out
  out=$(admit "$spec" || true)
  rejected "$out"
  [[ $(cat "$DESIRED") == "$delta_bytes" ]] || fail "$label wrote desired-inputs: $out"
  [[ $(file_hash "$UNIT") == "$unit_hash" ]] || fail "$label rewrote the unit"
  [[ $(file_hash "$HOME_DIR/automation.toml") == "$meta_hash" ]] || fail "$label rewrote automation.toml"
}

refuse_unchanged "$(jq -nc --arg cap "$CAP" --arg rev "$COMMIT_REV" '{
  version: 1,
  originating_work: "overseer",
  artifact_revision: $rev,
  capability: $cap
}')" "overseer originating_work"

refuse_unchanged "$(jq -nc --arg rev "$COMMIT_REV" '{
  version: 1,
  originating_work: "omp-tytn.1",
  artifact_revision: $rev,
  capability: "overseer"
}')" "overseer capability"

refuse_unchanged "$(jq -nc --arg cap "$CAP" '{
  version: 1,
  originating_work: "omp-tytn.1",
  artifact_revision: "not-a-40-hex-revision",
  capability: $cap
}')" "revision not 40 hex"

refuse_unchanged "$(jq -nc --arg cap "$CAP" --arg rev "$COMMIT_REV" --arg sha "$DELTA_SHA" '{
  version: 1,
  originating_work: "omp-tytn.3",
  capability: $cap,
  artifact_kind: "worktree-delta",
  artifact_revision: $rev,
  base: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  delta: [{path: "src/foo.rs", sha256: $sha}]
}')" "mixed kinds"

refuse_unchanged "$(jq -nc --arg cap "$CAP" '{
  version: 1,
  originating_work: "omp-tytn.3",
  capability: $cap,
  artifact_kind: "worktree-delta",
  base: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  delta: []
}')" "empty delta"

refuse_unchanged "$(jq -nc --arg cap "$CAP" --arg sha "$DELTA_SHA" '{
  version: 1,
  originating_work: "omp-tytn.3",
  capability: $cap,
  artifact_kind: "worktree-delta",
  base: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  delta: [{path: "../secret.rs", sha256: $sha}]
}')" ".. path"

[[ $(file_hash "$UNIT") == "$unit_hash" ]] || fail "admit rewrote the unit file"
[[ $(file_hash "$HOME_DIR/automation.toml") == "$meta_hash" ]] || fail "admit rewrote automation.toml"
[[ $(find "$UNIT_DIR" -type f | wc -l) -eq 1 ]] || fail "admit created or removed a unit file"
[[ ! -e /home/sf/worktrees/omp/admit/retry-fallback-unowned ]] \
  || fail "classified admit home retry-fallback-unowned exists"
[[ ! -e /home/sf/worktrees/omp/admit/todo-passive-phase ]] \
  || fail "classified admit home todo-passive-phase exists"

echo "admission existing-capability ok"
