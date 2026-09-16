# Time-Bounded Crash Parks and Park Release Technical Spec

**Design Brief:** no file-based Personal Design Brief exists for this work.
The binding product authority is the operator ruling of 2026-09-15 relayed in
the lead's planning packet; the proposal record is bead
`sdo-caprun-sticky-park-a8i` (hera comment, 2026-09-15, items 1–2: crash parks
become time-bounded; a documented release path exists for every park kind;
non-crash kinds keep current behaviour; the budget interaction is
one-attempt-then-re-park). The packet explicitly delegates the cooldown value,
the verb name, and the layer per part to the planner.
**Status:** sound
**Repo:** `/home/sf/workspace/systemd-ops`

## Intent (from the approved policy — do not rewrite)

- A park whose kind is `crash` is **time-bounded**: it stops binding its input
  after a cooldown, because an unavailable model is a property of the
  environment, not of the input.
- Every park kind gets a **documented release path** (a CLI verb), because a
  fingerprint-terminal park on a static input otherwise has no exit.
- The failure budget already stands at `FAILURE_BUDGET` when a park exists, so
  after a cooldown expiry or a manual release the expected shape is **one
  attempt, then re-park on failure** — not a budget reset.
- A park whose kind is NOT `crash` keeps its current behaviour
  (fingerprint-terminal).

## Mechanism verification (packet claims vs. code)

Every claim in the packet was checked against the source. Verdicts:

| # | Packet claim | Verdict | Evidence |
|---|---|---|---|
| 1 | The skip compares only the fingerprint | CONFIRMED | `input_matches_operational_park` (`dogfood/lib/automation-wrapper:557-562`) calls `operational_park_fingerprint` (`:542-554`), which reads only `.input_fingerprint`. `last_kind` and `parked_at` are written but never read anywhere. |
| 2 | The sidecar already stores `parked_at` and `last_kind`, so a cooldown needs no new state | CONFIRMED | `record_operational_park` (`dogfood/lib/automation-wrapper:595-603`) writes `{version:1,input_fingerprint,last_kind,parked_at}` with `parked_at` from `date -u +%Y-%m-%dT%H:%M:%SZ` (RFC 3339 UTC, GNU-date parseable). |
| 3 | The budget already stands at FAILURE_BUDGET when a park exists | CONFIRMED | `operational_failure_maybe_park` (`dogfood/lib/automation-wrapper:623-635`) parks only after `record_attempt_failure` (`:575-586`) reaches `n >= FAILURE_BUDGET` (`:529`, default 2). Nothing decrements; only `clear_failure_budget` (`:588-591`) removes the file, on success or a semantic blocked outcome. pr-run uses the same file with `PR_FAILURE_BUDGET` (`dogfood/lib/pr-attempt:6-7`, default 2). |
| 4 | One attempt then re-park is the expected shape | CONFIRMED as consequence | After the skip releases an input, the next failure increments the budget to 3 (>= 2), so `park_identical_input` (`:612-618`) fires immediately and `record_operational_park` overwrites the sidecar with a fresh `parked_at`. On success, `clear_failure_budget` removes both sidecars. |
| 5 | No release path exists in the mechanism | CORRECTED (imprecise) | Two *internal* paths remove the sidecar: `clear_failure_budget` (`dogfood/lib/automation-wrapper:588-591`) and `recover_worktree` (`:353`, together with `automation clear-blocker` at `:349`). Neither is an operator release path. `automation clear-blocker` (`src/automation.rs:841-857`) clears only the blocker and leaves the park binding. `tests/pr-attempt.sh:243` hand-deletes the sidecar with `rm -f` — the missing verb in fixture form. Correct statement: no **operator-invokable** release path exists. |

Live-state context (not repo, recorded for provenance only): five units sit
parked with `kind=crash`, budget 2/2, from the 2026-09-15 model-outage window
(bead `sdo-caprun-sticky-park-a8i`). This spec and its plan do **not** clear
them; releasing live parks is an operator act using the new verb, not a build
task.

