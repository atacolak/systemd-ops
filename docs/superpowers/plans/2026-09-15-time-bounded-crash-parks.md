# Time-Bounded Crash Parks and Park Release Implementation Plan

**Technical Spec:** `docs/superpowers/specs/2026-09-15-time-bounded-crash-parks.md`
**Design Brief:** none on file — approved policy is the operator ruling of
2026-09-15 relayed in the lead's packet (proposal record: bead
`sdo-caprun-sticky-park-a8i`), via the spec; do not bypass.

> **For the project lead:** first `br where` in this repo. no board → `br init --prefix <xx>` here (never `~/.beads`). then one campaign parent bead for this plan, then task-by-task with isolated `builder` workers. Do not implement these tasks inline. Persist progress in this file's checkboxes **and** as one parent-child bead per Task (`br create --parent --body`). Title `tN: <ask>` (prefix `[done] ` after verifier pass). Description markdown, ≤800 characters, wrap at ~60 cols: `Asked:` paragraph, then `## Landed` with sha/tests/leftover (`not yet` while in flight). Do not paste this plan packet. After pass, keep the child `in_progress` (mgr Rolling), assignee cleared — do not defer, do not `br close` until the parent parks (closed = Past the Stand). Lined Up is only for minted-not-yet-claimed children.

**Goal:** Crash parks expire after a cooldown (one attempt then re-park), and
every park kind gains a documented operator release verb,
`automation release-park --unit STEM`.
**Architecture:** The cooldown lives entirely in the skip predicate
`input_matches_operational_park` in `dogfood/lib/automation-wrapper` (bash);
all six skip sites in the three drivers inherit it unchanged. The release path
is a new rust CLI verb implemented in `src/operator.rs` beside `clear`,
dispatched from the `automation` group in `src/bin/systemd_ops.rs`, gated by
`require_owned`, audited via the operator activity surface.
**Tech stack:** bash + jq for the dogfood wrapper (existing); rust with
serde_json only (no new crates) for the verb; shell proofs under `tests/`.

---

## Decisions

Both delegated decisions were approved by midi, who owns park semantics, on
2026-09-15 (hcom 69155). The planner's recommendation and its reasoning are
kept below, together with the rejected option, so the value carries its
provenance. Neither value may be changed by a builder.

1. **Cooldown value: 1800 s (30 min), env `PARK_CRASH_COOLDOWN_SECONDS`.
   APPROVED as recommended.**
   Reasoning: an attempt costs one model pass, and against an outage that pass
   fails fast (not at the 60 m `--max-time`), so a probe per 30 min is cheap.
   The observed outage window was ~51 min (05:00:59–05:51:30, bead
   `sdo-caprun-sticky-park-a8i`); its cause is `omp-actor-bridge-ayn.64`
   (@default roles do not migrate off a dead primary — see the spec's
   incident-cause note). 30 min rides out short outages with at most
   one or two mid-outage probes and recovers within 30 min of restoration. It
   sits under the existing 60 m release-grace scale. Rejected 3600 s: doubles
   recovery latency to save one cheap probe per hour. Env-overridable so tests
   and operators can force either branch without editing code.
2. **Verb name: `automation release-park --unit STEM`. APPROVED as
   recommended.** The park sidecar is
   automation runtime state; the `automation` group already holds the sibling
   (`automation clear-blocker`) and stem-addressed mutations (`automation
   complete --unit STEM`). Rejected: `operator release-park` (operator.json is
   the advisory surface, not the park's home); a bound no-stem form (no caller
   — drivers never self-release; the cooldown covers crash).
3. **Layer per part:** cooldown → `dogfood/lib/automation-wrapper` (bash), no
   driver logic changes (comment blocks only); release verb → rust
   (`src/operator.rs` + dispatch/USAGE in `src/bin/systemd_ops.rs`); proofs →
   `tests/failure-budget.sh`, `tests/capability-release-run.sh`, new
   `tests/park-release.sh`; docs → `CHANGELOG.md`, `README.md`,
   `docs/TESTING.md`. No MCP tool, no man page change (AGENTS.md: the MCP man
   page is obligatory only for a new MCP flag or capability scope).

