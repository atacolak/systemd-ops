# systemd-ops 0.7.0 consolidation Technical Spec

**Design Brief:** `/home/sf/worlds/personal/designs/systemd-ops/consolidation-0-7-0.md`
**Status:** sound
**Repo:** `/home/sf/workspace/systemd-ops` (HEAD `aafe91e62226d9000ed46642d4aed12fa43762a5` on `main`)

## Intent (from the Brief — do not rewrite)

> Turn the current dogfooded systemd-ops state into a coherent, tracked, documented, releasable 0.7.0 system, then commit and push the latest intended systemd-ops work. This is a stability-boundary consolidation, not a new architecture.

Success criteria (quoted):

- Canonical tracked source exists for the dogfood logic/proofs that tests currently load from a live OMP copy.
- Tests do not depend on an accidental mutable live copy under `/home/sf/workspace/oh-my-pi/.systemd-ops` where avoidable.
- OMP deployment can remain; the architecture of automation is not redesigned.
- Future PR-maintainer units use a stable project-root cwd. Durable PR identity is the fork branch. Each intelligent pass still uses a disposable attempt worktree.
- The current new PR unit is normalized if that is safe.
- Docs describe current actual systemd-ops semantics. Stale recovered-worktree / manual-runtime-compose instructions are gone.
- Package metadata and installed docs are internally consistent.
- Full verification is green: cargo tests, clippy, fmt, dogfood shell proofs, docs/man/package checks, staged make install or equivalent.
- Latest intended systemd-ops work, including the existing unpushed test commits if they belong to this boundary, is committed and pushed.
- No unrelated dirty state is destroyed.
- No crate/tag/GitHub release is published unless that is already clearly intended and safe. If publication needs operator approval, leave a FOR USER item.

Operator constraints (quoted):

- Do not add new architecture unless a concrete correctness contradiction blocks packaging.
- Do not redesign systemd-ops.
- Do not add publication-event machinery.
- Do not change OMP observe/compose policy without a demonstrated bug.
- Do not create a self-updating systemd-ops loop.
- Do not auto-retire capabilities because their PR merged.
- Do not force-push, reset, or clean unknown worktrees.
- Do not destroy unrelated dirty state.
- Tiny obvious robustness/QoL only. Known candidate: if a managed PR is already MERGED, cheaply complete the PR operation before paying for another model maintenance pass. Do this only if genuinely bounded and clearly safe; otherwise leave a note.
- Parent/child wording must match actual generic core semantics and the way OMP uses the relation. Do not casually redefine core semantics in prose if code says otherwise. If there is a genuine contradiction, surface it before changing behavior.
- Prefer documentation clarification if parent/waiting behavior is already correct. Do not broaden this into a new dependency engine.
- Runtime should be described as automatic composition of accepted Capability checkpoints, not something the project lead manually decides every normal cycle.

In scope (quoted): dogfood tracked source; tests pointed at it; OMP deployment path; PR-maintainer project-root cwd convention; normalize `managed-omp-pr-10922` if safe; reconcile listed docs and agent contracts; tiny MERGED-before-pass QoL only if bounded and safe; verification and 0.7.0 metadata; commit/push; FOR USER if tagging/publishing needs approval.

Out of scope (quoted): new architecture; publication events; observe/compose policy changes without a demonstrated bug; self-updating systemd-ops; auto-retiring capabilities on merge; making systemd-ops itself a managed scope; force-push/reset/cleaning unknown worktrees; publishing crate/tag/GitHub release without a clearly safe existing intent.

## Mapping onto the current system

Claims below are labeled **observed** / **inferred** / **unknown**.

### Git and release metadata (observed)

- `Cargo.toml` `version = "0.7.0"`. Installed `/home/sf/.local/bin/systemd-ops` reports `0.7.0` (mtime 2026-09-01). Latest tag `v0.6.0`. `CHANGELOG.md` header is still `## unreleased (not published)`.
- Unpushed commits on `main`: `2d86613 test(automation): prove PR settle quiet, P3 ignore, and one runtime model`; `aafe91e test(automation): prove disposable PR attempts, parking, and 60m grace`. Both are tests-only. They belong to this boundary.
- Working tree clean except untracked `.ai-bridge/` and `.systemd-ops/scope.toml`. Do not add, delete, or gitignore-absorb those in this pass.
- No `docs/superpowers/` tree existed before this spec. Default spec/plan location applies.

### Dogfood source of truth (observed)

