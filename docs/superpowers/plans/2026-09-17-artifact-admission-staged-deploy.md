# Artifact Admission and Staged Runtime Deploy Implementation Plan

**Technical Spec:** `docs/superpowers/specs/2026-09-17-artifact-admission-staged-deploy.md`
**Design Brief:** `docs/superpowers/briefs/2026-09-17-artifact-admission-staged-deploy.md` (via spec; do not bypass)
**Bead:** `sdo-a89` (parent) / `sdo-a89.1` (this plan)

> **For the project lead:** first `br where` in this repo. then task-by-task
> with isolated `builder` workers. Do not implement these tasks inline.
> Persist progress in this file's checkboxes **and** as one parent-child
> bead per Task (`br create --parent sdo-a89 --body`). Title `tN: <ask>`
> (prefix `[done] ` after verifier pass). Description markdown, <=800
> characters, wrap at ~60 cols: `Asked:` paragraph, then `## Landed`
> with sha/tests/leftover (`not yet` while in flight). Do not paste this
> plan packet. After pass, keep the child `in_progress` until the parent
> parks. Do not `br close` until the parent parks.

**Goal:** A verified artifact enters a capability only through
`automation plan-admit` + `apply`. A READY compose produces a candidate
generation and does not flip `generations/current`. midi canary ACK is
the only promote. GC uses occupancy midi publishes. Merged PRs and
empty-delta capabilities leave the organism through normalize verbs.
Topology names the loops, not the instances. `.66` then `omp-vuz.1`
travel the machine.

**Architecture:** New mutations are `PlanClass::Automation` actions
applied through `write::apply`. Desired artifacts live in
`state/desired-inputs.json` and join `effective_input_fingerprint`
only when that file exists. Deploy phase lives in
`state/deployment.json` on the runtime-integrator.
`published-generation.json` stays the promoted pointer. The current-flip
moves out of `runtime-run` into `dogfood/lib/runtime-promote`, invoked
only by the canary-pass apply. Capability-run condition 4 is reused,
not rewritten. system-notify remains transport.

**Tech stack:** Rust 2021 (rust-version 1.82), existing sealed tokens,
bash dogfood drivers, hermetic `tests/*.sh`. Nothing new in
`Cargo.toml`.

---

## File structure

| File | Change | Responsibility |
|---|---|---|
| `src/automation.rs` | fingerprint third line; plan-admit / plan-deploy-* / plan-normalize-*; inspect fields; apply actions | Machine state and sealed verbs |
| `src/bin/systemd_ops.rs` | USAGE + dispatch for the new `automation` subcommands | CLI surface only |
| `src/scope.rs` | attention codes `deploy_stalled`, `canary_pending`, `admission_unconsumed` | ScopeView stall surface |
| `src/tui.rs` | reason-column labels for the three codes | Presentation |
| `src/mcp.rs` | only if `get_operation` must pass through the new inspect keys (it should already, via the same object) | No new tool |
| `dogfood/drivers/runtime-run` | stop flipping `current` on READY; write candidate; notify midi | Candidate only |
| `dogfood/lib/runtime-promote` | new; the extracted flip + published-generation write | Promote apply helper |
| `dogfood/lib/automation-wrapper` | no new notify semantics; source runtime-promote only if tests need it | Keep observe_input |
| `docs/TESTING.md` | name the new proofs and the rewritten publish proof | Durable CI map |
| `docs/SCOPES.md` | name the three attention codes | Same weight as existing codes |
| `/home/sf/workspace/oh-my-pi/.systemd-ops/topology.txt` | rewrite to invariant loops (Task 9 only) | Human-authored, not hashed |

Do not edit HCOM, `recycle_actor`, live capability units, or
`/home/sf/.omp/wt/ayn-66-f6de4bd`.

---

### Task 1: Desired-inputs join the observation hash

**Owner:** builder
**Files:**
- Modify: `src/automation.rs:330-331` (`effective_input_fingerprint`)
- Modify: `src/automation.rs:704-716` (`write_observation`) and the
  observe path that calls it (`:1091-1098`) so a present
  `state/desired-inputs.json` is hashed
- Test: `src/automation.rs` `#[cfg(test)] mod tests` next to
  `observer_payload_and_effective_input_are_strict_and_stable`

**Verification (anti-gameable):** a unit test that the no-file case
equals today's `sha256("world={wf}\nbrain={br}\n")` byte for byte, and
that creating the file with one input changes the hex. A builder
cannot satisfy both by always appending `desired=`. `cargo test --lib
automation::` passes.

- [ ] **Step 1: Write the failing tests**
- [ ] **Step 2: Make the tests pass**
- [ ] **Step 3: Commit**

`git commit -m "feat: hash desired-inputs into observation when present"`

---

### Task 2: `plan-admit` for an existing capability

**Owner:** builder
**Depends:** Task 1
**Files:**
- Modify: `src/automation.rs` (new `plan_admit` / apply action `admit`)
- Modify: `src/bin/systemd_ops.rs` USAGE + dispatch
- Create: `tests/admission.sh` (existing-capability half only)