**Standing constraint (packet acceptance):** no task touches an existing live
sidecar, and no task clears the five currently parked live units. Tests plant
their own fixture sidecars under `mktemp` directories only.

**Deployment is held, and it is not the lead's call.** The capability
`brain_paths` include `.systemd-ops/lib/automation-wrapper`, and `brain_revision`
is computed from the live deployed files under the scope root, so syncing this
change to the live copy changes every parked unit's observation fingerprint and
releases its skip, handing all five parked units an attempt. That is the same
operator-visible outcome the operator vetoed when he declined hand-clearing for
this mechanism, so midi declined to rule on it and escalated it to the operator
(hcom 69155). The repository change stays inert until synced. Do not sync the
live copy, and do not treat silence as consent.

---

## File structure

| File | Change | Responsibility |
|---|---|---|
| `dogfood/lib/automation-wrapper` | Modify `:529` area (new constant), replace `:556-562` (predicate) | Cooldown rule in the single skip predicate |
| `dogfood/drivers/capability-run` | Modify comment block `:90-93` | Truthful skip comment (expiry sentence) |
| `dogfood/drivers/pr-run` | Modify comment block `:37-40` | Same |
| `dogfood/drivers/runtime-run` | Modify comment block `:228-231` | Same |
| `tests/failure-budget.sh` | Modify: new cases before final `echo` | Lib-level regression + guard cases |
| `tests/capability-release-run.sh` | Modify: new case after `:524` | Driver-level expiry/re-park proof |
| `src/operator.rs` | Modify: new `release_park` + `park_owns_blocker` beside `clear` (`:1109-1134`); unit tests in `mod tests` | Release verb implementation |
| `src/bin/systemd_ops.rs` | Modify: USAGE (`:76` area), dispatch arm in `run_automation` (after `:842-845`), usage test (`:1039-1047`) | CLI surface |
| `tests/park-release.sh` | Create | CLI end-to-end proof of the verb |
| `docs/TESTING.md` | Modify: fast-gates list | Register the new proof |
| `CHANGELOG.md` | Modify: `## 0.7.0 (not published)` | Changelog obligation |
| `README.md` | Modify: automation paragraph (`:137-153`) | Operator-facing semantics |

---

### Task 1: Crash-park cooldown in the wrapper predicate

**Owner:** builder
**Files:**
- Modify: `dogfood/lib/automation-wrapper:529` (add constant), `:556-562` (replace predicate)
- Modify: `dogfood/drivers/capability-run:90-93`, `dogfood/drivers/pr-run:37-40`, `dogfood/drivers/runtime-run:228-231` (comment blocks only)
- Test: `tests/failure-budget.sh`, `tests/capability-release-run.sh`

**Verification (anti-gameable):** `bash tests/failure-budget.sh` and
`bash tests/capability-release-run.sh` pass with the new cases; on the
pre-change code both FAIL (shown in Step 2). Guards
`bash tests/pr-attempt.sh` and `bash tests/wrapper-contract.sh` stay green —
they pin timeout-park and semantic-blocked behaviour this task must not move.

- [x] **Step 1: Write the failing tests**

Append to `tests/failure-budget.sh` immediately before the final
`echo "failure-budget ok"` line:

```bash
# --- crash parks are time-bounded; every other kind stays terminal ---
# An expired crash park no longer binds the input: the tick runs one attempt.
record_operational_park fp-crash crash
( PARK_CRASH_COOLDOWN_SECONDS=0; input_matches_operational_park fp-crash ) \
  && fail "expired crash park still skipped the input"
# A fresh crash park still binds inside the cooldown.
( PARK_CRASH_COOLDOWN_SECONDS=999999; input_matches_operational_park fp-crash ) \
  || fail "fresh crash park did not skip the input"
# A crash park with no readable parked_at fails open, like a corrupt record,
# and the wrapper says so on stderr.
jq -nc '{version:1,input_fingerprint:"fp-nodate",last_kind:"crash"}' \
  >"$STATE_DIR/operational-park.json"
nodate_err=$( ( PARK_CRASH_COOLDOWN_SECONDS=999999; input_matches_operational_park fp-nodate ) 2>&1 >/dev/null || true )
[[ $nodate_err == *"no readable parked_at"* ]] \
  || fail "crash park without parked_at was not reported: $nodate_err"
( PARK_CRASH_COOLDOWN_SECONDS=999999; input_matches_operational_park fp-nodate ) \
  && fail "crash park without parked_at skipped the input"
# timeout, contract-failure and dirty-at-exit stay fingerprint-terminal even
# with a zero cooldown.
for terminal_kind in timeout contract-failure dirty-at-exit; do
  record_operational_park fp-term "$terminal_kind"
  ( PARK_CRASH_COOLDOWN_SECONDS=0; input_matches_operational_park fp-term ) \
    || fail "$terminal_kind park expired; only crash parks are time-bounded"
done
# Re-park after expiry: the budget already stands at FAILURE_BUDGET, so one
# failed attempt re-parks, overwrites the sidecar with a fresh parked_at, and
# the budget keeps climbing.
rm -f "$STATE_DIR/failure-budget.json"
record_operational_park fp-crash crash
record_attempt_failure fp-crash crash >/dev/null
record_attempt_failure fp-crash crash >/dev/null
first_at=$(jq -er '.parked_at' "$STATE_DIR/operational-park.json")
sleep 1
operational_failure_maybe_park fp-crash crash 7 \
  || fail "third identical crash did not re-park"
[[ $(jq -er '.parked_at' "$STATE_DIR/operational-park.json") != "$first_at" ]] \
  || fail "re-park did not refresh parked_at"
jq -e '.failures == 3' "$STATE_DIR/failure-budget.json" >/dev/null \
  || fail "re-park did not keep the budget climbing"
```

In `tests/capability-release-run.sh`, insert immediately after the line
`  || fail "the park skip self-reported READY"` (currently `:525`):

```bash
# A crash park is time-bounded: with the cooldown elapsed the same unchanged
# input gets exactly one fresh attempt, and the budget — still standing at
# FAILURE_BUDGET — re-parks on failure with a fresh parked_at.
code=$(PARK_CRASH_COOLDOWN_SECONDS=0 run_cap_code 1000 "$target" "$TMP/cap-park-expired.out")
unset PARK_CRASH_COOLDOWN_SECONDS
[[ $code -eq 0 ]] || fail "expired crash park exited $code, want the re-park: $(cat "$TMP/cap-park-expired.out")"
[[ $(grep -c called "$SCOPE/agent-calls" || true) -eq 3 ]] \
  || fail "expired crash park did not run exactly one pass"
jq -e '.failures == 3 and .input_fingerprint == "fp-cap"' "$CAP_STATE/failure-budget.json" >/dev/null \
  || fail "re-park did not keep the budget climbing"
jq -e '.input_fingerprint == "fp-cap"' "$CAP_STATE/operational-park.json" >/dev/null \
  || fail "re-park did not restore the park sidecar"
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `bash tests/failure-budget.sh`
Expected: FAIL with `expired crash park still skipped the input`

Run: `bash tests/capability-release-run.sh`
Expected: FAIL with `expired crash park did not run exactly one pass`
(the tick skips, so `agent-calls` stays at 2)

- [x] **Step 3: Implement the cooldown**

In `dogfood/lib/automation-wrapper`, immediately after
`FAILURE_BUDGET=${FAILURE_BUDGET:-2}` (`:529`) add:

```bash
# Seconds a crash park binds its input. A crash is a property of the
# environment, not of the input, so the park stops binding this long after
# parked_at and the tick runs one attempt; the unchanged budget re-parks on
# failure with a fresh parked_at. Crash only: every other park kind stays
# fingerprint-terminal until the input moves or an operator releases the park.
PARK_CRASH_COOLDOWN_SECONDS=${PARK_CRASH_COOLDOWN_SECONDS:-1800}
```

Replace `input_matches_operational_park` (`:556-562`) with:

```bash
# True when this observation is the input an operational park already covered
# and the park still binds it. A different fingerprint never matches, so a
# later input runs normally. Only crash parks expire: PARK_CRASH_COOLDOWN_SECONDS
# after parked_at the park stops binding and the tick runs one attempt. The
# budget still stands at FAILURE_BUDGET, so a failed attempt re-parks with a
# fresh parked_at. timeout, contract-failure and dirty-at-exit stay
# fingerprint-terminal until the input moves or an operator releases the park.
input_matches_operational_park() {
  local input=$1
  local parked
  parked=$(operational_park_fingerprint) || return 1
  [[ -n "$parked" && "$parked" == "$input" ]] || return 1
  # The fingerprint matched, so the sidecar parsed. Only crash parks expire.
  local f kind parked_at parked_epoch now_epoch
  f=$(operational_park_file)
  kind=$(jq -r '.last_kind // empty' "$f" 2>/dev/null)
  [[ $kind == crash ]] || return 0
  parked_at=$(jq -r '.parked_at // empty' "$f" 2>/dev/null)
  if ! parked_epoch=$(date -u -d "$parked_at" +%s 2>/dev/null); then
    echo "crash park at $f has no readable parked_at; treating the park as expired" >&2
    return 1
  fi
  now_epoch=$(date -u +%s)
  (( now_epoch - parked_epoch < PARK_CRASH_COOLDOWN_SECONDS ))
}
```

In each of `dogfood/drivers/capability-run` (`:90-93`),
`dogfood/drivers/runtime-run` (`:228-231`), and `dogfood/drivers/pr-run`
(`:37-40`), extend the identical skip comment block so the second sentence
reads (keeping each driver's own final word "pass"/"attempt"):

```bash
# An operational park bounds the retry instead of certifying the input: while
# the observation still matches a park that binds it, this tick does no work.
# A crash park stops binding PARK_CRASH_COOLDOWN_SECONDS after parked_at; every
# other kind is fingerprint-terminal until the input moves or an operator
# releases the park. The skip reports nothing, writes no processed state and
# clears no blocker or budget; it only declines to burn a pass.
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `bash tests/failure-budget.sh`
Expected: PASS (`failure-budget ok`)