Tests default to live OMP paths (env override exists for most, not all):

| Test | Default source |
|---|---|
| `tests/pr-attempt.sh` | `drivers/pr-run`, `lib/automation-wrapper`; hardcoded `cp` of `lib/pr-attempt` |
| `tests/wrapper-contract.sh` | same, including hardcoded `pr-attempt` copy |
| `tests/capability-release-run.sh` | `drivers/capability-run`, `lib/automation-wrapper`, `lib/pr-settlement`, `lib/pr-review-state` |
| `tests/capability-readiness.sh` | `lib/automation-wrapper` |
| `tests/pr-critical.sh` | `lib/pr-review-state`, `lib/pr-settlement` |
| `tests/pr-settlement.sh` | `lib/pr-settlement` |
| `tests/pr-severity.sh` | `lib/pr-review-state` |
| `tests/runtime-wave.sh` | `lib/automation-wrapper` |

systemd-ops git tracks none of those drivers/libs. Live OMP tree at inspect:

- Drivers: `pr-run`, `pr-observe`, `capability-run`, `capability-observe`, `runtime-run`, `runtime-observe`, `runtime-reconcile`, `omp-release-watch`, `project-lead-watch`, `fork-main-sync`
- Libs used by current proofs: `automation-wrapper`, `pr-attempt`, `pr-settlement`, `pr-review-state`
- Extra live files **not** in the lead inspect set and **not** loaded by current tests: `lib/verify-merged-pr`, `lib/verify-merged-pr.test.sh` (mtime 2026-09-06). These are out of this Brief (capability auto-retire is out of scope). Do not import them. Do not delete them from OMP.

`pr-run` already completes a MERGED PR **after** a successful intelligent pass (`gh pr view` then `automation complete --reason "merged upstream"`). Existing proofs set `PROOF_SKIP_GH=1`, so they never exercise that path.

### PR-maintainer cwd (observed)

- Most live `managed-omp-pr-*` units: `WorkingDirectory=/home/sf/workspace/oh-my-pi` and the same path as `pr-run`'s fourth argv (bound worktree / git base).
- `managed-omp-pr-10922`: `WorkingDirectory` and fourth argv are `/home/sf/worktrees/omp/fix/copy-outline-lazy-read`. Origin comments already name `/home/sf/workspace/oh-my-pi`. Timer exists (`OnUnitInactiveSec=3min`).
- That recovered worktree is **clean** (`git status --porcelain` empty) and **behind** `fork/fix/copy-outline-lazy-read` by 14 commits. Rebinding cwd does not require reset/clean. Leave the recovered tree on disk.
- `lib/pr-attempt` uses bound worktree / `GIT_BASE` / else `SCOPE_ROOT` as the git base; the agent cwd is the disposable attempt. Bound unit cwd is not the agent cwd.

Authoring: `OperationSpec.cwd` becomes `WorkingDirectory=` (`src/operations.rs`). There is no second template engine. Future-unit convention is instruction + `cwd` + `exec.argv` (fourth argument = project-root git base), not a core change.

Capability recovered-worktree paths are not rewritten in this pass.

### Parent / waiting vs OMP Runtime (observed)

Generic core `derive_semantic_states` in `src/automation.rs`:

- Completed children (`automation.lifecycle.status == "completed"`) are filtered out of the active set.
- WAITING iff there is at least one active declared child whose derived semantic state is not `ready`.
- Zero children: vacuously not waiting; an otherwise checkpoint-current agent-backed parent is `ready`.
- `docs/SCOPES.md` already states this: “WAITING is derived from any active declared child that is not semantically READY. Zero children are vacuously satisfied.” Parent is “a producer-consumer relation, not ownership transfer, readiness, or scheduling.”

OMP Runtime (`capability_sources_ready_for_generation` in live `automation-wrapper`) is a **domain composition gate**, not core semantic_state:

- Direct children with `agent == "capability-maintainer"` only, non-completed.
- Requires `length > 0` and all: no current blocker, not running, no active iteration, structured checkpoint for the required generation with a non-empty output revision.
- `tests/runtime-wave.sh` proves six READY + one stale does not compose; seven READY does.

This is not a core mismatch. Do not add a dependency engine. Do not change observe/compose policy. Document the layering.

### Docs / package drift (observed)

