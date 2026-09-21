# Artifact Admission and Staged Runtime Deploy Implementation Plan

**Technical Spec:** `docs/superpowers/specs/2026-09-17-artifact-admission-staged-deploy.md`
**Design Brief:** `docs/superpowers/briefs/2026-09-17-artifact-admission-staged-deploy.md` (via spec; do not bypass)
**Bead:** `sdo-a89` (parent) / `sdo-a89.1` (this plan). Tasks 11-14
are the `sdo-a89.14` amendment (first-capability bootstrap, spec
D1a).

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

- [x] **Step 1: Write the failing tests**
- [x] **Step 2: Make the tests pass**
- [x] **Step 3: Commit**

`git commit -m "feat: hash desired-inputs into observation when present"`
Landed `7a2862f`. `review-sdo-a89.2` PASS.

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

- [x] **Step 1: Write the failing proof**
- [x] **Step 2: Make the proof pass**
- [x] **Step 3: Commit**

`git commit -m "feat: automation plan-admit for an existing capability"`
Landed `6106d14`. `review-sdo-a89.3` PASS.

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

- [x] **Step 1: Write the failing proof**
- [x] **Step 2: Make the proof pass**
- [x] **Step 3: Commit**

`git commit -m "feat: plan-admit creates a capability-maintainer child"`
Landed `e962b5b`. `review-sdo-a89.4` PASS.

---

### Task 3b: `plan-admit` replaces a superseded identity

**Owner:** builder
**Depends:** Task 3
**Files:**
- Modify: `src/automation.rs` (`parse_admit_spec`, `plan_admit`,
  `apply_admit`)
- Modify: `tests/admission.sh` (existing-capability replace proofs)

**Verification:** on a throwaway existing capability with one
desired input, `plan-admit` + `apply` with `supersedes` naming
that identity writes exactly one remaining entry (the new
artifact), moves `input_fingerprint`, and does not rewrite the
unit. A missing `supersedes` target, a same-identity
`supersedes`, `supersedes` on a new capability, and a
replacement that already exists as another entry are refused
with no write. Overlay/copy/`git worktree add` stay absent.

- [x] **Step 1: Write the failing proof**
- [x] **Step 2: Make the proof pass**
- [x] **Step 3: Commit**

`git commit -m "feat: plan-admit supersedes replaces a desired identity"`
Landed `c517494`. `review-sdo-a89.7` PASS. Hop named callable
to midi 2026-09-17T19:32Z (repo binary at c517494; live PATH
still 0.7.0 af1ad5b8). Midi invoked 2026-09-17T19:35:37Z
(`#100469`). Apply verified: desired-inputs is exactly one
entry, identity `b52e7cd`, `256625ad` gone (removed, not
consumed), unit spec-sha still `64ce6bd9`, automation.toml
mtime still 17:55:33Z. Midi synced live drivers `#100628`
(4186911 / c3beac6 / runtime-promote). Hera started the
timer+service 2026-09-18T03:46:33Z. Pass READY 03:51:26Z:
checkpoint generation `37273117` output `53873e8c`, C still
an ancestor (consumed), worktree clean, `current` still
`eb966708`. Integrator compose is in flight as of 03:52:35Z
and must write a candidate without flipping `current`.

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

- [x] **Step 1: Write the failing (rewritten) proof**
- [x] **Step 2: Make the proof pass**
- [x] **Step 3: Commit**

`git commit -m "feat: compose writes a candidate and does not flip current"`
Landed `c3beac6`. `review-sdo-a89.6` PASS.

---

### Task 5: Canary ACK and atomic promote

**Owner:** builder
**Depends:** Task 4
**Files:**
- Modify: `src/automation.rs` (`plan_deploy_canary_ack` / apply)
- Modify: `src/bin/systemd_ops.rs`
- Create: `tests/deploy-canary.sh`
- Modify: `docs/TESTING.md` (name the new proof in Fast gates)
- Modify: `.github/workflows/ci.yml` (Dogfood proofs step)

**Verification:** fail ACK (`result=fail`, actor not midi,
observed_generation mismatch, or failed continuity) leaves
`current` and `published-generation.json` on the previous promoted
generation, records `canary.result=fail`, writes `recovery.target`
as that previous promoted identity (`omp@` + first 8), notifies
midi via the system-notify seam (stubbed; no real hcom) with a
message that names the failed candidate `omp@`+8, the restored
`omp@`+8, and the refuse reason, preserves the candidate, and does
not set phase `rolling_out`. Pass notify keeps the existing
success note. Midi is a required recovery occupant: a later
`result=pass` is refused until `result=recovered` (observed equal
to `recovery.target.sha`) or an occupancy row `name=midi`
`state=live` on that sha. Recovered does not flip. Pass after
recovered flips exactly once, sets phase `rolling_out`, and leaves
`current` and `published-generation.json` naming the same output
revision. A second pass ACK against the same candidate is a no-op
flip (already promoted). systemd-ops does not call `recycle_actor`
or `hcom r`. Add `tests/deploy-canary.sh` to the Fast gates list in
`docs/TESTING.md` and the Dogfood proofs step in
`.github/workflows/ci.yml`.

- [x] **Step 1: Write the failing proof**
- [x] **Step 2: Make the proof pass**
- [x] **Step 3: Commit**

`git commit -m "feat: midi canary ACK is the only promote"`
Landed `86cb28a`. `review-sdo-a89.8` PASS. leftover: occupancy
hold-clear proven in Task 6 (`f13e963`); pass no-op does not heal
a missed post-promote deployment.json write.

---

### Task 6: Occupancy and generation GC

**Owner:** builder
**Depends:** Task 5
**Files:**
- Modify: `src/automation.rs` (`plan_deploy_occupancy`, `plan_deploy_gc`)
- Modify: `src/bin/systemd_ops.rs`
- Create: `tests/deploy-gc.sh`
- Modify: `dogfood/drivers/runtime-run` (delete newest-3 prune call sites)