Run: `bash tests/capability-release-run.sh`
Expected: PASS

Run (guards): `bash tests/pr-attempt.sh && bash tests/wrapper-contract.sh`
Expected: PASS, PASS

- [x] **Step 5: Commit**

```bash
git add dogfood/lib/automation-wrapper dogfood/drivers/capability-run dogfood/drivers/pr-run dogfood/drivers/runtime-run tests/failure-budget.sh tests/capability-release-run.sh
git commit -m "automation: time-bound crash parks behind PARK_CRASH_COOLDOWN_SECONDS"
```

---

### Task 2: `automation release-park --unit STEM` verb

**Owner:** builder
**Files:**
- Modify: `src/operator.rs` (new `release_park` + `park_owns_blocker` after `clear`, `:1134`; unit tests in `mod tests`)
- Modify: `src/bin/systemd_ops.rs:76` (USAGE), `run_automation` after `:845` (dispatch), `:1039-1047` (usage test)
- Test: `cargo test` (unit + usage); end-to-end proof is Task 3

**Verification (anti-gameable):** `cargo test` passes including the new
`park_owns_blocker` cases and the USAGE assertion; on pre-change code
`cargo test` FAILS to compile (`cannot find function`) / the USAGE assertion
fails. `cargo clippy --all-targets --locked -- -D warnings` and
`cargo fmt --check` stay clean.

- [x] **Step 1: Write the failing tests**

In `src/operator.rs`'s `mod tests`, add:

```rust
    #[test]
    fn park_owns_blocker_matching_rules() {
        let blocker = crate::automation::Blocker {
            version: 2,
            id: "b1".into(),
            input_fingerprint: Some("fp-1".into()),
            kind: "iteration-failed".into(),
            route: "parent".into(),
            at: "2026-09-15T05:51:30Z".into(),
            iteration_id: None,
            code: Some(0),
            summary: "parked after 2 identical crash failures on unchanged input".into(),
        };
        assert!(park_owns_blocker(&blocker, Some("fp-1")));
        assert!(!park_owns_blocker(&blocker, Some("fp-other")));
        assert!(!park_owns_blocker(&blocker, None));
        let semantic = crate::automation::Blocker {
            kind: "semantic-blocked".into(),
            ..blocker
        };
        assert!(!park_owns_blocker(&semantic, Some("fp-1")));
    }
```