- `docs/SCOPES.md` still says “no `owner_agent`, `hcom_agent`, `omp_profile`, `lead`, or `manager` fields.” Code, tests, CHANGELOG, and live OMP `scope.toml` already support optional `[coordination] lead = "hcom:xxxx"` (`src/scope.rs` `validate_coordination_lead`).
- `docs/PACKAGING.md` “Dependencies | serde, serde_json” and “No bundled dependencies: two crates”. `Cargo.toml` also has `toml = "0.9.12"` and `ratatui = "0.29.0"`. README and AGENTS.md already name all four.
- Makefile `DOCS := README.md docs/TOOLS.md docs/DESIGN.md docs/TESTING.md docs/SCOPES.md`. README documentation section also points at `docs/PACKAGING.md`, `SECURITY.md`, `CHANGELOG.md`, `AGENTS.md`. Installed docs must include everything README points packaged users toward.
- CI staged install checks binaries, man, units, licenses; not the doc files.
- `docs/TESTING.md` does not list the dogfood shell proofs. CI does not run them. After tracking, they can run from a fresh clone (they use temp scopes / fake OMP; `capability-release-run.sh` fakes quiet with `PR_SETTLE_NOW`, not a wall-clock 5m sleep).
- Project-lead contract (`/home/sf/worlds/base/agents/project-lead.md`) still says “preserve dirty PR worktrees. recover by creating a clean replacement and rebinding cwd, never by resetting” and wake-inspects “dirty/missing worktrees”. That is the stale recovered-worktree normal path the Brief forbids.
- Runtime-maintainer contract already treats Runtime sources as capability-maintainer children and says the project lead does not write `fork/runtime`. Docs/contracts must state Runtime as automatic composition of accepted Capability checkpoints, not a lead-decided cycle.

### QoL candidate (observed / mapping)

Cheap complete **before** a model pass is bounded and safe if and only if:

- After the existing processed-input short-circuit and wake lock, **before** `iteration-start` and before `create_pr_attempt`.
- `PROOF_SKIP_GH` unset, `GH_BIN` executable (default `/home/sf/.local/bin/gh`), `gh pr view` returns exactly `MERGED`.
- Then the same `automation complete --unit "$OPERATION_STEM" --reason "merged upstream"` already used after a successful pass, then `notify_parent completed`, then exit 0.
- `gh` failure: do not complete; fall through to the existing pass.
- State `CLOSED` or anything other than `MERGED`: do not complete.
- No worktree reset/clean; no capability retire; no merge/close of the GitHub PR.

This satisfies the Brief candidate without a new engine. Include it.

## Architecture

systemd-ops remains the inspect/control/author engine. OMP dogfood wrappers remain bash drivers/libs that shell `systemd-ops --json`. This pass does not merge those into Rust.

Tracked source of truth for those wrappers lives **in the systemd-ops git tree** at `dogfood/drivers/` and `dogfood/lib/`. Tests default to that tree. The crate package continues to exclude live suites and adapters; add `/dogfood` to `Cargo.toml` `exclude` so crates.io stays the buildable engine plus its documentation.

Live OMP `/home/sf/workspace/oh-my-pi/.systemd-ops/{drivers,lib}` remains a **deployment copy**. Units keep executing that copy. Deployment is an explicit copy of the tracked named files onto matching names. No `--delete`. No systemd-ops self-update loop.

PR-maintainer execution:

1. Unit `WorkingDirectory` and `pr-run` fourth argv = **scope project root** (OMP: `/home/sf/workspace/oh-my-pi`), which is the git base.
2. Durable identity = `fork/$HEAD_BRANCH`.
3. Each intelligent pass: disposable attempt worktree via `lib/pr-attempt`; agent `--cwd` is that attempt; attempt is disposed on exit.

Generic semantic_state is unchanged. OMP Runtime composition remains the wrapper gate over capability checkpoints.

## Components and interfaces

### Tracked dogfood tree

Create (copy from live OMP, do not rewrite policy):

```
dogfood/drivers/pr-run
dogfood/drivers/pr-observe
dogfood/drivers/capability-run
dogfood/drivers/capability-observe
dogfood/drivers/runtime-run
dogfood/drivers/runtime-observe
dogfood/drivers/runtime-reconcile
dogfood/drivers/omp-release-watch
dogfood/drivers/project-lead-watch
dogfood/drivers/fork-main-sync
dogfood/lib/automation-wrapper
dogfood/lib/pr-attempt
dogfood/lib/pr-settlement
dogfood/lib/pr-review-state
```