**Verification:** occupancy apply rewrites the `occupancy` array
using the D3c record (`runtime_generation.sha` is output_revision,
label is `omp@` + first 8). Phase becomes `converged` when every
row is acked on promoted or `stopped`/`retired`. GC refuses a
`state=live` row naming that sha, refuses when `current` points at
it, and removes it when both checks pass.
`automation inspect` `runtime_generation.retained` lists every
kept directory with at least one of the five reasons; a leftover
with empty reasons is GC-eligible.
`rg prune_runtime_generations dogfood/drivers/runtime-run` is empty.
Already-live rows keep their recorded generation until a later
occupancy apply. A live row is not a kill. New launches bind to
promoted current; systemd-ops does not author them.

- [x] **Step 1: Write the failing proof**
- [x] **Step 2: Make the proof pass**
- [x] **Step 3: Commit**

`git commit -m "feat: generation GC uses published occupancy"`
Landed `f13e963`. `review-sdo-a89.9` PASS. leftover: inspect
cohort/pending and attaching occupancy rows is Task 9.
Occupancy midi-live hold-clear for canary pass is proven.

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

- [x] **Step 1: Write the failing proof**
- [x] **Step 2: Make the proof pass**
- [x] **Step 3: Commit**

`git commit -m "feat: normalize a merged PR out of the organism"`
Landed `8c2a12c`. `review-sdo-a89.10` PASS. leftover: retired
automation home remains (existing plan-retire preserves homes);
merged_at is token metadata, not RFC3339-parsed; github rg in
the proof is same-run.

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

- [x] **Step 1: Write the failing proof**
- [x] **Step 2: Make the proof pass**
- [x] **Step 3: Commit**

`git commit -m "feat: retire a capability only on empty delta and no PR children"`
Landed `e1e6b20`. `review-sdo-a89.11` PASS. leftover: predicate is
sed-sourced from capability-run (capability-run has no
CAPABILITY_RUN_LIB=1 guard); not a copied body.

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

- [x] **Step 1: Write the failing proofs / doc assertions**
- [x] **Step 2: Make them pass**
- [x] **Step 3: Commit**

`git commit -m "feat: surface deploy and admission stalls; rewrite topology"`
Landed `d7a5871` and required-fix `24ea3af`. Isolated
`review-sdo-a89.12` FAIL on D8 talker get_operation;
`review-sdo-a89.12b` PASS. leftover: live oh-my-pi topology copy
is lead-only (applied); deployment_inspect stub when file absent
and integrator active; hermetic proof does not pin
runtime_generation/retained/omp@+8; pre-existing tui.rs em dashes.

---

### Task 10: Dogfood `.66` then `omp-vuz.1` (lead + midi, not a builder)

**Owner:** hera, with midi
**Depends:** Tasks 2-6 at minimum. Task 3 if `.66` needs a new cap.
Tasks 11-14 for `omp-vuz.1` (first-capability bootstrap; without
them the first maintainer tick dies on the absent
`fork/cap/todo-passive-phase` ref and no READY checkpoint can name
`76643a6871e1a17c4a199564df95e73490b1060a`).
**Forbidden:** hand-author of livu's proposed units; copying
`/home/sf/.omp/wt/ayn-66-f6de4bd`; cherry-pick onto runtime; flipping
`~/.bun/bin/omp` by hand; `mkdir` or `git worktree add` of
`/home/sf/worktrees/omp/admit/retry-fallback-unowned` or
`/home/sf/worktrees/omp/admit/todo-passive-phase` before apply;
treating the livu pre-admission tips
(`/home/sf/worktrees/omp/cap/retry-fallback-unowned` at `b5a0e2264b`,
`/home/sf/worktrees/omp/cap/todo-passive-phase` at `4517803da2`) as
capability authority. The capability home is the unit's bound cwd
(spec D1a): live, `managed-omp-cap-todo-passive-phase` binds
`/home/sf/worktrees/omp/cap/todo-passive-phase`, and that path is
its home; only its pre-admission tip is refused as authority.

**Verification:** each specimen has an admit event, a moved
capability fingerprint, a READY checkpoint that contains the
admitted artifact identity (`.66`: worktree-delta base + path
sha256s, not a 40-hex; `omp-vuz.1`: the commit midi picks), a
candidate `deployment.json` that did not flip `current`, a midi
canary ACK, a promote that did, and occupancy rows. Product-close
happens after rollout, not after compose.

- [ ] **Step 1: midi supplies the `.66` worktree-delta and invokes admit**
- [ ] **Step 2: maintainer consumes; compose candidates; midi canaries**
- [ ] **Step 3: promote + occupancy; then repeat for `omp-vuz.1`**

No commit from this task unless a bugfix falls out. Bugfixes get
their own child bead.

### Task 11: Checkpoint carries the admitted set

**Owner:** builder
**Depends:** none (Rust only; lands before 12/14 which consume it)
**Files:**
- Modify: `src/automation.rs` (`Checkpoint` struct :81-92,
  `validate_checkpoint` :532-556, `write_checkpoint` :774-799,
  `checkpoint_json` :877-900, relations `checkpoint_record`
  :1255-1279, `child_revision` :965-999, and every
  `write_checkpoint` call site - find them with
  `rg -n 'write_checkpoint' src/`)
- Modify: `src/bin/systemd_ops.rs` (USAGE :74, dispatch :854-868)
- Test: `src/automation.rs` `#[cfg(test)] mod tests`, next to
  `structured_checkpoint_is_atomic_and_bounded`

**Verification (anti-gameable):** the three new tests below fail
before the change (the field does not exist) and pass after;
`cargo test --lib automation::` is green;
`rg -n 'admitted' src/bin/systemd_ops.rs` shows the USAGE flag and
the dispatch loop. A builder cannot pass by hardcoding: the legacy
bytes test loads a checkpoint file written without the field.

- [x] **Step 1: Write the failing tests**