(If `crate::automation::Blocker` gains no `..blocker` update syntax because a
field is private, construct the second value explicitly with
`kind: "semantic-blocked"` and the same remaining fields.)

In `src/bin/systemd_ops.rs`'s `usage_lists_scope_root_and_iterations` test
(`:1039-1047`), add:

```rust
        assert!(USAGE.contains("automation release-park --unit STEM"));
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test`
Expected: FAIL — `cannot find function 'park_owns_blocker' in this scope`
(compile error), or, if the implementation order put the USAGE line first,
`assertion failed: USAGE.contains("automation release-park --unit STEM")`

- [x] **Step 3: Implement the verb**

> **SUPERSEDED by commit 098d91d.** The code block below is the original
> prescription and it is WRONG as written: it removes the park sidecar first and
> then runs steps that can still refuse or fail, which leaves a non-retryable
> partial mutation. A verification constructed the windows: a malformed blocker,
> a refused blocker symlink or a failed blocker unlink after the park is gone
> leaves a blocker no retry can clear, because the no-park path short-circuits;
> and an over-long activity text or an operator-surface error releases the unit
> while reporting failure with no audit. The implementation therefore validates
> everything first and removes the park LAST: manifest, ownership, sidecar read,
> park symlink check, blocker load with its ownership and symlink checks,
> activity-text bound, operator-surface load, then the owned blocker, then the
> park, then the activity. Read
> `src/operator.rs::release_park` at 098d91d rather than this block. One window
> remains and is known: if the audit write in `finish_write` fails after both
> unlinks, the park and its blocker are gone with no activity entry, and a retry
> takes the idempotent no-park path, so that audit is never written. It is
> recorded here because it is a limit of the verb, not an oversight.

In `src/operator.rs`, immediately after `clear` (ends `:1134`), add:

```rust
/// True only for the park's own blocker record: an iteration-failed blocker
/// bound to the parked fingerprint. A semantic-blocked or any other blocker is
/// not the park's record and survives a release.
fn park_owns_blocker(
    blocker: &crate::automation::Blocker,
    park_fingerprint: Option<&str>,
) -> bool {
    blocker.kind == "iteration-failed"
        && park_fingerprint.is_some()
        && blocker.input_fingerprint.as_deref() == park_fingerprint
}

/// Release an operational park: remove the sidecar and the park's own blocker,
/// and record the release in operator activity. The failure budget is
/// deliberately kept: it already stands at the budget, so the next failure
/// re-parks. Processed state, checkpoints, units, and worktrees are untouched.
pub fn release_park(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    stem: &str,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    require_owned(&manifest, stem)?;
    let home = crate::automation::operation_home_checked(&manifest.root, stem)?;
    let park_path = home.join("state").join("operational-park.json");
    refuse_symlink(&park_path)?;
    if !park_path.exists() {
        return Ok(json!({
            "unit": stem,
            "released": false,
            "blocker_cleared": false,
        }));
    }
    let record = fs::read(&park_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let field = |key: &str| -> Option<String> {
        record
            .as_ref()
            .and_then(|r| r.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let fingerprint = field("input_fingerprint");
    let kind = field("last_kind");
    let parked_at = field("parked_at");
    fs::remove_file(&park_path)
        .map_err(|e| BackendError(format!("remove {}: {e}", park_path.display())))?;
    let mut blocker_cleared = false;
    if let Some(blocker) = crate::automation::load_blocker(&manifest.root, stem)? {
        if park_owns_blocker(&blocker, fingerprint.as_deref()) {
            let path = crate::automation::blocker_path(&manifest.root, stem);
            refuse_symlink(&path)?;
            fs::remove_file(&path)
                .map_err(|e| BackendError(format!("remove {}: {e}", path.display())))?;
            blocker_cleared = true;
        }
    }
    let text = bound(
        "activity.text",
        &format!(
            "released operational park (kind={}, fingerprint={}, parked_at={})",
            kind.as_deref().unwrap_or("unknown"),
            fingerprint.as_deref().unwrap_or("unknown"),
            parked_at.as_deref().unwrap_or("unknown"),
        ),
        MAX_ACTIVITY_TEXT,
    )?;
    let (load, warning) = load_with_warning(&manifest.root, stem);
    let mut surface = match load {
        OperatorLoad::Ready(s) => s,
        OperatorLoad::Missing => empty_surface(),
        OperatorLoad::Error(msg) => {
            return Err(BackendError(format!(
                "operator state for '{stem}' is malformed: {msg}"
            )))
        }
    };
    surface.activity.push(OperatorActivity {
        at: now_stamp(),
        text,
    });
    if surface.activity.len() > MAX_ACTIVITY {
        let drop = surface.activity.len() - MAX_ACTIVITY;
        surface.activity.drain(0..drop);
    }
    finish_write(&manifest.root, stem, &surface)?;
    Ok(json!({
        "unit": stem,
        "released": true,
        "park": {
            "input_fingerprint": fingerprint,
            "last_kind": kind,
            "parked_at": parked_at,
        },
        "blocker_cleared": blocker_cleared,
        "warning": warning,
    }))
}
```