**Verification:** `tests/admission.sh` creates a fake owned capability
home with `automation.toml`, calls `plan-admit` + `apply` with a valid
commit triple, and asserts `desired-inputs.json` gained exactly one
entry and `automation observe` moved `input_fingerprint`. A second
identical admit is a no-op (same bytes, same hash). The same proof
repeats with `artifact_kind=worktree-delta` (base + one file sha256);
duplicate identity is a no-op. `overseer` as `originating_work` or
`capability` is refused with no write. Malformed revision (not 40
hex), mixed kinds, empty delta, and a `..` path are refused. No unit
is created or rewritten.

- [ ] **Step 1: Write the failing proof**
- [ ] **Step 2: Make the proof pass**
- [ ] **Step 3: Commit**

`git commit -m "feat: automation plan-admit for an existing capability"`

This is the midi-facing invoke. After this commit the existing-cap
path is callable. New-cap creation is Task 3.

---

### Task 3: `plan-admit` creates a new capability

**Owner:** builder
**Depends:** Task 2
**Files:**
- Modify: `src/automation.rs` admit apply: compose existing
  `plan_instance("create")` when the unit does not exist
- Modify: `tests/admission.sh` (new-capability half)
- Do not write live units under oh-my-pi

**Verification:** on a throwaway scope, admit of an unknown
`capability` creates `managed-omp-cap-<id>`, writes `automation.toml`
with `agent=capability-maintainer` and
`parent=managed-omp-runtime-integrator`, writes the first desired
input, and leaves the timer enabled. A colliding `capability:` tag
with a sibling is refused. `fork/cap/<id>` is not pushed.
Worktree-delta of a new capability copies no bytes and does not
`git worktree add` from the classified tree.

- [ ] **Step 1: Write the failing proof**
- [ ] **Step 2: Make the proof pass**
- [ ] **Step 3: Commit**

`git commit -m "feat: plan-admit creates a capability-maintainer child"`

---

### Task 4: Candidate compose does not flip current

**Owner:** builder
**Depends:** Task 1 (no API dependency on 2/3; may run after 1)
**Files:**
- Modify: `dogfood/drivers/runtime-run` (`publish_runtime_generation`)
- Create: `dogfood/lib/runtime-promote` (extracted flip)
- Modify: `tests/runtime-generation-publish.sh` (rewrite the flip pins)

**Verification:** the existing proof, rewritten, fails if READY compose
moves `generations/current` or the omp install, and passes when those
pointers stay on the previous promoted generation while
`deployment.json` records `phase=candidate` and
`published-generation.json` is unchanged. Sourcing
`runtime-promote` and invoking the flip then moves both pointers and
rewrites `published-generation.json`. `prune_runtime_generations` is
not called from READY compose.

- [ ] **Step 1: Write the failing (rewritten) proof**
- [ ] **Step 2: Make the proof pass**
- [ ] **Step 3: Commit**

`git commit -m "feat: compose writes a candidate and does not flip current"`

---

### Task 5: Canary ACK and atomic promote

**Owner:** builder
**Depends:** Task 4
**Files:**
- Modify: `src/automation.rs` (`plan_deploy_canary_ack` / apply)
- Modify: `src/bin/systemd_ops.rs`
- Create: `tests/deploy-canary.sh`

**Verification:** fail ACK (`result=fail`, or actor not midi, or
observed_generation mismatch) leaves `current` and
`published-generation.json` on the previous promoted generation and
records `canary.result=fail`. Pass ACK flips exactly once, sets
phase `rolling_out`, and leaves `current` and
`published-generation.json` naming the same output revision. A second
pass ACK against the same candidate is a no-op flip (already
promoted). systemd-ops does not call `recycle_actor` or `hcom r`.

- [ ] **Step 1: Write the failing proof**
- [ ] **Step 2: Make the proof pass**
- [ ] **Step 3: Commit**

`git commit -m "feat: midi canary ACK is the only promote"`

---

### Task 6: Occupancy and generation GC

**Owner:** builder
**Depends:** Task 5
**Files:**
- Modify: `src/automation.rs` (`plan_deploy_occupancy`, `plan_deploy_gc`)
- Modify: `src/bin/systemd_ops.rs`
- Create: `tests/deploy-gc.sh`
- Modify: `dogfood/drivers/runtime-run` (delete newest-3 prune call sites)

**Verification:** occupancy apply rewrites the `occupancy` array and
moves phase to `converged` when every row is acked on promoted or
`stopped`/`retired`. GC refuses a `state=live` row naming G, refuses
when `current` points at G, and removes G when both checks pass.
`rg prune_runtime_generations dogfood/drivers/runtime-run` is empty.

- [ ] **Step 1: Write the failing proof**
- [ ] **Step 2: Make the proof pass**
- [ ] **Step 3: Commit**

`git commit -m "feat: generation GC uses published occupancy"`

---

### Task 7: PR normalize