```rust
    #[test]
    fn checkpoint_admitted_round_trips_and_validates() {
        let root = tmp_root("checkpoint-admitted");
        with_bound_operation(&root, "managed-child", || {
            write_parent_fixture(&root, "managed-child", None);
            let admitted = vec![
                "76643a6871e1a17c4a199564df95e73490b1060a".to_string(),
                "a94bf2596c9533e313250b9e79680da3de25187e28ee79e7bf43d88a7571020d".to_string(),
            ];
            let written = write_checkpoint(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "fp-a",
                Some("gen-a"),
                Some("out-a"),
                &admitted,
            )
            .unwrap();
            assert_eq!(
                written["checkpoint"]["admitted"],
                serde_json::json!(admitted)
            );
            let loaded = load_checkpoint(&root, "managed-child").unwrap().unwrap();
            assert_eq!(loaded.admitted, admitted);
        });
    }

    #[test]
    fn checkpoint_admitted_rejects_non_lowercase_hex() {
        for bad in [
            "76643a68", // too short
            "76643A6871e1a17c4a199564df95e73490b1060a", // uppercase
            "zz643a6871e1a17c4a199564df95e73490b1060a", // non-hex
        ] {
            assert!(validate_checkpoint(Checkpoint {
                version: 2,
                input_fingerprint: "fp-a".to_string(),
                generation: None,
                output_revision: None,
                admitted: vec![bad.to_string()],
                checkpointed_at: "2026-09-22T00:00:00Z".to_string(),
            })
            .is_err());
        }
    }

    #[test]
    fn checkpoint_without_admitted_loads_with_empty_list() {
        let root = tmp_root("checkpoint-legacy");
        let path = checkpoint_path(&root, "managed-child");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"version":2,"input_fingerprint":"fp-old","generation":"gen-a","output_revision":"out-a","checkpointed_at":"2026-09-17T00:00:00Z"}"#,
        )
        .unwrap();
        let loaded = load_checkpoint(&root, "managed-child").unwrap().unwrap();
        assert!(loaded.admitted.is_empty());
        let projected = checkpoint_json(Some(&loaded));
        assert_eq!(projected["admitted"], serde_json::json!([]));
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib automation:: checkpoint`
Expected: FAIL to compile (`no field `admitted` ...` and
`this function takes 5 arguments`).

- [x] **Step 3: Implement**

`Checkpoint` gains the field:

```rust
    #[serde(default)]
    pub output_revision: Option<String>,
    #[serde(default)]
    pub admitted: Vec<String>,
    pub checkpointed_at: String,
```

`validate_checkpoint` gains, after the `output_revision` check
(reuse the existing `lowercase_hex` helper at :1950):

```rust
    for identity in &checkpoint.admitted {
        match identity.len() {
            40 | 64 => {
                lowercase_hex("checkpoint admitted", identity, identity.len())?;
            }
            _ => {
                return Err(BackendError(
                    "checkpoint admitted entries must be 40 or 64 lowercase hex".into(),
                ));
            }
        }
    }
```

`write_checkpoint` gains a last parameter `admitted: &[String]` and
rebuilds the literal with the field:

```rust
    let checkpoint = validate_checkpoint(Checkpoint {
        version: 2,
        input_fingerprint: input_fingerprint.to_string(),
        generation: generation.map(str::to_string),
        output_revision: output_revision.map(str::to_string),
        admitted: admitted.to_vec(),
        checkpointed_at: crate::config::unix_to_rfc3339(crate::config::now_unix()),
    })?;
```

`checkpoint_json` gains `"admitted": checkpoint.admitted` in the
`Some` arm and `"admitted": []` in the `None` arm. The relations
`checkpoint_record` gains:

```rust
            admitted: checkpoint
                .get("admitted")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
```

`child_revision` digests the set inside `if let Some(checkpoint)`:

```rust
        for identity in &checkpoint.admitted {
            buf.extend_from_slice(identity.as_bytes());
            buf.push(0);
        }
```

Fix every other `write_checkpoint` caller (the existing test
`structured_checkpoint_is_atomic_and_bounded` passes `&[]`).

`src/bin/systemd_ops.rs` USAGE :74 becomes:

```
  automation checkpoint --input-fingerprint HASH [--generation VALUE] [--output-revision VALUE] [--admitted HEX]...
```

and the dispatch collects the repeatable flag:

```rust
        "checkpoint" => {
            let input_fingerprint = require_opt(args, "--input-fingerprint")
                .or_else(|_| require_opt(args, "--fingerprint"))
                .map_err(BackendError)?;
            let generation = take_opt(args, "--generation").map_err(BackendError)?;
            let output_revision = take_opt(args, "--output-revision").map_err(BackendError)?;
            let mut admitted = Vec::new();
            while let Some(value) = take_opt(args, "--admitted").map_err(BackendError)? {
                admitted.push(value);
            }
            remaining_flags(args).map_err(BackendError)?;
            automation::write_checkpoint(
                scope_root,
                cwd,
                &input_fingerprint,
                generation.as_deref(),
                output_revision.as_deref(),
                &admitted,
            )
        }
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib automation::`
Expected: PASS, including the three new test names.

- [x] **Step 5: Commit**

```bash
git add src/automation.rs src/bin/systemd_ops.rs
git commit -m "feat: checkpoint carries the admitted identity set"
```

---

### Task 12: READY checkpoint names the admitted identities

**Owner:** builder
**Depends:** Task 11 (the `--admitted` flag must exist)
**Files:**
- Modify: `dogfood/lib/automation-wrapper` (`write_output_checkpoint`
  :44-53)
- Create: `tests/capability-bootstrap.sh` (checkpoint section only;
  Tasks 13-14 extend the same file)
- Modify: `docs/TESTING.md` (name the new proof in Fast gates)
- Modify: `.github/workflows/ci.yml` (Dogfood proofs step)

**Verification (anti-gameable):** `bash
tests/capability-bootstrap.sh` fails before the wrapper change (the
captured args never contain `--admitted`) and passes after. The
proof stubs `ops()` and reads the real
`$STATE_DIR/desired-inputs.json`, so a builder cannot pass by
echoing the expected string.

