# Wanted-but-Inactive Timer Attention Technical Spec

**Design Brief:** `/home/sf/worlds/personal/designs/systemd-ops/wanted-inactive-timer-attention.md`
(Status in the Brief: `ready-for-handoff`. Bead: `sdo-bn2`.)
**Status:** sound
**Repo:** `/home/sf/workspace/systemd-ops`

## Intent (from the Brief - do not rewrite)

Quoted from the Brief, not paraphrased.

Problem statement:

> A stopped-but-enabled scheduled oneshot is currently health-invisible.

> `managed-omp-project-lead-watch` and `managed-omp-fork-main-sync` were
> `UnitFileState=enabled`, `ActiveState=inactive`, `next=null`,
> `last_result=success`. [...] `scope show` reported `health=healthy` and
> `attention=[]`.

> Ata wants a bounded detector/guard so an enabled/wanted managed scope timer
> going inactive cannot stay dark 12-24h. The project-lead watch is the
> important reflex: it is what wakes a stalled lead.

Success criteria (binding, verbatim):

1. > An owned scheduled oneshot that is enabled, not lifecycle-completed, and
   > has no future trigger appears on `scope show` attention (and therefore the
   > TUI) with a stable code such as `timer_unscheduled` or `wanted_inactive`.
2. > A lifecycle-completed unit (`state/lifecycle.json` status=completed and
   > timer disabled), such as `managed-omp-pr-10922` and `managed-omp-pr-12104`
   > after merge, does not appear.
3. > A healthy enabled timer with a future trigger does not appear.
4. > A never-run timer stays `unknown` / `never-run` as today unless the planner
   > finds a reason to change that.
5. > Scope health may stay `healthy` if only attention is required. Degrading
   > the scope is allowed only if the spec says so explicitly. Silent `healthy`
   > + empty attention is the defect.
6. > Tests pin the 0914 shape: enabled, last success, `next=null`, lifecycle
   > active, not critical -> attention nonempty.
7. > Docs (`docs/SCOPES.md`) name the new attention code at the same weight as
   > `operation_failed` / `not-scheduled`.
8. > No new crate. No MCP flag unless a new field is actually added to the view.
   > No change to `apply_verb` / plan-apply mutation path.

Constraints (verbatim):

> Advisory fields (briefs, iterations) stay health-neutral.

> Operation homes do not own systemd truth. Enablement and next-elapse come
> from systemd. Lifecycle-completed is the only home-side exception already in
> the model (`state/lifecycle.json`).

> Do not make every `unknown` a scope failure. Completed and disabled units
> must stay quiet.

> Do not auto-start, enable, or retire anything from the detector. Surface only.

Explicitly not this work:

> inventing a new probe/heartbeat/incident plane; failing a unit whose last
> result succeeded; auto-starting timers; changing retire/complete (those
> disable the timer on purpose).

The Brief recommends approach A:

> A. New attention code on the existing ScopeView path: enabled + timer + no
> future trigger + lifecycle not completed -> `attention[]` entry. Health stays
> `unknown` / `not-scheduled` unless the spec promotes it.

This spec implements A.

## Mapping onto the current system

Claims below are labelled **observed** (read in the source at the cited line),
**inferred** (derived from observed code), or **unknown**.

### The hole is one unreached match arm

**Observed** - `src/scope.rs:455-484`, inside `aggregate`, is the only place an
owned operation can produce an `Attention` entry. The arms are
`"failed" if critical`, `"failed"`, `"unknown" if critical`, then `_ => {}`.

**Observed** - `operation_health` (`src/operations.rs:1272-1320`) returns
`"unknown"` for a timer oneshot whose last result is `success` when
`enabled && has_future_trigger(view)` is false:

```1295:1308:src/operations.rs
        Some("oneshot") | Some("oneshot-linger") => {
            if never {
                "unknown"
            } else if last_result.and_then(Value::as_str) != Some("success") || state == "failed" {
                "failed"
            } else if is_timer_operation(view) {
                if enabled && has_future_trigger(view) {
                    "healthy"
                } else {
                    "unknown"
                }
            } else {
                "healthy"
            }
        }
```

**Inferred (the 0914 shape, exactly)** - an owned, non-critical, enabled timer
oneshot with `last_result=success` and `next=null` yields `op_health="unknown"`
and `critical=false`, so the match falls through `_ => {}`: no attention entry,
no health raise. `ScopeHealth` starts at `Healthy` (`src/scope.rs:402`) and is
never raised. That reproduces the Brief's live proof - `health=healthy`,
`attention=[]` - from source alone. This is the defect and the whole fix site.

