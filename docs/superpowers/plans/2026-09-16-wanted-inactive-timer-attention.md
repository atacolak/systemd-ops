# Wanted-but-Inactive Timer Attention Implementation Plan

**Technical Spec:** `docs/superpowers/specs/2026-09-16-wanted-inactive-timer-attention.md`
**Design Brief:** `/home/sf/worlds/personal/designs/systemd-ops/wanted-inactive-timer-attention.md` (via spec; do not bypass)

> **For the project lead:** first `br where` in this repo. no board -> `br init --prefix <xx>` here (never `~/.beads`). then one campaign parent bead for this plan, then task-by-task with isolated `builder` workers. Do not implement these tasks inline. Persist progress in this file's checkboxes **and** as one parent-child bead per Task (`br create --parent --body`). Title `tN: <ask>` (prefix `[done] ` after verifier pass). Description markdown, <=800 characters, wrap at ~60 cols: `Asked:` paragraph, then `## Landed` with sha/tests/leftover (`not yet` while in flight). Do not paste this plan packet. After pass, keep the child `in_progress` (mgr Rolling), assignee cleared - do not defer, do not `br close` until the parent parks (closed = Past the Stand). Lined Up is only for minted-not-yet-claimed children.

**Goal:** An owned, enabled, lifecycle-active timer oneshot with no future trigger appears on `scope show` attention with the code `wanted_inactive`, instead of being silently `healthy` with `attention=[]`.

**Architecture:** A pure predicate over systemd fields (`is_wanted_inactive_timer`) lives beside the existing health helpers in `src/operations.rs`; `scope::aggregate` combines it with lifecycle completion and criticality inside the existing `match op_health`, adding an attention entry without changing any health value; the TUI gains one row label and `docs/SCOPES.md` names the code. This matches the spec exactly - no new view field, no new crate, no MCP change, no touch to `apply_verb`.

**Tech stack:** Rust 2021 (rust-version 1.82), `serde_json::Value` views, inline `#[cfg(test)] mod tests` per module, `cargo test --lib` as the runner. Nothing new is added to `Cargo.toml`.

---

## File structure

| File | Change | Responsibility |
|---|---|---|
| `src/operations.rs` | modify `enablement_is_on` (`:1209-1214`), add `pub fn enablement_on` + `pub fn is_wanted_inactive_timer` after `is_timer_operation` (`:1224-1226`), add tests in `mod tests` (`:2017+`) | Systemd truth: is this a wanted timer oneshot with no future trigger. Knows nothing about scopes or lifecycle. |
| `src/scope.rs` | modify `aggregate` (`:397-486`): one new local after the `op_health` block (`:419-423`) and the `"unknown"` arms of the match (`:455-484`); add tests in `mod tests` (`:653+`) | Scope truth: combines the predicate with lifecycle completion and criticality, emits `Attention`, leaves health arithmetic alone. |
| `src/tui.rs` | modify `Row` (`:126-176`), the `Row` literal in `from_view` (`:753-805`), `when_label` (`:2250-2283`), and the `dummy` test helper (`:2481+`); add one test | Presentation only: a dark wanted timer reads `not scheduled` instead of a blank cell. |
| `docs/SCOPES.md` | modify the Health section (`:206-226`), the ScopeView block (`:227-240`), and the Aggregation section (`:265-280`) | Names `wanted_inactive` at the same weight as `operation_failed` / `not-scheduled`. |

No other file is touched. Do not edit `src/mcp.rs`, `src/varlink.rs`, `src/write.rs`, `src/automation.rs`, `Cargo.toml`, or anything under `tests/`.

---

### Task 1: Detector predicate in `operations.rs`

**Owner:** builder
**Files:**
- Modify: `src/operations.rs:1209-1214` (`enablement_is_on`)
- Modify: `src/operations.rs:1224-1226` (insert the two new functions directly after `is_timer_operation`)
- Test: `src/operations.rs` (inside `#[cfg(test)] mod tests`, which starts at `:2017` and has `use super::*;`, append after `successful_scheduled_without_next_is_unknown` at `:2402-2414`)