In `src/bin/systemd_ops.rs` USAGE, after the `  automation clear-blocker` line
(`:76`) add:

```
  automation release-park --unit STEM
```

In `run_automation`, after the `"clear-blocker"` arm (`:842-845`) add:

```rust
        "release-park" => {
            let unit = require_opt(args, "--unit").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            operator::release_park(scope_root, cwd, &unit)
        }
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS, including `park_owns_blocker_matching_rules` and
`usage_lists_scope_root_and_iterations`

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo fmt --check`
Expected: clean

- [x] **Step 5: Commit**

```bash
git add src/operator.rs src/bin/systemd_ops.rs
git commit -m "automation: add release-park, the operator release path for operational parks"
```

---

### Task 3: CLI proof `tests/park-release.sh`

**Owner:** builder
**Files:**
- Create: `tests/park-release.sh`
- Modify: `docs/TESTING.md` (fast-gates list, after `bash tests/operator-report.sh`)

**Verification (anti-gameable):** `cargo build && bash tests/park-release.sh`
passes against the just-built `target/debug/systemd-ops`; on the pre-Task-2
binary it FAILS with `unknown automation command 'release-park'`. A verifier
can re-run the script against any build.

- [x] **Step 1: Write the failing test**

Create `tests/park-release.sh` (modeled on `tests/operator-report.sh`):

```bash
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
out=$(json automation release-park --unit "$STEM")
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
out=$(json automation release-park --unit "$STEM")
ok "$out"
jq -e '.data.released == false' <<<"$out" >/dev/null || fail "no-park release: $out"

# A semantic blocker is not the park's record and survives the release.
plant_park
jq -nc '{version:2,id:"b2",input_fingerprint:"fp-1",kind:"semantic-blocked",route:"lead",at:"2026-09-15T06:00:00Z",summary:"real semantic block"}' \
  >"$STATE/blocker.json"
out=$(json automation release-park --unit "$STEM")
ok "$out"
jq -e '.data.released == true and .data.blocker_cleared == false' <<<"$out" >/dev/null \
  || fail "semantic blocker release reply: $out"
jq -e '.kind == "semantic-blocked"' "$STATE/blocker.json" >/dev/null \
  || fail "release cleared a semantic blocker"

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
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo build && bash tests/park-release.sh`
Expected: FAIL — `expected rejection`/`expected ok` mismatch because the reply
is `unknown automation command 'release-park'` (when run before Task 2 lands;
after Task 2 this is the pass proof)

- [x] **Step 3: Register the proof**

In `docs/TESTING.md`, add `bash tests/park-release.sh` to the fast-gates list
immediately after the `bash tests/operator-report.sh` line.

- [x] **Step 4: Run the test to verify it passes**

Run: `cargo build && bash tests/park-release.sh`
Expected: PASS (`park-release ok`)

Run (guards): `bash tests/operator-report.sh && bash tests/automation-cli.sh`
Expected: PASS, PASS