### Completed units are already quiet, for free

**Observed** - `src/scope.rs:414-423`: lifecycle completion short-circuits the
operation health used by the match:

```414:423:src/scope.rs
        let completed = automation
            .get("lifecycle")
            .and_then(|lifecycle| lifecycle.get("status"))
            .and_then(Value::as_str)
            == Some("completed");
        let op_health = if completed {
            "healthy"
        } else {
            operations::operation_health(&view)
        };
```

**Observed** - `src/automation.rs:473-493` (`load_lifecycle`) returns `Ok(None)`
when `state/lifecycle.json` is absent and **errors** unless `status` is
`completed` with a non-empty reason; `lifecycle_json`
(`src/automation.rs:1101-1110`) defaults a missing file to
`{"status":"active"}`. **Inferred:** "lifecycle completed" is exactly "the file
exists", and criterion 2 (completed + disabled stays quiet) is satisfied by the
existing short-circuit as long as the new detector rides the `op_health` match
rather than bypassing it. No new lifecycle plumbing is required.

### The predicate inputs all exist already

**Observed** - the operation view carries every field the predicate needs, and
the private helpers already encode the semantics:

- `"enablement"` is stamped into the view at `src/operations.rs:1542` from the
  timer unit's `UnitFileState` (string, e.g. `enabled` / `disabled` / `static`;
  `src/operations.rs:1383`).
- `enablement_is_on` (`src/operations.rs:1209-1214`) accepts `enabled`,
  `enabled-runtime`, `static`.
- `has_future_trigger` (`src/operations.rs:1216-1222`) is false for missing,
  `null`, empty, and `"n/a"` next-elapse.
- `is_timer_operation` (`src/operations.rs:1224-1226`) is
  `activation == "timer"`.

All three are private to `operations.rs`. **Inferred:** the detector belongs
next to them in `operations.rs`, exposed as one `pub fn`, so systemd truth stays
in one module (Brief constraint: "Operation homes do not own systemd truth").

### Surfaces that consume attention[]

**Observed** - there are exactly two consumers, and neither needs a per-code
change to display a new code:

| Surface | Path | Behaviour with a new code |
|---|---|---|
| CLI `scope show` | `src/bin/systemd_ops.rs:294` then `ScopeView::to_json` (`src/scope.rs:88-109`), printed at `src/bin/systemd_ops.rs:243-258` | Prints `operation relationship code reason` generically. A new code appears with no code-specific work. |
| TUI | `src/tui.rs:293` and `src/tui.rs:1334` consume `ScopeView` directly | Uses `attention` only for the header count `N attention` (`src/tui.rs:1541-1549`). There is no attention list pane. |

**Observed** - `src/mcp.rs` has no scope tool at all (`rg scope_show src/mcp.rs`
is empty; the only `scope` occurrences are capability enums such as
`Scope::UnitsRead`), and `src/varlink.rs` never touches `attention`.
**Inferred:** criterion 8's "No MCP flag" is satisfied trivially, because this
spec adds no field to any view - only a new value of the existing
`attention[].code` string.

**Observed** - the TUI "when" column comes from `when_label`
(`src/tui.rs:2250-2283`). For the 0914 shape it falls through every branch
(`lifecycle != completed`; `health != failed`; not running; `activation` is
`timer` not `direct` and `kind` is `oneshot` not `simple`; `next` empty;
`health_basis` is `not-scheduled`, not `never-run`) and returns `String::new()`.
**Inferred:** today a dark wanted timer renders a blank "when" cell in the TUI;
only the header count would move. The Brief's optional TUI copy is therefore
worth a bounded label (see Architecture).

### Contradiction check: Brief vs reality

No product-shaped gap was found. Every binding criterion is reachable on the
existing ScopeView path without a product change:

| Criterion | Reality | Verdict |
|---|---|---|
| 1 attention entry for enabled + not-completed + no future trigger | reachable at the `src/scope.rs:455` match | map |
| 2 completed + disabled stays quiet | already guaranteed by `src/scope.rs:414-423` | already satisfied |
| 3 healthy enabled timer with a future trigger stays quiet | `op_health == "healthy"` falls to `_ => {}` | already satisfied |
| 4 never-run stays `unknown` / `never-run` | `operation_health` and `health_basis` untouched by this spec | preserved |
| 5 scope health may stay `healthy` | this spec changes no health value | preserved |
| 6 tests pin the 0914 shape | unit tests exist for both seams | map |
| 7 `docs/SCOPES.md` names the code | prose sections at `docs/SCOPES.md:206`, `:227`, `:265` | map |
| 8 no new crate / MCP flag / `apply_verb` change | no new dependency; no new view field; `apply_verb` untouched | preserved |

The Brief's three "open questions for the planner" are all resolved **by the
Brief's own text**, not by planner taste, so none of them is escalated:

1. *Should a critical reflex unit raise scope health?* Criterion 5 settles the
   default: "Scope health may stay `healthy` if only attention is required.
   Degrading the scope is allowed only if the spec says so explicitly." This
   spec does **not** say so: it changes no health value anywhere. The existing
   `"unknown" if critical` raise to `ScopeHealth::Unknown` is preserved
   byte-for-byte, so a critical dark timer behaves on health exactly as it does
   today.
2. *Should a deliberate compose-gate hold appear?* Criterion 1's predicate
   (enabled, lifecycle not completed, no future trigger) matches a held-down
   integrator, and the Brief states "the detector should still be able to see
   the hold". So it appears. The Brief's other half - "do not blindly start a
   gated integrator" - is honoured because this is surface-only: no start, no
   enable, no retire, no health change.
3. *Code name?* The Brief supplies the decision rule: "Prefer the one that will
   not be confused with `health_basis=not-scheduled` on a completed/disabled
   unit." `timer_unscheduled` is a near-synonym of `not-scheduled` and would
   collide in exactly that way. **Chosen: `wanted_inactive`.** It names the
   wanted-vs-actual mismatch (enabled but not scheduled) rather than the
   schedule state, and it cannot be misread as the existing `health_basis`.

## Architecture

Three thin layers, each in the module that already owns that kind of truth. No
new module, no new crate, no new view field, no change to any health value.

```
systemd truth        src/operations.rs
                     pub fn enablement_on(&str) -> bool    (extracted; one source of truth)
                     pub fn is_wanted_inactive_timer(&Value) -> bool
                       = kind oneshot|oneshot-linger
                       & activation == "timer"
                       & enablement is on
                       & !has_future_trigger
                       & !missing
                              |
scope + lifecycle    src/scope.rs::aggregate
                     owned loop, existing match op_health:
                       "unknown" if critical -> health raise UNCHANGED,
                                                code = wanted_inactive when the
                                                predicate holds, else operation_unknown
                       "unknown" if wanted   -> attention only, health untouched  <-- the fix
                       completed             -> op_health forced "healthy" -> `_` -> quiet
                              |
presentation         CLI: generic printer, no change
                     TUI: when_label gains "not scheduled" for
                          health_basis == "not-scheduled" && enablement on
```

Deliberate design points:

- **The detector is a predicate over systemd fields only.** It does not read
  `state/lifecycle.json`; `aggregate` already owns lifecycle. This keeps the
  Brief constraint "Operation homes do not own systemd truth" intact and makes
  the predicate unit-testable from a literal JSON view.
- **The detector rides the existing `op_health` match instead of preceding it.**
  Failure precedence is preserved for free: a timer whose last result failed is
  already `"failed"` and keeps `operation_failed`. Nothing "fails a unit whose
  last result succeeded" (explicit non-goal).
- **One stable code across criticality.** Criterion 1 asks for "a stable code".
  A critical dark timer today reports the generic `operation_unknown`; after
  this change both critical and non-critical dark timers report
  `wanted_inactive`, while the health effect of criticality is unchanged. This
  is the only pre-existing operator-visible string this spec alters, and it
  makes the condition greppable in one place.
- **`last_result` is not part of the predicate.** Criterion 1 states the
  predicate as "enabled, not lifecycle-completed, and has no future trigger" -
  no result condition. So a never-run enabled timer with `next=null` (a
  provisioned-but-never-started reflex, just as dark as the 0914 units) also
  appears in attention. Criterion 4 is still honoured exactly: it constrains
  *health*, and `operation_health` / `health_basis` are untouched, so such a
  unit keeps `unknown` / `never-run` and keeps reading "never ran" in the TUI.

## Components and interfaces

### src/operations.rs