Do not track `verify-merged-pr*`. Do not track OMP `operations/`, `scope.toml`, or state.

Test default pattern (existing env overrides stay):

```bash
DOGFOOD_ROOT=${DOGFOOD_ROOT:-$ROOT/dogfood}
SOURCE_DRIVER=${SOURCE_DRIVER:-$DOGFOOD_ROOT/drivers/pr-run}
# similarly for SOURCE_LIB / WRAPPER_LIB / SETTLE_LIB / REVIEW_LIB
cp "$DOGFOOD_ROOT/lib/pr-attempt" "$scope/.systemd-ops/lib/pr-attempt"
```

Hardcoded `/home/sf/workspace/oh-my-pi/.systemd-ops/...` paths in `tests/` must go.

New proof `tests/dogfood-source.sh`: tracked files exist; `tests/*.sh` does not mention the live OMP `.systemd-ops` path.

### Deployment path

A one-shot copy (script or documented commands) from `dogfood/{drivers,lib}/<name>` to `$OMP_ROOT/.systemd-ops/{drivers,lib}/<name>` with `OMP_ROOT` default `/home/sf/workspace/oh-my-pi`. Copy only tracked names. Never `rsync --delete`. Never install a timer that copies systemd-ops into itself.

After MERGED-before-pass lands in tracked `pr-run`, deploy that file so live units match the tracked source. Live ExecStart paths stay under the OMP tree.

### PR unit convention

For new and updated PR-maintainer authoring (`automation plan-create` / `plan-update` via existing OperationSpec):

- `cwd` = scope root (project root), not a `fix/*` recovered worktree.
- `exec.argv` fourth argument = that same project root (bound git base for `pr-run`).
- Do not change `exec.path` away from the deployed OMP driver in this pass.

Normalize `managed-omp-pr-10922` with `inspect get-operation` → edit `editable_spec.cwd` and `editable_spec.exec.argv` last element → `automation plan-update` → `automation apply`. Leave `/home/sf/worktrees/omp/fix/copy-outline-lazy-read` on disk. Do not start, stop, reset, or complete the unit as part of cwd normalization. If the oneshot is running, wait until inactive before apply.

### MERGED-before-pass (tracked `dogfood/drivers/pr-run`)

Insert after the post-lock processed-input short-circuit and before `resolve_target_generation` / `iteration-start` / `create_pr_attempt`:

- `GH_BIN=${GH_BIN:-/home/sf/.local/bin/gh}` (tiny testability seam; default remains the live binary).
- Skip when `PROOF_SKIP_GH` is set (existing proofs stay silent).
- If `GH_BIN` is executable and `pr view` JSON `state` is exactly `MERGED`, call existing complete + `notify_parent completed` and exit 0.
- Keep the existing post-success MERGED complete as a backstop for the pass that first observed merge during the model run.

Proof: extend `tests/pr-attempt.sh` with a fake `GH_BIN` that prints `MERGED`, a `SYSTEMD_OPS_BIN` wrapper that records `automation complete` and otherwise delegates, no `PROOF_SKIP_GH`. Expect: no attempt worktree, no fake-OMP invocation, complete recorded, user dirty file preserved.

### Docs and package metadata

Update in place (no new architecture chapters):

- `docs/SCOPES.md`: document `[coordination] lead`; keep “no owner_agent / hcom_agent / omp_profile / manager”; state generic WAITING vs OMP Runtime composition gate; state PR project-root cwd + disposable attempts; Runtime as automatic capability-checkpoint composition.
- `docs/DESIGN.md`: dogfood tracked vs deployed; no recovered-worktree-as-normal-PR-path; no lead-manual runtime compose.
- `docs/PACKAGING.md`: dependencies serde, serde_json, toml; TUI adds ratatui; installed doc list; dogfood is git-only (crate `exclude`).
- `docs/TESTING.md`: dogfood shell proofs and that they default to `dogfood/`.
- `README.md`: documentation list already complete; mention `dogfood/` as git-tracked OMP automation source if needed for clone-and-see.
- `AGENTS.md`: layout row for `dogfood/`.
- `CHANGELOG.md`: promote `## unreleased (not published)` to `## 0.7.0 (not published)` and describe this consolidation. Do not date it as published. Do not tag.
- `Makefile` `DOCS`: add `docs/PACKAGING.md SECURITY.md CHANGELOG.md AGENTS.md`.
- `.github/workflows/ci.yml`: staged install must assert those doc files exist; check job runs dogfood shell proofs after `cargo test` (needs `jq`, git, debug binary).
- Agent contracts: `/home/sf/worlds/base/agents/project-lead.md`; `/home/sf/worlds/automation-agents/.omp/agents/{pr-maintainer,capability-maintainer,runtime-maintainer}.md`.