- [x] **Step 1: Write the failing proof**

```bash
#!/usr/bin/env bash
# First-capability bootstrap proofs (spec D1a): absent fork/<branch>
# remote ref, desired-inputs authority, READY checkpoint admitted
# set, integrator admitted-coverage gate. Fixture repos only.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIB=${WRAPPER_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/automation-wrapper}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export PROOF_SKIP_GH=1

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq required"
[[ -r $LIB ]] || fail "missing $LIB"

# shellcheck source=/dev/null
source "$LIB"

# --- write_output_checkpoint names the admitted set ---
SCOPE_ROOT=$TMP/scope
STATE_DIR=$SCOPE_ROOT/state
mkdir -p "$STATE_DIR"
captured=$TMP/captured-args
ops() { printf '%s
' "$*" >"$captured"; }

commit_id=76643a6871e1a17c4a199564df95e73490b1060a
delta_id=a94bf2596c9533e313250b9e79680da3de25187e28ee79e7bf43d88a7571020d
cat >"$STATE_DIR/desired-inputs.json" <<EOF
{"version":1,"inputs":[
  {"originating_work":"omp-vuz.1","artifact_kind":"commit","artifact_revision":"$commit_id","artifact_identity":"$commit_id","admitted_at":"2026-09-22T06:30:18Z"},
  {"originating_work":"w2","artifact_kind":"worktree-delta","base":"$commit_id","delta":[{"path":"p","sha256":"$delta_id"}],"artifact_identity":"$delta_id","admitted_at":"2026-09-22T06:30:18Z"}
]}
EOF
write_output_checkpoint fp-a gen-a out-a
grep -q -- "--admitted $commit_id" "$captured"   || fail "commit identity missing from checkpoint args"
grep -q -- "--admitted $delta_id" "$captured"   || fail "delta identity missing from checkpoint args"

rm "$STATE_DIR/desired-inputs.json"
: >"$captured"
write_output_checkpoint fp-a gen-a out-a
grep -q -- "--admitted" "$captured"   && fail "admitted flag emitted without desired-inputs"

echo "capability-bootstrap ok"
```

- [x] **Step 2: Run the proof to verify it fails**

Run: `bash tests/capability-bootstrap.sh`
Expected: FAIL with "commit identity missing from checkpoint args".

- [x] **Step 3: Implement**

```bash
write_output_checkpoint() {
  local input_fingerprint=$1
  local generation=$2
  local output_revision=$3
  local args=(automation checkpoint --input-fingerprint "$input_fingerprint")
  [[ -n "$generation" ]] && args+=(--generation "$generation")
  [[ -n "$output_revision" ]] && args+=(--output-revision "$output_revision")
  # A READY checkpoint certifies consume: name every admitted identity
  # the pass was gated on, so the integrator observes what this READY
  # covers (spec D1a/D2). Absent file means nothing was admitted.
  local desired="$STATE_DIR/desired-inputs.json" identity
  if [[ -r "$desired" ]]; then
    while IFS= read -r identity; do
      [[ -n "$identity" ]] && args+=(--admitted "$identity")
    done < <(jq -r '.inputs[]? | .artifact_identity // empty' "$desired")
  fi
  ops "${args[@]}" >/dev/null
}
```

- [x] **Step 4: Run the proof to verify it passes**

Run: `bash tests/capability-bootstrap.sh`
Expected: PASS, prints "capability-bootstrap ok".

- [x] **Step 5: Name the proof and commit**

Add `tests/capability-bootstrap.sh` to the Fast gates list in
`docs/TESTING.md` and to the Dogfood proofs step in
`.github/workflows/ci.yml`, next to `tests/admission.sh`.

```bash
git add dogfood/lib/automation-wrapper tests/capability-bootstrap.sh docs/TESTING.md .github/workflows/ci.yml
git commit -m "feat: READY checkpoint names the admitted identities"
```

---

### Task 13: First-capability bootstrap when fork/cap/<id> is absent

**Owner:** builder
**Depends:** Task 12 (shares `dogfood/lib/automation-wrapper` and
the proof file; serialize)
**Files:**
- Modify: `dogfood/lib/automation-wrapper` (`ensure_clean_worktree`
  :602-631, new `capability_bootstrap_local_branch` next to it)
- Modify: `dogfood/drivers/capability-observe` (:23)
- Modify: `src/automation.rs:37-48` (`BLOCKER_KINDS` gains
  `artifact-unresolvable`; without it the real `write_blocker`
  rejects the kind the helper records)
- Test: `src/automation.rs` `#[cfg(test)] mod tests`, next to
  `native_provisioning_is_a_known_blocker_kind`
- Modify: `tests/capability-bootstrap.sh` (append the sections
  below, before the final echo)

**Verification (anti-gameable):** the appended proof sections fail
before the change (today `ensure_clean_worktree` returns 128 on the
absent ref and `capability-observe` exits nonzero) and pass after.
The proof uses real fixture git repos and a real bare `fork` remote
that lacks the branch, so a builder cannot pass without changing
the fetch-fail path. Two rerun cases lock the idempotency
predicate: a pre-consume rerun (in-progress commit on top of the
baseline, admitted revision still not an ancestor, no checkpoint)
must not reset, and a fingerprint-only `checkpoint.json` that does
not name the admitted identity must not preserve the pre-admission
tip. The Rust unit test fails before the change with "unknown
blocker kind" from the real `write_blocker`, so the
`artifact-unresolvable` kind cannot be faked by the stubbed
`ops()` in the shell proof.

- [x] **Step 1: Write the failing test and proof sections**

Rust unit test, next to `native_provisioning_is_a_known_blocker_kind`:

```rust
    #[test]
    fn artifact_unresolvable_is_a_known_blocker_kind() {
        let root = tmp_root("artifact-unresolvable-kind");
        with_bound_operation(&root, "managed-child", || {
            write_parent_fixture(&root, "managed-child", None);
            let written = write_blocker(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "artifact-unresolvable",
                Some("parent"),
                Some("fp-boot"),
                None,
                Some(4),
                "admitted artifact is not present in the local object store",
            )
            .unwrap();
            assert_eq!(written["changed"], true);
            assert_eq!(written["blocker"]["kind"], "artifact-unresolvable");
        });
        let _ = fs::remove_dir_all(root);
    }
```

```bash
# --- bootstrap: absent fork ref, desired-inputs is authority ---
REPO=$TMP/repo
mkdir -p "$REPO"
git -C "$REPO" init -q
git -C "$REPO" config user.email proof@example.invalid
git -C "$REPO" config user.name proof
git -C "$REPO" checkout -q -b main
printf 'base
' >"$REPO/file"
git -C "$REPO" add file
git -C "$REPO" commit -qm base
baseline=$(git -C "$REPO" rev-parse HEAD)
# admitted artifact: a commit that exists locally but is not on main
git -C "$REPO" checkout -q -b feat/probe
printf 'artifact
' >"$REPO/artifact"
git -C "$REPO" add artifact
git -C "$REPO" commit -qm artifact
admitted=$(git -C "$REPO" rev-parse HEAD)
# pre-admission local tip (livu line): diverges from the baseline
git -C "$REPO" checkout -q -b cap/probe main
printf 'livu
' >"$REPO/livu"
git -C "$REPO" add livu
git -C "$REPO" commit -qm livu-tip
# fork remote without the capability branch
git init -q --bare "$TMP/fork.git"
git -C "$REPO" remote add fork "$TMP/fork.git"
git -C "$REPO" push -q fork main

WORKTREE=$REPO
STATE_DIR=$TMP/boot-state
mkdir -p "$STATE_DIR"
cat >"$STATE_DIR/desired-inputs.json" <<EOF
{"version":1,"inputs":[{"originating_work":"omp-vuz.1","artifact_kind":"commit","artifact_revision":"$admitted","artifact_identity":"$admitted","admitted_at":"2026-09-22T06:30:18Z"}]}
EOF
UPSTREAM_GENERATION=$baseline
OPERATION_STEM=managed-omp-cap-probe
provision_native_addon() { return 0; }
provision_node_modules() { return 0; }

ensure_clean_worktree cap/probe || fail "first bootstrap run failed"
[[ $(git -C "$REPO" rev-parse HEAD) == "$baseline" ]]   || fail "branch was not reset to the baseline"
git -C "$REPO" for-each-ref --format='%(refname)' refs/heads/archive/   | grep -q 'cap/probe-pre-bootstrap-'   || fail "pre-bootstrap tip was not archived"
[[ $(git -C "$REPO" symbolic-ref --short HEAD) == cap/probe ]]   || fail "worktree left the cap/probe branch"
[[ -r $STATE_DIR/bootstrap.json ]]   || fail "bootstrap marker was not written"

# pre-consume rerun: the admitted revision is still not an ancestor
# and no checkpoint exists, yet in-progress reconciliation on top of
# the baseline must survive (spec D1a step 3, bootstrap.json marker)
printf 'wip\n' >"$REPO/wip"
git -C "$REPO" add wip
git -C "$REPO" commit -qm wip-toward-consume
wip_head=$(git -C "$REPO" rev-parse HEAD)
ensure_clean_worktree cap/probe || fail "pre-consume rerun failed"
[[ $(git -C "$REPO" rev-parse HEAD) == "$wip_head" ]]   || fail "pre-consume rerun reset in-progress work"

# a checkpoint that does not name the admitted identity is not
# bootstrap evidence and must not preserve a pre-admission tip
git -C "$REPO" checkout -q -b cap/probe2 main
printf 'livu2\n' >"$REPO/livu2"
git -C "$REPO" add livu2
git -C "$REPO" commit -qm livu-tip-2
STATE2=$TMP/boot-state2
mkdir -p "$STATE2"
cp "$STATE_DIR/desired-inputs.json" "$STATE2/desired-inputs.json"
printf '{"version":2,"input_fingerprint":"fp-old","checkpointed_at":"2026-09-22T00:00:00Z"}\n' >"$STATE2/checkpoint.json"
saved_state=$STATE_DIR
STATE_DIR=$STATE2
ensure_clean_worktree cap/probe2 || fail "fingerprint-only checkpoint run failed"
STATE_DIR=$saved_state
[[ $(git -C "$REPO" rev-parse cap/probe2) == "$baseline" ]]   || fail "fingerprint-only checkpoint preserved the pre-admission tip"
git -C "$REPO" for-each-ref --format='%(refname)' refs/heads/archive/   | grep -q 'cap/probe2-pre-bootstrap-'   || fail "cap/probe2 tip was not archived"
[[ -r $STATE2/bootstrap.json ]]   || fail "bootstrap marker was not written for cap/probe2"
git -C "$REPO" checkout -q cap/probe

# idempotent rerun: once an admitted revision is an ancestor of
# HEAD, the branch is never reset again
git -C "$REPO" merge -q --no-edit "$admitted" >/dev/null
ensure_clean_worktree cap/probe || fail "idempotent rerun failed"
[[ $(git -C "$REPO" rev-parse HEAD) != "$baseline" ]]   || fail "rerun reset the branch again"

# no desired-inputs: no bootstrap authority, fetch failure preserved
git -C "$REPO" checkout -q -b cap/orphan main
rm "$STATE_DIR/desired-inputs.json"
set +e
ensure_clean_worktree cap/orphan
code=$?
set -e
[[ $code -ne 0 && $code -ne 4 ]]   || fail "no-authority case exited $code, want the fetch error"
[[ $(git -C "$REPO" rev-parse cap/orphan) == "$baseline" ]]   || fail "no-authority case mutated the branch"

# unresolvable admitted artifact: typed blocker, never git-128
git -C "$REPO" checkout -q -b cap/gone main
missing=1111111111111111111111111111111111111111
cat >"$STATE_DIR/desired-inputs.json" <<EOF
{"version":1,"inputs":[{"originating_work":"w9","artifact_kind":"commit","artifact_revision":"$missing","artifact_identity":"$missing","admitted_at":"2026-09-22T06:30:18Z"}]}
EOF
blocker_kind=$TMP/blocker-kind
ops() {
  if [[ ${1:-} == automation && ${2:-} == blocker ]]; then
    shift 2
    while (($#)); do
      [[ $1 == --kind ]] && printf '%s
' "$2" >"$blocker_kind"
      shift
    done
    echo '{"ok":true,"data":{"changed":true}}'
    return 0
  fi
  echo '{"ok":true}'
}
set +e
ensure_clean_worktree cap/gone
code=$?
set -e
[[ $code -eq 4 ]] || fail "unresolvable artifact exited $code, want 4"
[[ $(cat "$blocker_kind" 2>/dev/null) == artifact-unresolvable ]]   || fail "blocker kind '$(cat "$blocker_kind" 2>/dev/null)', want artifact-unresolvable"

# --- capability-observe tolerates an absent fork ref ---
OBSERVE=${DOGFOOD_ROOT:-$ROOT/dogfood}/drivers/capability-observe
mkdir -p "$SCOPE_ROOT/.systemd-ops/operations/managed-omp-release-watch/state"
printf '{"target_commit":"%s"}
' "$baseline"   >"$SCOPE_ROOT/.systemd-ops/operations/managed-omp-release-watch/state/generation.json"
git -C "$REPO" checkout -q cap/probe
SYSTEMD_OPS_CWD=$REPO SYSTEMD_OPS_SCOPE_ROOT=$SCOPE_ROOT   "$OBSERVE" cap/probe >"$TMP/obs.json"   || fail "capability-observe failed with an absent fork ref"
jq -e '.version == 1 and (.world_fingerprint | length == 64)'   "$TMP/obs.json" >/dev/null   || fail "capability-observe did not emit a world fingerprint"
# the absent ref hashes as an empty remote_head: recompute the world
# fingerprint with remote_head="" and require byte equality
obs_dirty=$(git -C "$REPO" status --porcelain=v1 --untracked-files=normal | sha256sum | cut -d' ' -f1)
obs_head=$(git -C "$REPO" rev-parse HEAD)
obs_want=$(jq -cn   --arg branch cap/probe   --arg target "$baseline"   --arg local_head "$obs_head"   --arg remote_head ""   --arg dirty "$obs_dirty"   '{branch:$branch,target:$target,local_head:$local_head,remote_head:$remote_head,dirty:$dirty}'   | sha256sum | cut -d' ' -f1)
jq -e --arg want "$obs_want"   '.world_fingerprint == $want'   "$TMP/obs.json" >/dev/null   || fail "absent fork ref did not hash as an empty remote_head"
git -C "$REPO" push -q fork cap/probe
git -C "$REPO" fetch -q fork cap/probe
SYSTEMD_OPS_CWD=$REPO SYSTEMD_OPS_SCOPE_ROOT=$SCOPE_ROOT   "$OBSERVE" cap/probe >/dev/null   || fail "capability-observe failed with a present fork ref"
```

