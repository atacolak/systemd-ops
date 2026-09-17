#!/usr/bin/env bash
# Existing-capability plan-admit, then new-capability create. Throwaway scope.
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

# Supersedes proofs: throwaway existing capability with one desired input.
REPLACE_CAP=replace-cap
REPLACE_STEM=managed-omp-cap-$REPLACE_CAP
REPLACE_HOME=$SCOPE/.systemd-ops/operations/$REPLACE_STEM
REPLACE_DESIRED=$REPLACE_HOME/state/desired-inputs.json
REPLACE_UNIT=$UNIT_DIR/$REPLACE_STEM.service
REPLACE_OLD=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
REPLACE_NEW=cccccccccccccccccccccccccccccccccccccccc

mkdir -p "$REPLACE_HOME"
cp "$HOME_DIR/automation.toml" "$REPLACE_HOME/automation.toml"
cat >"$REPLACE_UNIT" <<EOF
# Managed by systemd-ops
[Unit]
Description=$REPLACE_CAP
[Service]
Type=oneshot
ExecStart=/bin/true
WorkingDirectory=$SCOPE
EOF

cat >"$TMP/bin/git" <<EOF
#!/bin/sh
printf '%s\\n' "\$*" >>"$TMP/git.log"
case " \$* " in
  *" worktree add "*)
    echo "git worktree add is forbidden in admission proof" >&2
    exit 1
    ;;
  *)
    exit 0
    ;;
esac
EOF
chmod 755 "$TMP/bin/git"