```rust
/// True for a unit-file state systemd considers "wanted at boot".
pub fn enablement_on(state: &str) -> bool;

/// True when systemd says this operation is a wanted timer oneshot with no
/// future trigger: enabled, timer-activated, and next-elapse absent.
/// Lifecycle completion is NOT consulted here; the scope layer owns it.
pub fn is_wanted_inactive_timer(view: &Value) -> bool;
```

`enablement_is_on(&Value)` stays private and delegates to `enablement_on`, so
the accepted set (`enabled`, `enabled-runtime`, `static`) has one definition.
`operation_health` and `health_basis` keep their current signatures and their
current return values for every input.

### src/scope.rs

`aggregate`'s owned loop gains one local and two code selections. `Attention`
(`src/scope.rs:66-72`) is unchanged: `code` is `&'static str`, `reason` is
`String`.

| Case | code | reason | Scope health effect |
|---|---|---|---|
| owned, `op_health=="unknown"`, predicate holds, not critical | `wanted_inactive` | `enabled, no future trigger` | none (stays as-is) |
| owned, `op_health=="unknown"`, predicate holds, critical | `wanted_inactive` | `enabled, no future trigger` | `raise(Unknown)`, as today |
| owned, `op_health=="unknown"`, predicate false, critical | `operation_unknown` | `unknown` | `raise(Unknown)`, as today |
| owned, `op_health=="unknown"`, predicate false, not critical | none | none | none, as today |
| lifecycle completed | none | none | none; `op_health` is forced `healthy` |

The `!completed &&` guard is kept explicit in the local
(`let wanted_inactive = !completed && operations::is_wanted_inactive_timer(&view);`)
even though `completed` already forces `op_health` to `"healthy"`. Criterion 2
is a Brief invariant; stating it locally means a future edit to the
short-circuit cannot silently un-satisfy it.

### src/tui.rs

- `Row` (`src/tui.rs:126-176`) gains `enablement: String`, populated from the
  existing view field at the `from_view` mapping (`src/tui.rs:779`). This is a
  new field on an internal TUI struct, **not** a new field on any emitted view,
  so criterion 8's MCP-flag clause is not triggered.
- `when_label` (`src/tui.rs:2250-2283`) gains one branch, after the `never-run`
  branch: `health_basis == "not-scheduled"` **and** `enablement_on(...)` yields
  `"not scheduled"`.

The enablement guard is load-bearing: `health_basis == "not-scheduled"` is also
true for a *disabled* successful timer (pinned by the existing test
`successful_scheduled_timer_disabled_is_unknown`,
`src/operations.rs:2388-2400`). Without the guard, a deliberately disabled timer
would gain a new label the Brief never asked for. With it, the label appears
only for the wanted-inactive condition.

### docs/SCOPES.md

Prose only, three existing sections, at the same weight as `operation_failed` /
`not-scheduled`: the Health section (`:206`), the ScopeView section (`:227`),
and the Aggregation table plus its "Attention is not a health value" paragraph
(`:265-277`).

## Data / control flow

Read path, unchanged except where marked:

1. CLI `scope show` (`src/bin/systemd_ops.rs:294`) or the TUI worker
   (`src/tui.rs:1334`) calls `scope::show_resolved`.
2. `show_manifest` (`src/scope.rs:368`) lists owned operations, appends
   `missing_view` for absent critical stems, and resolves watch entries.
3. `aggregate` (`src/scope.rs:397`) walks owned operations. Per unit:
   `critical` from the manifest; `automation` and `completed` from
   `automation::join_operation`; `op_health` is `"healthy"` if completed, else
   `operation_health`.
   **New:** `wanted_inactive` = `!completed && is_wanted_inactive_timer(&view)`.
4. The `match op_health` emits attention. **New:** the two `"unknown"` paths
   above. Health arithmetic (`ScopeHealth::raise`, `src/scope.rs:57-63`) is
   untouched.
5. Watching entries (`src/scope.rs:489-521`) are **unchanged**: they still react
   to `"failed"` only. Criterion 1 scopes the detector to "An owned scheduled
   oneshot".
6. `ScopeView::to_json` serialises `attention[]` as today; the CLI prints it
   generically; the TUI counts it in the header and renders the per-row
   `when_label`.