Incident cause (lead-verified, cited not re-derived): the outage behind those
parks is tracked in the oh-my-pi ledger as `omp-actor-bridge-ayn.64` (P1,
open): "@default roles do not migrate off a dead primary; chain is
post-failure only + cooldown-expiry revert". Mechanism, verified by the lead
in the live runtime generation:

- `packages/coding-agent/src/session/retry-fallback-chains.ts:454`
  `findRetryFallbackCandidates` resolves the current selector to `exactIndex`
  and returns `chain.slice(exactIndex + 1)` — the walk is forward only unless
  `wrapAround` is explicitly passed.
- `retry-fallback-chains.ts:238` `getRetryFallbackRevertPolicy` returns
  `"never"` only when `retry.fallbackRevertPolicy` is exactly that; otherwise
  `"cooldown-expiry"`.
- `session/turn-recovery.ts:1674-1680` consults the chain inside the failure
  path, after a `retry.modelFallback` check, so the primary is always
  attempted first.

That is why the parks are a record of the environment: two identical crash
failures, no input change, and no release. The runtime-side fix is
`omp-actor-bridge-ayn.64`'s question and is out of scope here; this spec
covers only the park policy.

## Park site and kind enumeration

All parks write the same sidecar (`<operation-home>/state/operational-park.json`,
operation home preferred at `<scope-root>/.systemd-ops/operations/<stem>`,
`src/automation.rs:188-196`) via `record_operational_park`. All skips go through
the single predicate `input_matches_operational_park`. One predicate change
covers every site; no driver logic changes.

| Driver | Skip sites | Park sites and kinds |
|---|---|---|
| `dogfood/drivers/capability-run` | `:94` (pre-lock), `:119` (in-loop) | `:233` via `operational_failure_maybe_park` — `crash`, or `timeout` when exit is 124 (`:229-231`); `:239` — `crash` on iteration-finish failure; `:247` — `contract-failure` (exit 0 without `automation_report`) |
| `dogfood/drivers/runtime-run` | `:232` (pre-lock), `:258` (in-loop) | `:311` — `crash`/`timeout` (`:307-310`); `:318` — `crash` on finish failure; `:326` — `contract-failure` |
| `dogfood/drivers/pr-run` | `:41` (pre-lock), `:58` (post-lock) | `handle_hard_failure` (`:116-129`, park at `:123-127`) for `crash`/`timeout` (`:131-136`), finish `crash` (`:139`), `contract-failure` (`:144`); `dirty-at-exit` park at `:156-160`. Parks via `park_pr_input` (`dogfood/lib/pr-attempt:99-106`) |

Policy per kind:

| Kind | Where produced | Policy | Reason |
|---|---|---|---|
| `crash` | all three drivers (nonzero omp exit; finish failure) | **Time-bounded** | A crash is a property of the environment (model outage, lost backend), not of the input — the packet ruling. |
| `timeout` | all three drivers (exit 124) | Fingerprint-terminal | The packet time-bounds `crash` only. |
| `contract-failure` | all three drivers (missing/invalid report) | Fingerprint-terminal | Agent violated the reporting contract on this input; an input/agent property. |
| `dirty-at-exit` | pr-run only | Fingerprint-terminal | The attempt left uncommitted state; an input/agent property. |

Fingerprint-terminal kinds are not stuck: the release verb (below) is their
documented exit, and a moved input fingerprint already bypasses any park
(`dogfood/lib/automation-wrapper:561`).

## Architecture

- **Cooldown: bash, in `dogfood/lib/automation-wrapper` only.** The skip
  predicate already lives there; all six skip sites inherit the change
  unchanged. Rust never reads the sidecar for skipping. No new state files:
  `parked_at` + `last_kind` suffice.
- **Release: rust CLI, new verb `automation release-park --unit STEM`.** First
  rust reader of the sidecar format. Stem-addressed (like `automation complete
  --unit STEM`, `src/bin/systemd_ops.rs:758-763`) because the caller is an
  operator/lead releasing *another* unit's park; no driver ever self-releases
  (the cooldown covers crash), so no bound no-stem form (YAGNI).