- [x] **Step 2: Run the test and proof to verify they fail**

Run: `cargo test --lib automation:: artifact_unresolvable`
Expected: FAIL - the real `write_blocker` returns "unknown blocker
kind 'artifact-unresolvable'" and the `.unwrap()` panics.
Run: `bash tests/capability-bootstrap.sh`
Expected: FAIL with "first bootstrap run failed" (today the absent
remote ref exits 128).

- [x] **Step 3: Implement**

`ensure_clean_worktree` keeps the success path byte-identical and
branches on failure:

```bash
ensure_clean_worktree() {
  local expected_branch=$1
  local kind summary
  if kind_summary=$(worktree_invariant_kind "$expected_branch"); then
    kind=$(printf '%s
' "$kind_summary" | sed -n '1p')
    summary=$(printf '%s
' "$kind_summary" | sed -n '2p')
    recover_worktree "$expected_branch" "$kind" "$summary" || return 4
  fi
  local fetch_rc=0
  git -C "$WORKTREE" fetch --quiet fork "$expected_branch" || fetch_rc=$?
  if [[ $fetch_rc -ne 0 ]]; then
    # A resolvable remote-tracking ref means the fetch failure is
    # transient; return it unchanged. An absent ref means fork never
    # received this branch: push stays the operator's gate, and a
    # capability with admitted desired inputs bootstraps locally
    # (spec D1a).
    if git -C "$WORKTREE" rev-parse --verify -q "fork/$expected_branch" >/dev/null 2>&1; then
      return "$fetch_rc"
    fi
    capability_bootstrap_local_branch "$expected_branch" "$fetch_rc" || return $?
  else
    local local_head remote_head
    local_head=$(git -C "$WORKTREE" rev-parse HEAD) || return $?
    remote_head=$(git -C "$WORKTREE" rev-parse "fork/$expected_branch") || return $?
    if [[ "$local_head" != "$remote_head" ]]; then
      if git -C "$WORKTREE" merge-base --is-ancestor "$local_head" "$remote_head"; then
        git -C "$WORKTREE" merge --ff-only "fork/$expected_branch" || return $?
      else
        recover_worktree "$expected_branch" worktree-diverged           "dedicated worktree diverged from fork/$expected_branch" || return 4
      fi
    fi
  fi
  if ! provision_native_addon; then
    echo "native provisioning failed for $WORKTREE" >&2
    record_blocker native-provisioning 4 "native addon missing or export-deficient in $WORKTREE" "" parent || true
    return 4
  fi
  if ! provision_node_modules; then
    echo "node_modules provisioning failed for $WORKTREE" >&2
    record_blocker native-provisioning 4 "node_modules missing in $WORKTREE" "" parent || true
    return 4
  fi
}

# First-capability bootstrap (spec D1a). Runs only when
# fork/$expected_branch does not exist. desired-inputs.json is the
# authority for what the capability must contain; the absent remote
# ref and any pre-admission local tip are not. Never pushes.
# Idempotent: once any admitted commit is an ancestor of HEAD, a
# structured checkpoint names every admitted identity, or this
# helper's own bootstrap.json marker exists, the branch is never
# reset again. A checkpoint that does not name the admitted set is
# not evidence and does not preserve a pre-admission tip.
# $2 is the fetch exit code, preserved when there is no bootstrap
# authority.
capability_bootstrap_local_branch() {
  local expected_branch=$1
  local no_authority_rc=${2:-1}
  local desired="$STATE_DIR/desired-inputs.json"
  if [[ ! -r $desired ]] \
    || ! jq -e '.version == 1 and (.inputs | type == "array" and length > 0)' "$desired" >/dev/null 2>&1; then
    echo "fork/$expected_branch is absent and $desired names no admitted input; no bootstrap authority" >&2
    return "$no_authority_rc"
  fi
  local rev
  while IFS= read -r rev; do
    [[ -n $rev ]] || continue
    if ! git -C "$WORKTREE" cat-file -e "$rev^{commit}" 2>/dev/null; then
      echo "admitted artifact $rev does not resolve in the local object store" >&2
      record_blocker artifact-unresolvable 4 \
        "admitted artifact $rev is not present in the local object store" "" parent || true
      return 4
    fi
  done < <(jq -r '.inputs[] | .artifact_revision // empty' "$desired")
  while IFS= read -r rev; do
    [[ -n $rev ]] || continue
    if git -C "$WORKTREE" merge-base --is-ancestor "$rev" HEAD 2>/dev/null; then
      return 0
    fi
  done < <(jq -r '.inputs[] | .artifact_revision // empty' "$desired")
  if jq -n -e --slurpfile cp "$STATE_DIR/checkpoint.json" --slurpfile di "$desired" '
      ($cp[0].input_fingerprint != null)
      and ([$di[0].inputs[].artifact_identity] as $want
        | ([$cp[0].admitted[]?]) as $have
        | ($want | all(. as $id | ($have | index($id)) != null)))
    ' >/dev/null 2>&1; then
    return 0
  fi
  if [[ -r $STATE_DIR/bootstrap.json ]]; then
    return 0
  fi
  local baseline old_head stamp archived_ref
  baseline=$(resolve_target_generation) || return $?
  if ! git -C "$WORKTREE" cat-file -e "$baseline^{commit}" 2>/dev/null; then
    echo "baseline $baseline does not resolve in the local object store" >&2
    return 4
  fi
  old_head=$(git -C "$WORKTREE" rev-parse HEAD) || return $?
  archived_ref=
  if [[ $old_head != "$baseline" ]]; then
    stamp=$(date -u +%Y%m%dT%H%M%SZ)
    archived_ref="archive/$expected_branch-pre-bootstrap-$stamp"
    git -C "$WORKTREE" branch -f "$archived_ref" "$old_head" || return $?
    git -C "$WORKTREE" reset --hard "$baseline" || return $?
  fi
  jq -n --arg baseline "$baseline" --arg ref "$archived_ref" \
    --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    '{version:1,baseline:$baseline,archived_ref:$ref,bootstrapped_at:$at}' \
    >"$STATE_DIR/bootstrap.json" || return $?
  return 0
}
```