**Verification (anti-gameable):** `cargo test --lib operations::` passes, and the new tests assert that `operation_health` and `health_basis` return exactly what they return today for the same views (`unknown` / `not-scheduled`, and `unknown` / `never-run`). A builder cannot satisfy them by changing a health value - the only way through is a new predicate. Re-runnable by the lead with no fixture setup.

- [x] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/operations.rs`:

```rust
    #[test]
    fn enablement_on_matches_systemd_wanted_states() {
        assert!(enablement_on("enabled"));
        assert!(enablement_on("enabled-runtime"));
        assert!(enablement_on("static"));
        assert!(!enablement_on("disabled"));
        assert!(!enablement_on("masked"));
        assert!(!enablement_on(""));
    }

    #[test]
    fn wanted_inactive_pins_the_0914_shape() {
        let view = json!({
            "kind": "oneshot",
            "activation": "timer",
            "enablement": "enabled",
            "next": Value::Null,
            "last_result": "success",
            "last": "2026-09-14T10:12:13Z",
            "state": "inactive",
            "sub": "dead",
        });
        assert!(is_wanted_inactive_timer(&view));
        assert_eq!(operation_health(&view), "unknown");
        assert_eq!(health_basis(&view), "not-scheduled");
    }

    #[test]
    fn wanted_inactive_covers_never_run_without_moving_its_basis() {
        let view = json!({
            "kind": "oneshot",
            "activation": "timer",
            "enablement": "enabled",
            "next": Value::Null,
            "last_result": Value::Null,
            "last": Value::Null,
            "state": "inactive",
            "sub": "dead",
        });
        assert!(is_wanted_inactive_timer(&view));
        assert_eq!(operation_health(&view), "unknown");
        assert_eq!(health_basis(&view), "never-run");
    }

    #[test]
    fn wanted_inactive_rejects_scheduled_disabled_simple_direct_and_missing() {
        let scheduled = json!({
            "kind": "oneshot",
            "activation": "timer",
            "enablement": "enabled",
            "next": "2026-09-17T07:00:00Z",
            "last_result": "success",
            "last": "2026-09-16T07:00:00Z",
        });
        assert!(!is_wanted_inactive_timer(&scheduled));

        let disabled = json!({
            "kind": "oneshot",
            "activation": "timer",
            "enablement": "disabled",
            "next": Value::Null,
            "last_result": "success",
            "last": "2026-09-16T07:00:00Z",
        });
        assert!(!is_wanted_inactive_timer(&disabled));

        let simple = json!({
            "kind": "simple",
            "activation": "direct",
            "enablement": "enabled",
            "state": "inactive",
            "sub": "dead",
        });
        assert!(!is_wanted_inactive_timer(&simple));

        let direct = json!({
            "kind": "oneshot",
            "activation": "direct",
            "enablement": "enabled",
            "next": Value::Null,
            "last_result": "success",
            "last": "2026-09-16T07:00:00Z",
        });
        assert!(!is_wanted_inactive_timer(&direct));

        let missing = json!({
            "kind": Value::Null,
            "enablement": "enabled",
            "next": Value::Null,
            "missing": true,
        });
        assert!(!is_wanted_inactive_timer(&missing));
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib operations::tests::wanted_inactive 2>&1 | tail -20`
Expected: FAIL - the crate does not compile:

```
error[E0425]: cannot find function `is_wanted_inactive_timer` in this scope
error[E0425]: cannot find function `enablement_on` in this scope
```

- [x] **Step 3: Write the minimal implementation**

Replace `enablement_is_on` at `src/operations.rs:1209-1214` with:

```rust
/// True for a unit-file state systemd considers wanted at boot.
pub fn enablement_on(state: &str) -> bool {
    matches!(state, "enabled" | "enabled-runtime" | "static")
}

fn enablement_is_on(enablement: &Value) -> bool {
    enablement.as_str().is_some_and(enablement_on)
}
```

Insert directly after `is_timer_operation` (`src/operations.rs:1224-1226`):

```rust
/// Wanted by systemd but not scheduled: an enabled timer oneshot with no
/// future trigger. Lifecycle completion is not consulted here; the scope
/// layer owns that.
pub fn is_wanted_inactive_timer(view: &Value) -> bool {
    if view.get("missing").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if !matches!(
        view.get("kind").and_then(Value::as_str),
        Some("oneshot") | Some("oneshot-linger")
    ) {
        return false;
    }
    is_timer_operation(view)
        && enablement_is_on(view.get("enablement").unwrap_or(&Value::Null))
        && !has_future_trigger(view)
}
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib operations:: 2>&1 | tail -5`
Expected: PASS - `test result: ok.` with 0 failed. The four new tests appear as
`operations::tests::enablement_on_matches_systemd_wanted_states`,
`operations::tests::wanted_inactive_pins_the_0914_shape`,
`operations::tests::wanted_inactive_covers_never_run_without_moving_its_basis`,
`operations::tests::wanted_inactive_rejects_scheduled_disabled_simple_direct_and_missing`.

- [x] **Step 5: Commit**

```bash
git add src/operations.rs
git commit -m "operations: detect a wanted-but-inactive timer oneshot

An enabled timer oneshot with no future trigger is dark: systemd wants
it, nothing will fire it. Expose the condition as a predicate next to
the health helpers, without moving any health value."
```

---

### Task 2: Scope aggregation emits `wanted_inactive`

**Owner:** builder
**Files:**
- Modify: `src/scope.rs:419-423` (add one local after the `op_health` block)
- Modify: `src/scope.rs:474-484` (the `"unknown" if critical` arm, plus one new arm before `_ => {}`)
- Test: `src/scope.rs` (inside `#[cfg(test)] mod tests`, which starts at `:653` with `use super::*;` and `use std::fs;`, append after `unknown_critical_is_unknown` at `:984-997`)

**Verification (anti-gameable):** `cargo test --lib scope::` passes. The tests call the real `aggregate` with a real `ScopeManifest`, and the completed case writes a real `state/lifecycle.json` into a temp scope root - "completed" is only true because `automation::load_lifecycle` reads that file (`src/automation.rs:473-493`), so it cannot be faked by a flag in the test view. The non-critical test asserts `view.health == ScopeHealth::Healthy` **and** `attention.len() == 1` in the same test, so neither "degrade the scope" nor "stay silent" passes.

- [x] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/scope.rs`:

```rust
    fn dark_0914(unit: &str) -> Value {
        op(
            unit,
            json!({
                "kind": "oneshot",
                "activation": "timer",
                "enablement": "enabled",
                "next": Value::Null,
                "last_result": "success",
                "last": "2026-09-14T10:12:13Z",
                "state": "inactive",
                "sub": "dead",
            }),
        )
    }

    #[test]
    fn wanted_inactive_timer_gets_attention_without_degrading_scope() {
        let view = aggregate(&manifest(), vec![dark_0914("managed-speech-watch")], vec![]);
        assert_eq!(view.health, ScopeHealth::Healthy);
        assert_eq!(view.attention.len(), 1, "{:?}", view.attention);
        assert_eq!(view.attention[0].operation, "managed-speech-watch");
        assert_eq!(view.attention[0].relationship, "owned");
        assert_eq!(view.attention[0].code, "wanted_inactive");
        assert_eq!(view.attention[0].reason, "enabled, no future trigger");
        assert_eq!(view.owned[0]["health"], json!("unknown"));
        assert_eq!(view.owned[0]["critical"], json!(false));
    }

    #[test]
    fn critical_wanted_inactive_keeps_todays_unknown_health() {
        let view = aggregate(&manifest(), vec![dark_0914("managed-speech-asr")], vec![]);
        assert_eq!(view.health, ScopeHealth::Unknown);
        assert_eq!(view.attention.len(), 1, "{:?}", view.attention);
        assert_eq!(view.attention[0].code, "wanted_inactive");
        assert_eq!(view.owned[0]["critical"], json!(true));
    }

    #[test]
    fn enabled_timer_with_a_future_trigger_is_quiet() {
        let view = aggregate(
            &manifest(),
            vec![op(
                "managed-speech-watch",
                json!({
                    "kind": "oneshot",
                    "activation": "timer",
                    "enablement": "enabled",
                    "next": "2026-09-17T07:00:00Z",
                    "last_result": "success",
                    "last": "2026-09-16T07:00:00Z",
                }),
            )],
            vec![],
        );
        assert_eq!(view.health, ScopeHealth::Healthy);
        assert!(view.attention.is_empty(), "{:?}", view.attention);
    }

    #[test]
    fn completed_and_disabled_timer_stays_quiet() {
        let root = tmp("completed-quiet");
        let state = root.join(".systemd-ops/operations/managed-speech-watch/state");
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("lifecycle.json"),
            "{\"status\":\"completed\",\"completed_at\":\"2026-09-15T00:00:00Z\",\"reason\":\"merged\"}",
        )
        .unwrap();
        let mut m = manifest();
        m.root = root.clone();
        let view = aggregate(
            &m,
            vec![op(
                "managed-speech-watch",
                json!({
                    "kind": "oneshot",
                    "activation": "timer",
                    "enablement": "disabled",
                    "next": Value::Null,
                    "last_result": "success",
                    "last": "2026-09-15T00:00:00Z",
                    "state": "inactive",
                    "sub": "dead",
                }),
            )],
            vec![],
        );
        assert_eq!(view.health, ScopeHealth::Healthy);
        assert!(view.attention.is_empty(), "{:?}", view.attention);
        let _ = fs::remove_dir_all(&root);
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib scope::tests::wanted_inactive_timer_gets_attention_without_degrading_scope 2>&1 | tail -15`
Expected: FAIL with
```
assertion `left == right` failed: []
  left: 0
 right: 1
```
from the `attention.len()` assertion - today the entry is never pushed.
`cargo test --lib scope::tests::critical_wanted_inactive_keeps_todays_unknown_health`
fails on `code`, with `left: "operation_unknown"` and `right: "wanted_inactive"`.
`enabled_timer_with_a_future_trigger_is_quiet` and
`completed_and_disabled_timer_stays_quiet` pass already - they are the
regression fence for criteria 2 and 3 and must still pass in Step 4.

- [x] **Step 3: Write the minimal implementation**

In `src/scope.rs`, immediately after the `let op_health = if completed { ... };`
block that ends at `:423`, add the local:

```rust
        let wanted_inactive = !completed && operations::is_wanted_inactive_timer(&view);
```

Then replace the `"unknown" if critical` arm at `:474-481` and add one arm
before `_ => {}`, so the `match` reads:

```rust
            "unknown" if critical => {
                health = health.raise(ScopeHealth::Unknown);
                attention.push(Attention {
                    operation: unit,
                    relationship: "owned",
                    code: if wanted_inactive {
                        "wanted_inactive"
                    } else {
                        "operation_unknown"
                    },
                    reason: if wanted_inactive {
                        "enabled, no future trigger".into()
                    } else {
                        "unknown".into()
                    },
                });
            }
            "unknown" if wanted_inactive => {
                attention.push(Attention {
                    operation: unit,
                    relationship: "owned",
                    code: "wanted_inactive",
                    reason: "enabled, no future trigger".into(),
                });
            }
            _ => {}
```

Leave the two `"failed"` arms, the watching loop (`:489-521`), and every
`health.raise(...)` call exactly as they are. The new arm raises nothing.

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib scope:: 2>&1 | tail -5`
Expected: PASS - `test result: ok.` with 0 failed, including the pre-existing
`unknown_critical_is_unknown`, `all_healthy_is_healthy`,
`critical_owned_failure_fails_scope`, `noncritical_owned_failure_degrades`, and
`watched_failure_degrades_as_watching`.

- [x] **Step 5: Commit**

```bash
git add src/scope.rs
git commit -m "scope: surface wanted-but-inactive timers in attention

A stopped-but-enabled managed timer was health-invisible: owned,
non-critical, unknown health, so aggregate fell through to no attention
entry and the scope stayed healthy. Emit wanted_inactive for it and for
its critical twin, without changing any health value. Lifecycle-completed
units stay quiet."
```

---

### Task 3: TUI reads `not scheduled` for a dark wanted timer

**Owner:** builder
**Files:**
- Modify: `src/tui.rs:152` (add one field to `Row`)
- Modify: `src/tui.rs:779` (the `Row` literal inside `from_view`, which starts at `:753`)
- Modify: `src/tui.rs:2279-2281` (`when_label`, after the `never-run` branch)
- Modify: `src/tui.rs:2504` (the `dummy` test helper's `Row` literal)
- Test: `src/tui.rs` (inside `#[cfg(test)] mod tests`, append after
  `running_requires_active_iteration_and_service` at `:2777-2796`)

**Verification (anti-gameable):** `cargo test --lib tui::` passes, and the single test asserts both directions on the same row: `enablement = "enabled"` yields `not scheduled`, and flipping only that field back to `disabled` returns the label to today's empty string. A blanket label on `health_basis == "not-scheduled"` fails the second assertion.

- [x] **Step 1: Write the failing test**

Append inside `mod tests` in `src/tui.rs`:

```rust
    #[test]
    fn wanted_inactive_timer_reads_not_scheduled() {
        let mut r = dummy("managed-omp-project-lead-watch", "owned");
        r.kind = "oneshot".into();
        r.activation = "timer".into();
        r.enablement = "enabled".into();
        r.health_basis = "not-scheduled".into();
        r.last_result = "success".into();
        r.state = "inactive".into();
        r.sub = "dead".into();
        assert_eq!(when_label(&r, SystemTime::now()), "not scheduled");
        r.enablement = "disabled".into();
        assert_eq!(when_label(&r, SystemTime::now()), "");
    }
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib tui::tests::wanted_inactive_timer_reads_not_scheduled 2>&1 | tail -15`
Expected: FAIL - the crate does not compile:

```
error[E0609]: no field `enablement` on type `Row`
```

- [x] **Step 3: Write the minimal implementation**

Add the field to `Row`, directly after `health_basis: String,` (`src/tui.rs:152`):

```rust
    enablement: String,
```

Populate it in the `Row` literal inside `from_view`, directly after
`health_basis: json_str(v, "health_basis"),` (`src/tui.rs:779`):

```rust
        enablement: json_str(v, "enablement"),
```

Add it to the `dummy` test helper's literal, directly after
`health_basis: String::new(),` (`src/tui.rs:2504`):

```rust
            enablement: String::new(),
```

Add the branch in `when_label`, directly after the `never-run` branch that ends
at `src/tui.rs:2281`:

```rust
    if r.health_basis == "not-scheduled" && crate::operations::enablement_on(&r.enablement) {
        return "not scheduled".into();
    }
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib tui:: 2>&1 | tail -5`
Expected: PASS - `test result: ok.` with 0 failed, including the pre-existing
`failed_when_uses_last`, `running_requires_active_iteration_and_service`, and
`completed_row_is_distinct_and_idle`.

- [x] **Step 5: Commit**

```bash
git add src/tui.rs
git commit -m "tui: label an enabled timer with no future trigger

The when column was blank for a dark wanted timer, so the row read like
an idle unit. Say 'not scheduled', gated on enablement so a deliberately
disabled timer keeps its current blank cell."
```

---

### Task 4: `docs/SCOPES.md` names the attention code

**Owner:** builder
**Files:**
- Modify: `docs/SCOPES.md:213-216` (the scheduled-oneshot bullet in the Health section)
- Modify: `docs/SCOPES.md:236-240` (the ScopeView block)
- Modify: `docs/SCOPES.md:275-277` (after the "Attention is not a health value" paragraph, before the "Watched failure is an external dependency" paragraph)

**Verification (anti-gameable):** `rg -n "wanted_inactive" docs/SCOPES.md` prints at least three hits, one in each of the Health, ScopeView, and Aggregation sections, and the Aggregation table row states the condition and the "no health effect on its own" consequence. The lead can diff this against the spec's Components section; a one-word mention in a single place fails the three-section requirement of Brief criterion 7.

- [x] **Step 1: Name the code in the Health section**

In the `## Health (v1)` bullet list, replace the scheduled-oneshot bullet
(`docs/SCOPES.md:213-216`) with:

```markdown
- Scheduled oneshot: never-run is `unknown`, never success. Last
  actual result failure → `failed`. Last success is `healthy` only if
  the timer is enabled and a future trigger exists; otherwise
  `unknown` (`health_basis=not-scheduled`). An *enabled* timer with no
  future trigger keeps that `unknown`, and is additionally listed under
  scope attention as `wanted_inactive`: systemd wants the unit, nothing
  will fire it. Health does not move for it.
```

- [x] **Step 2: Name the code in the ScopeView block**

In the `## ScopeView` fenced block, replace the `attention[]` line
(`docs/SCOPES.md:238`) with:

```
  attention[] { operation, relationship, code, reason }
              code: operation_failed | operation_unknown | wanted_inactive
```

- [x] **Step 3: Name the code in the Aggregation section**

Directly after the paragraph "Attention is not a health value. It lists what an
operator/agent should notice. No acknowledgement lifecycle."
(`docs/SCOPES.md:275-276`), insert:

```markdown
| Attention code | Condition | Scope health effect |
|---|---|---|
| `operation_failed` | owned or watched operation health `failed` | `failed` when owned and critical, else `degraded` |
| `operation_unknown` | owned critical operation health `unknown` that is not `wanted_inactive` | `unknown` |
| `wanted_inactive` | owned timer oneshot, enablement on, lifecycle not completed, no future trigger | none on its own; `unknown` only because the unit is critical |

A lifecycle-completed operation is never listed: completion disables the
timer on purpose, and completed operations are read as `healthy` during
aggregation. A disabled, lifecycle-active timer is not listed either -
nothing wants it.
```

- [x] **Step 4: Verify the docs claim**

Run: `rg -n "wanted_inactive" docs/SCOPES.md`
Expected: at least 3 hits - one in the Health bullet (~`:212-222`), one in the
ScopeView block (~`:238-240`), and two in the new Aggregation table
(header row plus the `wanted_inactive` row).

Run: `cargo test --lib scope:: 2>&1 | tail -5`
Expected: PASS - `test result: ok.` (documentation-only change must not move code).

- [x] **Step 5: Commit**

```bash
git add docs/SCOPES.md
git commit -m "docs: name wanted_inactive alongside operation_failed

Record the new attention code at the same weight as the existing ones:
what triggers it, that it does not move scope health, and why completed
or disabled timers stay quiet."
```

---

## Out of scope for every task

Do not, in any task:

- change `operation_health`, `health_basis`, `ScopeHealth::raise`, or any
  `health.raise(...)` call site;
- add a field to an emitted operation view or `ScopeView` JSON (the new `Row`
  field is internal to the TUI and reads an existing view field);
- touch `src/mcp.rs`, `src/varlink.rs`, `apply_verb`, the plan/apply mutation
  path, retire/complete, or anything under `tests/`;
- extend the new code to `watching` entries;
- add a dependency to `Cargo.toml`;
- start, enable, stop, or retire any live unit, or run `systemctl` at all;
- run a formatter or the live shell suites in `tests/`.