**Owner:** builder
**Depends:** none (disjoint). Do not land before Task 2 if sharing
CLI dispatch; serialize the `systemd_ops.rs` hunks or land after 2.
**Files:**
- Modify: `src/automation.rs` (`plan_normalize_pr` / apply)
- Modify: `src/bin/systemd_ops.rs`
- Create: `tests/normalize-pr.sh`

**Verification:** missing `merge_sha` is refused. A valid apply
retires the unit (absent from `list-operations`) and emits one
`system_notify` inform (the test stubs the seam, does not send).
`rg -n 'gh |github.com|api.github' src/automation.rs` gains no new
GitHub call site. Worktree removal runs only when `git worktree list`
names the operation `cwd`.

- [ ] **Step 1: Write the failing proof**
- [ ] **Step 2: Make the proof pass**
- [ ] **Step 3: Commit**

`git commit -m "feat: normalize a merged PR out of the organism"`

---

### Task 8: Capability normalize

**Owner:** builder
**Depends:** Task 7 for the shared retire composition
**Files:**
- Modify: `src/automation.rs` (`plan_normalize_capability`)
- Modify: `src/bin/systemd_ops.rs`
- Create: `tests/normalize-capability.sh`

**Verification:** a fixture with a non-empty merge-tree is refused; a
fixture with a `pr-maintainer` child is refused even with an empty
tree; a fixture with no PR child and empty merge-tree retires the
unit. The empty-delta command is the existing
`capability_pass_is_verified_noop` (sourced, not copied).

- [ ] **Step 1: Write the failing proof**
- [ ] **Step 2: Make the proof pass**
- [ ] **Step 3: Commit**

`git commit -m "feat: retire a capability only on empty delta and no PR children"`

---

### Task 9: Inspect, attention, topology, TESTING.md

**Owner:** builder
**Depends:** Tasks 2, 5, 6 (fields must exist)
**Files:**
- Modify: `src/automation.rs` `inspect`
- Modify: `src/scope.rs` aggregate
- Modify: `src/tui.rs` reason labels
- Modify: `docs/SCOPES.md`
- Modify: `docs/TESTING.md` (name new proofs + rewritten publish proof)
- Modify: `/home/sf/workspace/oh-my-pi/.systemd-ops/topology.txt`
  (lead applies this write; builder prepares the text in-repo at
  `docs/superpowers/topology-omp-invariant.txt` and the lead copies)

**Verification:** `automation inspect` on a fixture runtime unit
returns `deployment.phase` and `deployment.stall`. `scope show`
attention contains `canary_pending` for a candidate with pending
canary and `admission_unconsumed` for a cap whose desired commit is
not an ancestor of HEAD or whose worktree-delta hashes do not match. The topology file (in-repo copy) has no
`PR [0-9]`, no `managed-omp-pr-`, and no concrete capability
instance names; it describes the two loops from the charter.
`docs/TESTING.md` names every new `tests/*.sh` added by Tasks 2-8.
No em dash, no non-ASCII.

- [ ] **Step 1: Write the failing proofs / doc assertions**
- [ ] **Step 2: Make them pass**
- [ ] **Step 3: Commit**

`git commit -m "feat: surface deploy and admission stalls; rewrite topology"`

---

### Task 10: Dogfood `.66` then `omp-vuz.1` (lead + midi, not a builder)

**Owner:** hera, with midi
**Depends:** Tasks 2-6 at minimum. Task 3 if `.66` needs a new cap.
**Forbidden:** hand-author of livu's proposed units; copying
`/home/sf/.omp/wt/ayn-66-f6de4bd`; cherry-pick onto runtime; flipping
`~/.bun/bin/omp` by hand.

**Verification:** each specimen has an admit event, a moved
capability fingerprint, a READY checkpoint that contains the
admitted revision, a candidate `deployment.json` that did not flip
`current`, a midi canary ACK, a promote that did, and occupancy
rows. Product-close happens after rollout, not after compose.

- [ ] **Step 1: midi supplies the `.66` worktree-delta and invokes admit**
- [ ] **Step 2: maintainer consumes; compose candidates; midi canaries**
- [ ] **Step 3: promote + occupancy; then repeat for `omp-vuz.1`**

No commit from this task unless a bugfix falls out. Bugfixes get
their own child bead.

---

## Order and parallelism

```
Task 1
  +- Task 2 -> Task 3
  +- Task 4 -> Task 5 -> Task 6
Task 7 -> Task 8          (serialize systemd_ops.rs with 2 if concurrent)
Task 9 after 2, 5, 6, 7, 8
Task 10 after 2-6
```

Two builders may run Task 2 and Task 4 in parallel after Task 1.
They must not share a file. Task 7 waits for Task 2 to finish if
both touch `src/bin/systemd_ops.rs`, or Task 7 lands first and
Task 2 rebases.

## Out of scope for every builder

- Classifying `.66` or `omp-vuz.1`
- Authoring live `managed-omp-cap-*` on the old path
- Pushing `fork/cap/*`
- Recycle of midi or any actor
- Closing `bn2`, `r4j`, `du5`, `s7r`