`src/automation.rs` `BLOCKER_KINDS` (:37-48) gains the kind, so the
`record_blocker artifact-unresolvable ...` call above survives the
real `write_blocker` validation:

```rust
const BLOCKER_KINDS: &[&str] = &[
    "iteration-failed",
    "worktree-dirty",
    "worktree-diverged",
    "worktree-detached",
    "worktree-missing",
    "native-provisioning",
    "artifact-unresolvable",
    "contract-failure",
    "stale-generation",
    "postcondition-failed",
    "semantic-blocked",
];
```

`dogfood/drivers/capability-observe` :23 becomes:

```bash
# fork/$BRANCH may not exist: push is the operator's gate and a new
# capability bootstraps locally with desired-inputs as authority
# (spec D1a). An absent remote ref hashes as empty, not as an
# observer failure.
remote_head=$(git -C "$WORKTREE" rev-parse --verify -q "fork/$BRANCH" || true)
```

- [x] **Step 4: Run the test and proofs to verify they pass**

Run: `cargo test --lib automation::`
Expected: PASS, including `artifact_unresolvable_is_a_known_blocker_kind`.
Run: `bash tests/capability-bootstrap.sh`
Expected: PASS, prints "capability-bootstrap ok".
Also run: `bash tests/native-provisioning.sh` (the
`ensure_clean_worktree` success path must be unchanged).
Expected: PASS, prints "native-provisioning ok".

- [x] **Step 5: Commit**

```bash
git add src/automation.rs dogfood/lib/automation-wrapper dogfood/drivers/capability-observe tests/capability-bootstrap.sh
git commit -m "feat: first-capability bootstrap from desired-inputs when fork/cap/* is absent"
```

---

### Task 14: Integrator gate requires admitted coverage

**Owner:** builder
**Depends:** Task 11 (the `admitted` projection) and Task 12
(shares the proof file)
**Files:**
- Modify: `dogfood/lib/automation-wrapper`
  (`capability_sources_ready_for_generation` :150-176)
- Modify: `tests/capability-bootstrap.sh` (append the section
  below, before the final echo)

**Verification (anti-gameable):** the appended section fails before
the change (a stale checkpoint with an empty `admitted` is treated
ready today) and passes after. The proof feeds fixture context JSON
and a real on-disk desired-inputs file, so a builder cannot pass by
always returning ready. `bash tests/runtime-wave.sh` (set -euo
pipefail, never sets `SCOPE_ROOT`, CI caller of this gate) is the
regression that the coverage loop degrades to the old jq predicate
when no scope root is in play.

- [x] **Step 1: Write the failing proof section**