For the 0914 shape the resulting flow is: `op_health="unknown"`,
`critical=false`, `completed=false`, predicate true, so one `Attention` with
`operation: "managed-omp-project-lead-watch"`, `relationship: "owned"`,
`code: "wanted_inactive"`, `reason: "enabled, no future trigger"`, while
`ScopeView.health` is still `healthy`, `owned[i]["health"]` is still
`"unknown"`, and `health_basis` is still `"not-scheduled"`.

## Error handling

No new fallible path. Specifically:

- The predicate is total over any `Value`: every field access is
  `get(...).and_then(...)` with a false-y default, so a malformed or partial
  view yields `false` (quiet), never a panic. A `missing: true` view
  (`missing_view`, `src/scope.rs:564-581`) is excluded explicitly and keeps
  today's `operation_unknown` for critical stems.
- Lifecycle read failures are already handled upstream: `join_operation`
  returns `(Value::Null, Some(warning))` on a malformed `state/lifecycle.json`,
  the warning lands in `ScopeView.warnings`, and `completed` evaluates false.
  **Consequence to accept:** a unit with an unreadable lifecycle file that is
  otherwise dark-and-wanted will appear in attention alongside its warning.
  That is the correct bias - an unreadable home is not evidence of completion.
- No new I/O, no new subprocess, no new timeout surface. The TUI backend's 8s
  `systemctl` bound (`docs/SCOPES.md:288-294`) is unaffected.
- No mutation: the detector never starts, enables, or retires. `apply_verb` and
  the plan/apply path are not touched.

## Testing (behavioral contracts; exact tests live in the plan)

Contracts, each pinned by a unit test in the module that owns the behaviour.

Predicate (`src/operations.rs` tests):

1. The 0914 shape (`oneshot`, `activation=timer`, `enablement=enabled`,
   `next=null`, `last_result=success`) satisfies the predicate, **and** in the
   same test `operation_health` is still `"unknown"` and `health_basis` is still
   `"not-scheduled"`. The health assertions are the anti-regression half: they
   make it impossible to "fix" the defect by changing a health value.
2. Enabled with a future trigger: predicate false.
3. Disabled with `next=null`: predicate false.
4. `kind=simple` enabled and inactive: predicate false (already `failed`).
5. `activation=direct` oneshot: predicate false.
6. A never-run enabled timer with `next=null`: predicate true, while
   `health_basis` is still `"never-run"` (criterion 4 preserved).
7. `missing: true`: predicate false.
8. `enablement_on` accepts `enabled`, `enabled-runtime`, `static` and rejects
   `disabled`, `masked`, and the empty string.

Aggregation (`src/scope.rs` tests):

9. **Criterion 6, the required pin.** Owned, non-critical, 0914 shape, no
   lifecycle file: `attention` has exactly one entry with
   `code == "wanted_inactive"`, `relationship == "owned"`, the right operation,
   **and** `view.health == ScopeHealth::Healthy` (attention without a health
   change) **and** `owned[0]["health"] == "unknown"`.
10. Owned **critical** 0914 shape: `code == "wanted_inactive"` and
    `view.health == ScopeHealth::Unknown` (today's criticality effect,
    preserved).
11. **Criterion 3.** Owned enabled timer with a future trigger and last success:
    `attention` empty, health `Healthy`.
12. **Criterion 2.** Owned timer that is disabled, `next=null`, last success,
    with a real `state/lifecycle.json` (`status=completed`, non-empty reason)
    written under `<root>/.systemd-ops/operations/<stem>/state/`: `attention`
    empty and health `Healthy`. This must use a real temp scope root, because
    "completed" is defined by that file existing (`src/automation.rs:473-493`);
    a hand-set JSON field would test nothing.
13. The pre-existing `unknown_critical_is_unknown` (never-run, no enablement
    field) still yields `operation_unknown` and `reason "unknown"`, proving the
    generic path survives.

TUI (`src/tui.rs` tests):

14. A row with `health_basis == "not-scheduled"` and `enablement == "enabled"`
    renders `when_label == "not scheduled"`; flipping `enablement` to
    `disabled` returns to today's empty string.

Anti-gameable posture: contracts 9-12 are assertions on `aggregate`'s real
output built from a manifest and, for 12, from a real on-disk lifecycle file,
not on a helper the implementer can redefine. Contracts 1 and 6 assert that
health and `health_basis` did **not** move, so the defect cannot be closed by
relabelling health. The existing suite (`cargo test --lib scope::`,
`cargo test --lib operations::`) must stay green; the shell suites in `tests/`
are live-systemd integration and are out of scope here.