## Components and interfaces

### 1. `PARK_CRASH_COOLDOWN_SECONDS` (dogfood/lib/automation-wrapper)

New env-tunable constant beside `FAILURE_BUDGET` (`:529`):
`PARK_CRASH_COOLDOWN_SECONDS=${PARK_CRASH_COOLDOWN_SECONDS:-1800}`.
Default 1800 s (30 min) — see the plan's open-decision rationale.

### 2. `input_matches_operational_park` — new exact predicate

Returns 0 (skip this tick) iff ALL hold, in order:

1. The sidecar is readable and parses (existing corrupt-record rule:
   unreadable → report on stderr, return "no park", never skip —
   `dogfood/lib/automation-wrapper:546-552`).
2. `.input_fingerprint` equals the observed fingerprint (existing).
3. `.last_kind == "crash"` AND `.parked_at` parses via `date -u -d` AND
   `now - parked_at < PARK_CRASH_COOLDOWN_SECONDS`.

Any fingerprint match whose `last_kind` is not `crash` returns 0 (skip) exactly
as today — non-crash behaviour is byte-identical. A `crash` park with a missing
or unparseable `parked_at` fails **open** (no skip, stderr note), consistent
with the corrupt-record rule; the re-park on the next failure writes a fresh
`parked_at` and heals it. The expiry path does **not** delete or rewrite the
sidecar: it is overwritten by the re-park or removed by `clear_failure_budget`
on success. The predicate name and signature are unchanged; the six call sites
and the three drivers' logic are untouched (their skip comment blocks gain the
expiry sentence).

### 3. `automation release-park --unit STEM` (rust)

- **Namespace/arguments:** `automation` group, required `--unit STEM`. No other
  flags.
- **Guard:** `require_owned` (`src/operator.rs:181-194`) — the scope-owned gate
  every operation-state write already passes (`src/operator.rs:526,683,898,971,
  1014,1054,1085,1115`; `src/automation.rs:1011,1654,1693,1776,1820`).
  **Write-prefix is deliberately NOT consulted.** `require_write_unit`
  (`src/systemd.rs:129-141`) gates systemd unit mutations only
  (`src/write.rs:228,335`; `src/operations.rs:300,1726-1728,1786,1894`;
  `src/systemd.rs:854`), and release mutates no unit. Requiring it would refuse
  release on an owned operation outside the prefix — and with no prefix
  configured would refuse entirely (`src/config.rs:3`) — while `operator clear`
  on the same stem succeeds. That is the incoherence the inspect-reads brief
  already rejected for reads.
- **Clears:** the `operational-park.json` sidecar (after `refuse_symlink`,
  `src/automation.rs:374`), and the current blocker **only when it is the
  park's own record**: `kind == "iteration-failed"` AND
  `blocker.input_fingerprint == park.input_fingerprint` (Blocker fields,
  `src/automation.rs:96-108`; `load_blocker`, `:650`). Precedent for clearing
  blocker + sidecar together: `recover_worktree`
  (`dogfood/lib/automation-wrapper:349-353`). A `semantic-blocked` or any other
  blocker stays.