```bash
# --- integrator admitted-coverage gate ---
GEN=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
OUT=cccccccccccccccccccccccccccccccccccccccc
CAPUNIT=managed-omp-cap-probe
mkdir -p "$SCOPE_ROOT/.systemd-ops/operations/$CAPUNIT/state"
cat >"$SCOPE_ROOT/.systemd-ops/operations/$CAPUNIT/state/desired-inputs.json" <<EOF
{"version":1,"inputs":[{"originating_work":"omp-vuz.1","artifact_kind":"commit","artifact_revision":"$admitted","artifact_identity":"$admitted","admitted_at":"2026-09-22T06:30:18Z"}]}
EOF
ctx_for() {
  jq -n --arg gen "$1" --arg out "$2" --argjson admitted "$3" '{
    data: {relations: {children: [{
      unit: "managed-omp-cap-probe",
      lifecycle: "active",
      agent: "capability-maintainer",
      running: false,
      active_iteration: false,
      blocker: null,
      checkpoint: {present: true, kind: "structured", generation: $gen, output_revision: $out, admitted: $admitted}
    }]}}
  }'
}
ctx=$(ctx_for "$GEN" "$OUT" "[\"$admitted\"]")
capability_sources_ready_for_generation "$ctx" "$GEN"   || fail "covered admitted set was not ready"
ctx=$(ctx_for "$GEN" "$OUT" '[]')
capability_sources_ready_for_generation "$ctx" "$GEN"   && fail "stale checkpoint without admitted coverage was ready"
rm "$SCOPE_ROOT/.systemd-ops/operations/$CAPUNIT/state/desired-inputs.json"
capability_sources_ready_for_generation "$ctx" "$GEN"   || fail "child without desired-inputs was held back"
```

- [x] **Step 2: Run the proof to verify it fails**

Run: `bash tests/capability-bootstrap.sh`
Expected: FAIL with "stale checkpoint without admitted coverage was
ready".

- [x] **Step 3: Implement**

```bash
capability_sources_ready_for_generation() {
  local context=$1
  local required_generation=$2
  local tmp
  tmp=$(mktemp)
  printf '%s
' "$context" >"$tmp"
  jq -e --arg gen "$required_generation" '
    [.data.relations.children[]?
      | select((.lifecycle // "active") != "completed")
      | select((.agent // "") == "capability-maintainer")]
    | (length > 0)
      and all(
        ((.blocker.current // false) != true)
        and ((.running // false) != true)
        and ((.active_iteration // false) != true)
        and (.checkpoint.present == true)
        and (.checkpoint.kind == "structured")
        and ((.checkpoint.generation // "") == $gen)
        and ((.checkpoint.output_revision // "") != "")
      )
  ' "$tmp" >/dev/null || { rm -f "$tmp"; return 1; }
  # Admitted-coverage (spec D1a/D2): a READY checkpoint certifies
  # consume only when it names every identity the child currently
  # desires. A checkpoint from before a new admit does not certify.
  # SCOPE_ROOT may be unset: tests/runtime-wave.sh runs this gate
  # under set -euo pipefail without one. No scope root means no
  # desired-inputs files are in play, exactly as for a child whose
  # desired-inputs file is absent, so the loop is skipped.
  if [[ -n ${SCOPE_ROOT:-} ]]; then
    local unit desired
    while IFS= read -r unit; do
      [[ -n $unit ]] || continue
      desired="$SCOPE_ROOT/.systemd-ops/operations/$unit/state/desired-inputs.json"
      [[ -r $desired ]] || continue
      jq -e --arg unit "$unit" '
        [.inputs[].artifact_identity] as $want
        | ([input.data.relations.children[]?
            | select(.unit == $unit)
            | (.checkpoint.admitted // [])[]]) as $have
        | ([$want[] | . as $identity | ($have | index($identity)) != null] | all)
      ' "$desired" "$tmp" >/dev/null || { rm -f "$tmp"; return 1; }
    done < <(jq -r '.data.relations.children[]?
      | select((.lifecycle // "active") != "completed")
      | select((.agent // "") == "capability-maintainer")
      | .unit' "$tmp")
  fi
  rm -f "$tmp"
  return 0
}
```

- [x] **Step 4: Run the proofs to verify they pass**

Run: `bash tests/capability-bootstrap.sh`
Expected: PASS, prints "capability-bootstrap ok".
Also run: `bash tests/runtime-wave.sh`
Expected: PASS, prints "runtime-wave ok". This is the regression
command for the gate itself: that proof is `set -euo pipefail`,
never sets `SCOPE_ROOT`, and calls
`capability_sources_ready_for_generation` directly, so the seven-READY
case dies with "SCOPE_ROOT: unbound variable" if the guard is
missing. (`tests/capability-readiness.sh` exercises
`children_ready_for_generation`, a different function, and cannot
catch this.)

- [x] **Step 5: Commit**

```bash
git add dogfood/lib/automation-wrapper tests/capability-bootstrap.sh
git commit -m "feat: integrator composes only on checkpoints that cover desired-inputs"
```

---

---

## Order and parallelism

```
Task 1
  +- Task 2 -> Task 3
  +- Task 4 -> Task 5 -> Task 6
Task 7 -> Task 8          (serialize systemd_ops.rs with 2 if concurrent)
Task 9 after 2, 5, 6, 7, 8
Task 10 after 2-6 (the `omp-vuz.1` half also after 11-14)
Task 11 -> Task 12 -> Task 13 -> Task 14
```

Tasks 12-14 all touch `dogfood/lib/automation-wrapper` or its proof
file, so they serialize. Task 11 is Rust-only and may run in
parallel with nothing else in flight on `src/automation.rs`.

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
- Live mutations of any kind: no live unit rewrite, no timer start,
  no compose, no retarget of the livu tree
  `/home/sf/worktrees/omp/cap/todo-passive-phase` (the bootstrap
  reset is the maintainer tick's job at runtime, never a builder's
  hand), no `mkdir` of `/home/sf/worktrees/omp/admit/*`