### Core Rust

No change to `derive_semantic_states`, observation, or compose policy.

## Data / control flow

```
clone systemd-ops
  dogfood/{drivers,lib}     <- canonical scripts
  tests/*                   <- default DOGFOOD_ROOT=$ROOT/dogfood

deploy (explicit)
  dogfood/X  ->  $OMP_ROOT/.systemd-ops/X
  live units ExecStart remain $OMP_ROOT/.systemd-ops/drivers/...

PR wake (deployed pr-run)
  observe -> processed? exit 0
  lock
  MERGED? complete + notify_parent + exit 0
  else iteration-start -> disposable attempt -> omp pr-maintainer
       -> dirty reject / park / checkpoint
       -> if MERGED after pass: complete (backstop)
```

Generic ScopeView WAITING still uses declared non-completed children and READY. Runtime `runtime-run` still calls `capability_sources_ready_for_generation` before composing `fork/runtime`.

## Error handling

- Missing tracked dogfood file: tests fail immediately (`missing $SOURCE_DRIVER`), not by silently using live OMP.
- `gh` missing or non-MERGED: fall through to existing maintenance pass.
- `automation complete` failure on the cheap MERGED path: non-zero exit; do not start a model pass after a failed complete.
- 10922 apply: sealed plan/apply; refuse if token stale. Do not force the recovered worktree.
- Deploy: if a destination file is unexpected, copy still overwrites **tracked names only**. Extra live files (including `verify-merged-pr*`) remain.

## Testing (behavioral contracts; exact tests live in the plan)

- `tests/dogfood-source.sh`: tracked tree present; no live-OMP `.systemd-ops` defaults in `tests/*.sh`.
- Existing dogfood proofs pass with no live OMP tree required (override `DOGFOOD_ROOT` only if proving the override still works; default must be the repo tree).
- New `pr-attempt.sh` MERGED-before-pass case as specified above.
- Rust unit tests for `derive_semantic_states` remain the core waiting contract; do not “fix” them.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `bash tests/docs.sh`.
- Staged `make DESTDIR=... prefix=/usr install` includes the README documentation set.
- `cargo package --locked` excludes `/dogfood` (and existing `/tests`, `/.github`, `/omp`).
- Fresh-clone/CI: dogfood proofs run on GitHub `check` job.
- 10922: `systemctl --user cat` shows project-root cwd and argv; recovered worktree still exists and was not reset.

## Non-goals

- Redesign systemd-ops or a second automation engine.
- Publication-event machinery; crate publish; git tag; GitHub release.
- OMP observe/compose policy changes.
- Self-updating systemd-ops.
- Auto-retire capabilities because a PR merged; do not import `verify-merged-pr`.
- Force-push / reset / clean unknown worktrees; delete historical recovered worktrees.
- Rewrite capability recovered-worktree paths.
- Make systemd-ops itself a managed scope (do not absorb `.systemd-ops/scope.toml`).
- Absorb `.ai-bridge/`.

## Implementation approach chosen (and rejected internals)

**Chosen (Brief approach A):** `dogfood/` at the systemd-ops repo root. Tests consume it. OMP keeps a deployed copy. Crate `exclude` includes `/dogfood`. Changelog is the 0.7.0 unpublished boundary. MERGED-before-pass is included under the bounds above.

Rejected internals that would still satisfy some but not all Brief text:

- Adjacent tree outside systemd-ops: splits the release boundary; fresh-clone proofs fail.
- Leave live OMP as source and only document paths: fails “tests must not depend on a mutable live copy”.
- `omp/dogfood/`: mixes the TypeScript adapter with bash drivers; `omp/` is already a crate-exclude unit with a different job.
- `rsync --delete` deploy: would destroy extra live files and unrelated dirty state.
- Changing core WAITING to the Runtime composition gate: redefines generic semantics; forbidden.
- Skipping 10922 because it is behind 14 commits: behind is not dirty; rebind is the requested normalization.

## Open questions

None. Packaging path, live OMP remaining a deploy target, unpublished 0.7.0 changelog, and MERGED-before-pass inclusion are Brief-allowed planner picks.