- [x] **Step 5: Commit**

```bash
git add tests/park-release.sh docs/TESTING.md
git commit -m "test(automation): prove release-park clears the park, keeps the budget, audits"
```

---

### Task 4: Changelog and README

**Owner:** builder
**Files:**
- Modify: `CHANGELOG.md` (`## 0.7.0 (not published)` section)
- Modify: `README.md:137-153` (automation paragraph)

**Verification (anti-gameable):**
`grep -q "release-park" CHANGELOG.md && grep -q "PARK_CRASH_COOLDOWN_SECONDS" CHANGELOG.md && grep -q "release-park" README.md && bash tests/docs.sh`
all pass. `tests/docs.sh` proves the man page obligation is genuinely
unaffected (no MCP surface changed).

- [x] **Step 1: CHANGELOG entry**

In `CHANGELOG.md`, under `## 0.7.0 (not published)`, add a new paragraph:

```markdown
Crash parks are time-bounded. `input_matches_operational_park` releases a
crash park `PARK_CRASH_COOLDOWN_SECONDS` (default 1800) after the sidecar's
`parked_at`, so a model outage no longer pins a unit on an unchanged input.
The failure budget already stands at the budget when a park exists, so the
retried input gets one attempt and re-parks with a fresh `parked_at` on
failure. `timeout`, `contract-failure`, and `dirty-at-exit` parks stay
fingerprint-terminal. `automation release-park --unit STEM` is the documented
operator release path for every park kind: it removes the
`operational-park.json` sidecar and the park's own `iteration-failed`
blocker, records the release in operator activity, and deliberately keeps
`failure-budget.json`, processed state, and checkpoints. It is gated by scope
ownership like every operation-state write and mutates no systemd unit.
```

- [x] **Step 2: README automation paragraph**

In `README.md`'s automation paragraph (`:137-153`), after the sentence about
`automation process` writing `state/processed.json`, add:

```markdown
An operational park (`state/operational-park.json`) bounds retries of an
input whose identical operational failures exhausted the budget. A `crash`
park stops binding `PARK_CRASH_COOLDOWN_SECONDS` (default 1800) after
`parked_at`; `timeout`, `contract-failure`, and `dirty-at-exit` parks are
fingerprint-terminal. `automation release-park --unit STEM` releases any
park: it removes the sidecar and the park's own blocker, keeps the failure
budget (one attempt, then re-park on failure), and records the release in
operator activity.
```

- [x] **Step 3: Verify**

Run: `grep -q "release-park" CHANGELOG.md && grep -q "PARK_CRASH_COOLDOWN_SECONDS" CHANGELOG.md && grep -q "release-park" README.md && bash tests/docs.sh`
Expected: all pass; `tests/docs.sh` prints `PASS: the man page is clean and indexable`

- [x] **Step 4: Commit**

```bash
git add CHANGELOG.md README.md
git commit -m "docs: crash-park cooldown and release-park in changelog and README"
```

---

## Self-review notes

- Spec coverage: cooldown rule (T1), release verb + guard + audit + exit codes
  (T2, T3), docs surfaces (T2 USAGE, T3 TESTING.md, T4 CHANGELOG/README).
  Man page explicitly excluded per AGENTS.md (no MCP surface).
- No task touches a live sidecar or the five parked live units; all fixtures
  are `mktemp`-scoped.
- Type consistency: `park_owns_blocker`, `release_park`, USAGE string, and the
  proof's reply assertions use the same names and shapes across T2/T3.
- Every task is one builder packet with a runnable verification command.
- Beyond-plan additions, recorded here so nothing lands unrecorded. (a) The
  parked_at guard in the wrapper checks `[[ -z $parked_at ]]` explicitly as well
  as the date parse, because GNU date accepts an empty `-d` operand as midnight
  of the current day, so a crash park with no parked_at would otherwise be timed
  from today's midnight and its stderr note would never print. (b) Task 2 added
  seven release_park contract tests beyond the one the plan sketched, since a
  destructive verb needs its refusals pinned. (c) release_park validates before
  it mutates, which the Step 3 note above records in full.