cat >"$TMP/bin/systemctl" <<EOF
#!/bin/sh
unit_dir=$UNIT_DIR
while [ "\${1:-}" = --user ]; do shift; done
case " \$* " in
  *" list-unit-files "*)
    printf '['
    sep=
    for f in "\$unit_dir"/*; do
      [ -f "\$f" ] || continue
      printf '%s{"unit_file":"%s","state":"static","preset":"enabled"}' "\$sep" "\$(basename "\$f")"
      sep=,
    done
    printf ']\\n'
    ;;
  *" list-units "*|*" list-timers "*)
    printf '%s\\n' '[]'
    ;;
  *" show "*)
    unit=\${*##* }
    printf 'LoadState=loaded\\nActiveState=inactive\\nSubState=dead\\nFragmentPath=%s/%s\\n' "\$unit_dir" "\$unit"
    ;;
  *)
    printf '%s\\n' '[]'
    ;;
esac
EOF
chmod 755 "$TMP/bin/systemctl"

replace_unit_hash=$(file_hash "$REPLACE_UNIT")
replace_meta_hash=$(file_hash "$REPLACE_HOME/automation.toml")
export SYSTEMD_OPS_OPERATION=$REPLACE_STEM

REPLACE_OLD_SPEC=$(jq -nc --arg rev "$REPLACE_OLD" --arg cap "$REPLACE_CAP" '{
  version: 1,
  originating_work: "omp-tytn.6",
  artifact_revision: $rev,
  capability: $cap
}')
applied=$(admit_apply "$REPLACE_OLD_SPEC")
ok "$applied"
jq -e --arg rev "$REPLACE_OLD" '
  .version == 1 and
  (.inputs | length) == 1 and
  .inputs[0].artifact_identity == $rev
' "$REPLACE_DESIRED" >/dev/null || fail "replace-cap seed: $(cat "$REPLACE_DESIRED")"

replace_before=$(json automation observe)
ok "$replace_before"
replace_fp=$(jq -er '.data.observation.input_fingerprint' <<<"$replace_before")
before_replace_files=$(find "$SCOPE" "$UNIT_DIR" -type f | sort)

REPLACE_SPEC=$(jq -nc --arg rev "$REPLACE_NEW" --arg old "$REPLACE_OLD" --arg cap "$REPLACE_CAP" '{
  version: 1,
  originating_work: "omp-tytn.6",
  artifact_revision: $rev,
  capability: $cap,
  supersedes: $old
}')
applied=$(admit_apply "$REPLACE_SPEC")
ok "$applied"
jq -e --arg rev "$REPLACE_NEW" --arg old "$REPLACE_OLD" '
  .version == 1 and
  (.inputs | length) == 1 and
  .inputs[0].originating_work == "omp-tytn.6" and
  .inputs[0].artifact_kind == "commit" and
  .inputs[0].artifact_revision == $rev and
  .inputs[0].artifact_identity == $rev and
  (.inputs[0].admitted_at | type == "string") and
  (.inputs | map(.artifact_identity) | index($old) | not)
' "$REPLACE_DESIRED" >/dev/null || fail "supersedes desired-inputs: $(cat "$REPLACE_DESIRED")"

replace_after=$(json automation observe)
ok "$replace_after"
replace_fp_after=$(jq -er '.data.observation.input_fingerprint' <<<"$replace_after")
[[ $replace_fp_after != "$replace_fp" ]] || fail "supersedes did not move input_fingerprint"

[[ $(file_hash "$REPLACE_UNIT") == "$replace_unit_hash" ]] || fail "supersedes rewrote the unit"
[[ $(file_hash "$REPLACE_HOME/automation.toml") == "$replace_meta_hash" ]] || fail "supersedes rewrote automation.toml"

after_replace_files=$(find "$SCOPE" "$UNIT_DIR" -type f | sort)
[[ $after_replace_files == "$before_replace_files" ]] \
  || fail "supersedes wrote extra files: $(comm -13 <(printf '%s\n' "$before_replace_files") <(printf '%s\n' "$after_replace_files"))"

if [[ -f $TMP/git.log ]]; then
  grep -F 'worktree add' "$TMP/git.log" \
    && fail "supersedes invoked git worktree add: $(cat "$TMP/git.log")"
fi

replace_bytes=$(cat "$REPLACE_DESIRED")
replace_refuse() {
  local spec=$1 label=$2
  local out
  out=$(admit "$spec" || true)
  rejected "$out"
  [[ $(cat "$REPLACE_DESIRED") == "$replace_bytes" ]] || fail "$label wrote desired-inputs: $out"
  [[ $(file_hash "$REPLACE_UNIT") == "$replace_unit_hash" ]] || fail "$label rewrote the unit"
  [[ $(file_hash "$REPLACE_HOME/automation.toml") == "$replace_meta_hash" ]] || fail "$label rewrote automation.toml"
}

replace_refuse "$(jq -nc --arg rev "$REPLACE_NEW" --arg cap "$REPLACE_CAP" '{
  version: 1,
  originating_work: "omp-tytn.6",
  artifact_revision: $rev,
  capability: $cap,
  supersedes: "not-hex"
}')" "supersedes not hex"

replace_refuse "$(jq -nc --arg rev "$REPLACE_NEW" --arg cap "$REPLACE_CAP" '{
  version: 1,
  originating_work: "omp-tytn.6",
  artifact_revision: $rev,
  capability: $cap,
  supersedes: $rev
}')" "supersedes equals artifact_identity"

replace_refuse "$(jq -nc --arg rev "$REPLACE_NEW" --arg old "$REPLACE_OLD" --arg cap "$REPLACE_CAP" '{
  version: 1,
  originating_work: "omp-tytn.6",
  artifact_revision: "dddddddddddddddddddddddddddddddddddddddd",
  capability: $cap,
  supersedes: $old
}')" "supersedes identity absent"

ABSENT_CAP=absent-cap
ABSENT_STEM=managed-omp-cap-$ABSENT_CAP
absent=$(admit "$(jq -nc --arg cap "$ABSENT_CAP" --arg old "$REPLACE_NEW" '{
  version: 1,
  originating_work: "omp-tytn.7",
  artifact_revision: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
  capability: $cap,
  supersedes: $old
}')" || true)
rejected "$absent"
[[ $(cat "$REPLACE_DESIRED") == "$replace_bytes" ]] || fail "new-cap supersedes wrote replace-cap desired-inputs"
[[ ! -e $SCOPE/.systemd-ops/operations/$ABSENT_STEM/automation.toml ]]   || fail "new-cap supersedes wrote automation.toml"
[[ ! -e $UNIT_DIR/$ABSENT_STEM.service ]]   || fail "new-cap supersedes created a unit"
[[ ! -e $SCOPE/.systemd-ops/operations/$ABSENT_STEM/state/desired-inputs.json ]]   || fail "new-cap supersedes wrote desired-inputs"

REPLACE_APPEND=ffffffffffffffffffffffffffffffffffffffff
applied=$(admit_apply "$(jq -nc --arg rev "$REPLACE_APPEND" --arg cap "$REPLACE_CAP" '{
  version: 1,
  originating_work: "omp-tytn.6",
  artifact_revision: $rev,
  capability: $cap
}')")
ok "$applied"
jq -e --arg first "$REPLACE_NEW" --arg second "$REPLACE_APPEND" '
  .version == 1 and
  (.inputs | length) == 2 and
  .inputs[0].artifact_identity == $first and
  .inputs[1].artifact_identity == $second and
  .inputs[0].originating_work == "omp-tytn.6" and
  .inputs[1].originating_work == "omp-tytn.6"
' "$REPLACE_DESIRED" >/dev/null || fail "second admit without supersedes did not append: $(cat "$REPLACE_DESIRED")"
[[ $(file_hash "$REPLACE_UNIT") == "$replace_unit_hash" ]] || fail "append without supersedes rewrote the unit"

replace_bytes=$(cat "$REPLACE_DESIRED")
replace_refuse "$(jq -nc --arg rev "$REPLACE_APPEND" --arg old "$REPLACE_NEW" --arg cap "$REPLACE_CAP" '{
  version: 1,
  originating_work: "omp-tytn.6",
  artifact_revision: $rev,
  capability: $cap,
  supersedes: $old
}')" "replacement already exists as another entry"

REPLACE_INPLACE=9999999999999999999999999999999999999999
applied=$(admit_apply "$(jq -nc --arg rev "$REPLACE_INPLACE" --arg old "$REPLACE_NEW" --arg cap "$REPLACE_CAP" '{
  version: 1,
  originating_work: "omp-tytn.6",
  artifact_revision: $rev,
  capability: $cap,
  supersedes: $old
}')")
ok "$applied"
jq -e --arg first "$REPLACE_INPLACE" --arg second "$REPLACE_APPEND" '
  .version == 1 and
  (.inputs | length) == 2 and
  .inputs[0].artifact_identity == $first and
  .inputs[1].artifact_identity == $second
' "$REPLACE_DESIRED" >/dev/null || fail "supersedes did not replace in place: $(cat "$REPLACE_DESIRED")"
[[ $(file_hash "$REPLACE_UNIT") == "$replace_unit_hash" ]] || fail "in-place supersedes rewrote the unit"

export SYSTEMD_OPS_OPERATION=$STEM

echo "admission existing-capability ok"

# New-capability half. Same throwaway scope; no oh-my-pi writes.
PARENT=managed-omp-runtime-integrator
PARENT_HOME=$SCOPE/.systemd-ops/operations/$PARENT
NEW_CAP=new-cap
NEW_STEM=managed-omp-cap-$NEW_CAP
NEW_HOME=$SCOPE/.systemd-ops/operations/$NEW_STEM
NEW_DESIRED=$NEW_HOME/state/desired-inputs.json
NEW_UNIT=$UNIT_DIR/$NEW_STEM.service
NEW_TIMER=$UNIT_DIR/$NEW_STEM.timer
DELTA_CAP=delta-cap
DELTA_STEM=managed-omp-cap-$DELTA_CAP
DELTA_HOME=$SCOPE/.systemd-ops/operations/$DELTA_STEM
DELTA_DESIRED=$DELTA_HOME/state/desired-inputs.json
COLLIDE_CAP=taken-cap
COLLIDE_STEM=managed-omp-cap-$COLLIDE_CAP
SIBLING=managed-omp-cap-sibling
CLASSIFIED=$TMP/classified-src
CANARY_BYTES=classified-canary-bytes

cat >"$SCOPE/.systemd-ops/scope.toml" <<EOF
[scope]
id = "proof"
owned = ["managed-omp-cap-*", "managed-omp-runtime-integrator"]

[automation]
agent_root = "$AGENT"
EOF

mkdir -p "$PARENT_HOME" "$CLASSIFIED" "$TMP/enabled"
printf '%s\n' "$CANARY_BYTES" >"$CLASSIFIED/CANARY"
cat >"$PARENT_HOME/automation.toml" <<'EOF'
version = 1
agent = "capability-maintainer"
EOF
cat >"$UNIT_DIR/$PARENT.service" <<EOF
# Managed by systemd-ops
[Unit]
Description=$PARENT
[Service]
Type=oneshot
ExecStart=/bin/true
WorkingDirectory=$SCOPE
EOF
cat >"$UNIT_DIR/$SIBLING.service" <<EOF
# managed: systemd-ops 1
# systemd-ops-tags: omp,capability-maintainer,capability:$COLLIDE_CAP,branch:cap/$COLLIDE_CAP
[Unit]
Description=sibling collider
[Service]
Type=oneshot
ExecStart=/bin/true
WorkingDirectory=$SCOPE
EOF

cat >"$TMP/bin/systemctl" <<EOF
#!/bin/sh
log=$TMP/systemctl.log
unit_dir=$UNIT_DIR
enabled_dir=$TMP/enabled
mkdir -p "\$enabled_dir"
printf '%s\n' "\$*" >>"\$log"
while [ "\${1:-}" = --user ]; do shift; done
case " \$* " in
  *" list-unit-files "*)
    printf '['
    sep=
    for f in "\$unit_dir"/*; do
      [ -f "\$f" ] || continue
      printf '%s{"unit_file":"%s","state":"static","preset":"enabled"}' "\$sep" "\$(basename "\$f")"
      sep=,
    done
    printf ']\\n'
    ;;
  *" list-units "*|*" list-timers "*)
    printf '%s\\n' '[]'
    ;;
  *" daemon-reload "*)
    ;;
  *" enable "*)
    unit=\${*##* }
    touch "\$enabled_dir/\$unit"
    ;;
  *" start "*|*" stop "*|*" disable "*)
    ;;
  *" show "*)
    unit=\${*##* }
    printf 'LoadState=loaded\\nActiveState=inactive\\nSubState=dead\\nFragmentPath=%s/%s\\n' "\$unit_dir" "\$unit"
    ;;
  *)
    printf '%s\\n' '[]'
    ;;
esac
EOF
chmod 755 "$TMP/bin/systemctl"

cat >"$TMP/bin/git" <<EOF
#!/bin/sh
printf '%s\\n' "\$*" >>"$TMP/git.log"
case " \$* " in
  *" push "*)
    echo "git push is forbidden in admission proof" >&2
    exit 1
    ;;
  *" worktree add "*)
    echo "git worktree add is forbidden in admission proof" >&2
    exit 1
    ;;
  *)
    exit 0
    ;;
esac
EOF
chmod 755 "$TMP/bin/git"

NEW_COMMIT_SPEC=$(jq -nc --arg rev "$COMMIT_REV" --arg cap "$NEW_CAP" '{
  version: 1,
  originating_work: "omp-tytn.3",
  artifact_revision: $rev,
  capability: $cap
}')
[[ ! -e $NEW_HOME/automation.toml ]] || fail "new capability automation.toml existed before admit"
[[ ! -e $NEW_UNIT ]] || fail "new capability unit existed before admit"

applied=$(admit_apply "$NEW_COMMIT_SPEC")
ok "$applied"
[[ -f $NEW_UNIT ]] || fail "new capability did not create $NEW_STEM"
[[ -f $NEW_HOME/automation.toml ]] || fail "new capability did not write automation.toml"
[[ -f $NEW_DESIRED ]] || fail "new capability did not write the first desired input"
[[ -f $NEW_TIMER ]] || fail "new capability did not write a timer"
[[ -e $TMP/enabled/$NEW_STEM.timer ]] || fail "new capability timer was not enabled"
grep -q '^agent = "capability-maintainer"$' "$NEW_HOME/automation.toml" \
  || fail "new capability agent: $(cat "$NEW_HOME/automation.toml")"
grep -q '^parent = "managed-omp-runtime-integrator"$' "$NEW_HOME/automation.toml" \
  || fail "new capability parent: $(cat "$NEW_HOME/automation.toml")"
jq -e --arg rev "$COMMIT_REV" '
  .version == 1 and
  (.inputs | length) == 1 and
  .inputs[0].originating_work == "omp-tytn.3" and
  .inputs[0].artifact_kind == "commit" and
  .inputs[0].artifact_revision == $rev
' "$NEW_DESIRED" >/dev/null || fail "new capability desired-inputs: $(cat "$NEW_DESIRED")"

collide=$(admit "$(jq -nc --arg rev "$COMMIT_REV" --arg cap "$COLLIDE_CAP" '{
  version: 1,
  originating_work: "omp-tytn.4",
  artifact_revision: $rev,
  capability: $cap
}')" || true)
rejected "$collide"
[[ ! -e $SCOPE/.systemd-ops/operations/$COLLIDE_STEM/automation.toml ]] \
  || fail "colliding capability wrote automation.toml"
[[ ! -e $UNIT_DIR/$COLLIDE_STEM.service ]] \
  || fail "colliding capability created a unit"

DELTA_NEW_SPEC=$(jq -nc --arg cap "$DELTA_CAP" --arg sha "$DELTA_SHA" '{
  version: 1,
  originating_work: "omp-tytn.5",
  capability: $cap,
  artifact_kind: "worktree-delta",
  base: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  delta: [{path: "src/foo.rs", sha256: $sha}]
}')
before_delta_files=$(find "$SCOPE" "$UNIT_DIR" -type f | sort)
applied=$(admit_apply "$DELTA_NEW_SPEC")
ok "$applied"
[[ -f $DELTA_HOME/automation.toml ]] || fail "worktree-delta new capability did not write automation.toml"
[[ -f $DELTA_DESIRED ]] || fail "worktree-delta new capability did not write desired-inputs"
copied=$(find "$SCOPE" "$UNIT_DIR" -type f -exec grep -l "$CANARY_BYTES" {} + 2>/dev/null || true)
[[ -z $copied ]] || fail "worktree-delta new capability copied classified bytes: $copied"
if [[ -f $TMP/git.log ]]; then
  grep -E '(^| )push( |$)' "$TMP/git.log" \
    && fail "fork/cap/$DELTA_CAP was pushed: $(cat "$TMP/git.log")"
  grep -F 'worktree add' "$TMP/git.log" \
    && fail "git worktree add from classified tree: $(cat "$TMP/git.log")"
fi
# Expected new files only: delta unit, timer, automation.toml, desired-inputs.
after_delta_files=$(find "$SCOPE" "$UNIT_DIR" -type f | sort)
added=$(comm -13 <(printf '%s\n' "$before_delta_files") <(printf '%s\n' "$after_delta_files"))
printf '%s\n' "$added" | grep -qx "$UNIT_DIR/$DELTA_STEM.service" \
  || fail "worktree-delta new capability did not add service: $added"
printf '%s\n' "$added" | grep -qx "$UNIT_DIR/$DELTA_STEM.timer" \
  || fail "worktree-delta new capability did not add timer: $added"
printf '%s\n' "$added" | grep -qx "$DELTA_HOME/automation.toml" \
  || fail "worktree-delta new capability did not add automation.toml: $added"
printf '%s\n' "$added" | grep -qx "$DELTA_DESIRED" \
  || fail "worktree-delta new capability did not add desired-inputs: $added"
extra=$(printf '%s\n' "$added" | grep -vx "$UNIT_DIR/$DELTA_STEM.service" \
  | grep -vx "$UNIT_DIR/$DELTA_STEM.timer" \
  | grep -vx "$DELTA_HOME/automation.toml" \
  | grep -vx "$DELTA_DESIRED" || true)
[[ -z $extra ]] || fail "worktree-delta new capability wrote extra files: $extra"

if [[ -f $TMP/git.log ]]; then
  grep -E '(^| )push( |$)' "$TMP/git.log" \
    && fail "fork/cap/$NEW_CAP was pushed: $(cat "$TMP/git.log")"
fi
[[ ! -e /home/sf/worktrees/omp/admit/retry-fallback-unowned ]] \
  || fail "classified admit home retry-fallback-unowned exists"
[[ ! -e /home/sf/worktrees/omp/admit/todo-passive-phase ]] \
  || fail "classified admit home todo-passive-phase exists"

echo "admission new-capability ok"
echo "admission ok"