## Non-goals

From the Brief, restated so the plan cannot drift into them:

- No new probe, heartbeat, incident plane, watchdog process, or journal
  scraping. No HCOM from core.
- No auto-start, auto-enable, or auto-retire. Surface only.
- No change to retire/complete, to `apply_verb`, or to the plan/apply mutation
  path. No new crate. No compose/publish or live OMP driver changes.
- No change to any health value: not `operation_health`, not `health_basis`, not
  `ScopeHealth` aggregation. Criterion 5's permission to degrade is explicitly
  declined.
- Watching entries are not extended to the new code (criterion 1 says "owned").
- Advisory operator/brief/iteration fields stay health-neutral and are
  untouched.
- Not folded into crash-park (`sdo-s7r`) or notification provenance
  (`sdo-notify-provenance-r4j`).
- **No new TUI attention pane.** The TUI has no attention list today
  (`attention` feeds only the header count, `src/tui.rs:1541-1549`). The Brief
  puts "optional TUI copy for the new attention code" in scope and a second
  watchdog or new surface out of scope, so this spec delivers the bounded row
  label plus the existing count, and leaves a dedicated pane unbuilt.

## Implementation approach chosen (and rejected internals)

All three approaches below satisfy every Brief invariant; they differ only on
internals. Chosen: **1**.

**1. Predicate in `operations.rs`, decision in `aggregate`'s existing
`op_health` match (chosen).** Systemd truth stays in `operations.rs`, lifecycle
stays in `scope.rs`, failure precedence is inherited from the match order, and
the diff is one `pub fn` plus two arms. Matches the Brief's recommended
approach A and the repo's existing split of concerns.

**2. Compute the condition inside `aggregate` from raw view fields.** Rejected:
it would duplicate `enablement_is_on` and `has_future_trigger` semantics (the
accepted enablement set is a real detail - `static` counts) in a second module,
and `scope.rs` would start interpreting systemd strings, which `operations.rs`
owns.

**3. Add the flag to the operation view (e.g. `view["wanted_inactive"]`) in
`operation_view`, and have `aggregate` and the TUI read it.** Rejected on two
grounds: it adds a field to an emitted view, which under criterion 8 drags in
the MCP-flag question for no benefit; and it puts a scope-relative judgement
into a per-operation read used by `inspect list-operations` and
`inspect get-operation`, which have no scope context.

Rejected product-shaped alternatives, from the Brief itself: **B** (treat
enabled + no-next as `failed`) would degrade scope health for every
quiesced-but-enabled timer including a deliberate compose-gate hold; the Brief
rejects it and criterion 5's default forbids it. **C** (a per-scope timer-watch
unit that polls and HCOMs) adds a process, duplicates lead-watch, and still
needs this core signal.

Internal choices recorded for the reviewer:

- Code name `wanted_inactive`, per the Brief's own disambiguation rule (see
  Mapping, resolved question 3).
- `reason` string `enabled, no future trigger`, matching the terse style of the
  existing `"failed"` and `"unknown"` reasons; the CLI prints it verbatim.
- One code for the condition across criticality, so a critical dark timer
  reports `wanted_inactive` instead of the generic `operation_unknown`; the
  health effect of criticality is unchanged.
- `enablement_on` extracted as `pub` so the TUI guard and the predicate cannot
  drift on which unit-file states count as wanted.
- `!completed` restated locally in `aggregate` rather than relying solely on the
  `op_health` short-circuit.

## Open questions

None. No product question is outstanding: the Brief's three planner questions
are each resolved by binding Brief text (criterion 5 for health, criterion 1
plus the "see the hold" sentence for the compose-gate case, and the stated
naming rule for the code), and no Brief invariant needed a product change to
map onto this repo.

Non-product leftovers, recorded as risks rather than questions:

- A dedicated TUI attention pane remains unbuilt (see Non-goals). If Ata later
  wants the TUI to *list* attention entries rather than count them and label the
  row, that is a new UI slice for the lead to route.
- After this lands, any enabled timer that is deliberately held down with
  lifecycle still active becomes permanently visible in attention. That is the
  Brief's intent ("the detector should still be able to see the hold"), but it
  means a long compose-gate hold shows a standing attention entry with no
  acknowledgement lifecycle (`docs/SCOPES.md:275-276`: "No acknowledgement
  lifecycle").