- **Deliberately does NOT:** touch `failure-budget.json` (the budget stays at
  `FAILURE_BUDGET`, so the next failure re-parks — the packet's expected shape,
  and release is not a budget reset); touch `processed.json`, `checkpoint.json`,
  `observation.json`, or `operator.json` iterations; restart or otherwise
  mutate a systemd unit; touch any worktree; run on a schedule (manual only);
  clear the five currently parked live units (operator's call, post-release).
- **Audit trail:** an entry on the operator activity surface via the same
  load/push/truncate/write sequence as `operator append`
  (`src/operator.rs:1003-1045`; `MAX_ACTIVITY = 100`, `:27`; text bounded by
  `MAX_ACTIVITY_TEXT`, `:1009`): `released operational park (kind=…,
  fingerprint=…, parked_at=…)`. The retained `failure-budget.json` (`failures`,
  `last_kind`, `last_at` — `dogfood/lib/automation-wrapper:583-584`) is the
  durable record of why the unit parked. The JSON reply carries the released
  record. No new audit file — the repo has no audit log and none is requested.
- **Exit codes:** the CLI has exactly two (`src/bin/systemd_ops.rs:123-135`).
  0 with `ok:true` on success — including the idempotent no-park case, which
  returns `released:false` (mirrors `clear_blocker`'s `cleared:false`,
  `src/automation.rs:849-852`). 1 with `ok:false`: missing `--unit`
  (`invalid_argument` / "missing --unit"), non-owned stem ("operator writes are
  restricted to owned stems…", `src/operator.rs:189-192`), malformed operator
  surface. Driver exit codes 2/3/4 are driver-level and unaffected.
- **Reply shape:** `{unit, released, park:{input_fingerprint,last_kind,
  parked_at}, blocker_cleared, warning}` (`park` fields null when the sidecar
  was unparseable; the file is still removed — release is the cleanup path).

### 4. Documentation surfaces

- CLI help: `USAGE` in `src/bin/systemd_ops.rs:17-87` gains
  `automation release-park --unit STEM`; the usage test at `:1039-1047` gains
  the assertion.
- `CHANGELOG.md` under `## 0.7.0 (not published)`: the cooldown and the verb
  (the changelog describes what changed on the wire or on disk).
- `README.md` automation paragraph (`:137-153`): the park sidecar, the crash
  cooldown, and the release verb.
- `docs/TESTING.md` fast-gates list: the new `tests/park-release.sh`.
- **Man page: unchanged.** AGENTS.md obliges the `systemd-ops-mcp.1` man page
  only for a new MCP flag or capability scope; this is a direct CLI verb with
  no MCP tool, and direct CLI surface is documented in CLI help and scope docs,
  "not … the MCP man page as if the MCP binary accepted them". `tests/docs.sh`
  is unaffected.

## Data / control flow

Cooldown expiry (no release needed):

```
timer tick -> observe -> input_is_processed? no
  -> input_matches_operational_park:
       fingerprint matches, last_kind=crash, age >= cooldown -> NO skip
  -> one pass runs -> failure: record_attempt_failure n=3 >= 2
       -> park_identical_input -> sidecar overwritten, fresh parked_at
       -> exit 0
     success: mark_processed + clear_failure_budget (both sidecars gone)
```

Early non-park exits (e.g. capability-run's PR gates, `:146-158`) leave the old
sidecar in place; later ticks re-evaluate cheaply until a pass actually runs.

Release:

```
operator: systemd-ops --json automation release-park --unit managed-omp-cap-x
  -> require_owned -> remove operational-park.json
  -> blocker is park's own? remove blocker.json : leave it
  -> append operator activity (audit)
  -> next tick: no park binds -> one attempt -> re-park on failure
     (budget still at FAILURE_BUDGET) or clear on success
```

## Error handling

- Corrupt/unreadable sidecar on the skip path: existing fail-open with wrapper
  stderr report (`dogfood/lib/automation-wrapper:546-552`), pinned by
  `tests/failure-budget.sh:160-171` and `tests/capability-release-run.sh:554-563`.
- Crash park with missing/unparseable `parked_at`: fail-open with stderr note
  (new, same rule).
- Clock skew (`parked_at` in the future): negative age < cooldown → skip
  (fail-safe, no retry storm).
- Release on missing sidecar: `released:false`, exit 0. Release with malformed
  sidecar: file removed, `park` fields null. Release with malformed
  `operator.json`: error, exit 1 (same as `operator append`,
  `src/operator.rs:1019-1024`). Symlink sidecar/blocker: refused
  (`refuse_symlink`).
- Parked PR child feeding the release gate: an unprocessed child is unsettled
  by contract, so `capability_release_gate` holds the launch to +3600 s
  (`PR_RELEASE_GRACE_SECONDS`) instead of the ~300 s quiet window; accepted,
  and recorded in the plan's self-review notes.

## Testing (behavioral contracts; exact tests live in the plan)

Regression tests — each FAILS on current code:

1. `bash tests/failure-budget.sh` — new case: crash park +
   `PARK_CRASH_COOLDOWN_SECONDS=0` → `input_matches_operational_park` does not
   skip. Today it skips → FAIL.
2. `bash tests/failure-budget.sh` — new case: crash park with no `parked_at` →
   no skip + stderr note. Today it skips → FAIL.
3. `bash tests/capability-release-run.sh` — new driver-level case: expired
   crash park runs exactly one pass and re-parks (budget 2→3, fresh
   `parked_at`). Today the tick skips → FAIL.
4. `cargo test` — new `park_owns_blocker` unit tests and the USAGE assertion.
   Today: compile error / assertion failure → FAIL.
5. `bash tests/park-release.sh` (new) — release clears sidecar + own blocker,
   keeps `failure-budget.json`, writes audit activity, is idempotent, refuses
   unowned stems, preserves a semantic blocker. Today: "unknown automation
   command 'release-park'" → FAIL.

Contract guards — must stay green (pin the behaviour the policy keeps):

- `bash tests/failure-budget.sh` — fresh crash park still skips; `timeout`,
  `contract-failure`, `dirty-at-exit` parks stay terminal even at zero
  cooldown; re-park refreshes `parked_at`; all pre-existing cases.
- `bash tests/pr-attempt.sh` — timeout park skip pins (`:254-269`) unchanged.
- `bash tests/wrapper-contract.sh` — semantic blocked repeat behaviour.
- `bash tests/operator-report.sh`, `bash tests/automation-cli.sh` — CLI
  envelope and guard conventions.
- `cargo clippy --all-targets --locked -- -D warnings`, `cargo fmt --check`,
  `bash tests/docs.sh`.

## Non-goals

- Clearing or touching the five currently parked live units, or any existing
  sidecar. The verb is the tool; using it is an operator act.
- Time-bounding `timeout`, `contract-failure`, or `dirty-at-exit` parks.
- Budget reset on release or on expiry.
- Any MCP tool, any man page change, any new audit log file.
- Changes to processed/checkpoint/semantic-state semantics, driver control
  flow, or the six skip call sites.
- Restarting units or touching worktrees on release.
- The sibling open bead `sdo-pr-park-retry-semantics-9on` (PR park vs.
  processed semantics) — separate question, not amended here.

## Implementation approach chosen (and rejected internals)

- **Cooldown inside `input_matches_operational_park`** (one seam, six call
  sites unchanged). Rejected: a new skip function plus six driver edits (churn,
  no behaviour gain); rust-side cooldown (rust never sees ticks — wrong layer).
- **`automation release-park --unit STEM`, implemented in `src/operator.rs`
  beside `clear` (`:1109-1134`)** because it reuses `resolved_manifest`
  (`:500`), `require_owned`, and the activity-append machinery; dispatched from
  the `automation` group because the state it mutates is automation state
  (sibling: `automation clear-blocker`). Rejected: `operator` namespace
  (operator.json is the advisory surface; the park is not operator surface
  state); bound no-stem form (no caller — drivers never self-release);
  bash-only release helper (the packet requires a documented CLI verb with CLI
  help and changelog obligations).
- **Guard: `require_owned` only.** Rejected: additionally requiring
  write-prefix — see Components §3; it would make release unusable exactly
  where sibling operation-state writes are allowed.
- **Cooldown default 1800 s**, env-overridable. Rejected 3600 s: doubles
  recovery latency to save one cheap probe per hour (a failed attempt against
  an outage fails fast, not at the 60 m `--max-time`).

## Open questions

None affecting product. Planner-recommended internals delegated by the packet
(cooldown value, verb name, layer per part) are decided above and restated at
the top of the plan for the lead to veto.
