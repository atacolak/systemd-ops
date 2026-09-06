# systemd-ops 0.7.0 consolidation Implementation Plan

**Technical Spec:** `/home/sf/workspace/systemd-ops/docs/superpowers/specs/2026-09-06-consolidation-0-7-0.md`
**Design Brief:** `/home/sf/worlds/personal/designs/systemd-ops/consolidation-0-7-0.md` (via spec; do not bypass)

> **For the project lead:** execute task-by-task with isolated `builder` workers. Do not implement these tasks inline. Persist progress in this file's checkboxes, not only in conversation. After builder tasks, dispatch the `verifier` task. Do not tag, crate-publish, or GitHub-release.

**Goal:** Make OMP dogfood scripts a tracked systemd-ops source of truth, point tests at it, document actual 0.7.0 semantics, normalize PR-maintainer project-root cwd, add bounded MERGED-before-pass completion, then verify and push without publishing.
**Architecture:** Canonical bash drivers/libs live at `dogfood/{drivers,lib}/`. Tests default there. Live OMP `.systemd-ops/{drivers,lib}` stays a deployed copy (named-file copy, never delete). Generic `derive_semantic_states` is unchanged; OMP Runtime composition stays a wrapper gate. PR units use project-root cwd plus disposable attempts.
**Tech stack:** Existing Rust 2021 crate (`serde`, `serde_json`, `toml`, `ratatui`), bash dogfood proofs (`jq`, `git`), systemd user manager via sealed plan/apply, GitHub Actions already in `.github/workflows/ci.yml`.

---

## File map

| Path | Responsibility |
|---|---|
| `dogfood/drivers/*`, `dogfood/lib/*` | Tracked copy of OMP dogfood scripts (no `verify-merged-pr*`) |
| `tests/dogfood-source.sh` | Proof that tests default to the tracked tree |
| `tests/pr-attempt.sh` | Path retarget + MERGED-before-pass case |
| `tests/wrapper-contract.sh`, `tests/capability-release-run.sh`, `tests/capability-readiness.sh`, `tests/pr-critical.sh`, `tests/pr-settlement.sh`, `tests/pr-severity.sh`, `tests/runtime-wave.sh` | Path retarget only |
| `Cargo.toml` | `exclude` `/dogfood` |
| `dogfood/drivers/pr-run` | MERGED-before-pass (after import) |
| `README.md`, `docs/SCOPES.md`, `docs/DESIGN.md`, `docs/PACKAGING.md`, `docs/TESTING.md`, `CHANGELOG.md`, `AGENTS.md`, `Makefile`, `.github/workflows/ci.yml` | Docs/package 0.7.0 boundary |
| `/home/sf/worlds/base/agents/project-lead.md` | Drop recovered-worktree-as-normal-PR-path |
| `/home/sf/worlds/automation-agents/.omp/agents/{pr-maintainer,capability-maintainer,runtime-maintainer}.md` | PR cwd + Runtime composition wording |
| live unit `managed-omp-pr-10922` | Project-root cwd via plan/apply |
| live OMP `.systemd-ops/{drivers,lib}/<tracked names>` | Deploy copy of tracked files only |

Do not touch: `src/**`, `.ai-bridge/`, systemd-ops `.systemd-ops/scope.toml`, `lib/verify-merged-pr*`, capability unit worktrees, tags, crates.io.

## Parallelism

- **Serial first:** Task 1.
- **Then parallel (disjoint files):** Task 2 (`dogfood/drivers/pr-run` + `tests/pr-attempt.sh` MERGED case), Task 4 (worlds agent files only), Task 5 (live unit only).
- **Then serial:** Task 3 (repo docs/CI/changelog), Task 6 (OMP deploy of tracked names), Task 7 (verifier), Task 8 (push).

Task 2 must not start until Task 1 has committed the imported `pr-run` and retargeted `tests/pr-attempt.sh` path defaults. Task 2 only **appends** the MERGED case and edits `dogfood/drivers/pr-run`.

---

### Task 1: Track dogfood source and retarget tests

**Owner:** builder
**Files:**
- Create: `dogfood/drivers/{pr-run,pr-observe,capability-run,capability-observe,runtime-run,runtime-observe,runtime-reconcile,omp-release-watch,project-lead-watch,fork-main-sync}`
- Create: `dogfood/lib/{automation-wrapper,pr-attempt,pr-settlement,pr-review-state}`
- Create: `tests/dogfood-source.sh`
- Modify: `tests/pr-attempt.sh:7-8,78`
- Modify: `tests/wrapper-contract.sh:7-8,43`
- Modify: `tests/capability-release-run.sh:7-10`
- Modify: `tests/capability-readiness.sh:6`
- Modify: `tests/pr-critical.sh:6-7`
- Modify: `tests/pr-settlement.sh:6`
- Modify: `tests/pr-severity.sh:6`
- Modify: `tests/runtime-wave.sh:6`
- Modify: `Cargo.toml:16`
- Test: `tests/dogfood-source.sh` plus the eight existing dogfood proofs

**Verification (anti-gameable):** On a tree that still has the live OMP copy, `bash tests/dogfood-source.sh` passes **and** `grep -R '/home/sf/workspace/oh-my-pi/.systemd-ops' tests/*.sh` prints nothing. `DOGFOOD_ROOT=/tmp/does-not-exist bash tests/capability-readiness.sh` fails with `missing`. `bash tests/pr-attempt.sh` and the other seven proofs pass using `$ROOT/dogfood` without reading live OMP (prove by running with `DOGFOOD_ROOT` unset and confirming `tests/dogfood-source.sh` already forbade the live path).

- [x] **Step 1: Write the failing source proof**

Create `tests/dogfood-source.sh`:

```bash
#!/usr/bin/env bash
# Tracked dogfood is the default source for wrapper proofs.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }

[[ -d $ROOT/dogfood/drivers ]] || fail "missing $ROOT/dogfood/drivers"
[[ -d $ROOT/dogfood/lib ]] || fail "missing $ROOT/dogfood/lib"

for f in pr-run pr-observe capability-run capability-observe runtime-run runtime-observe runtime-reconcile omp-release-watch project-lead-watch fork-main-sync; do
  [[ -f $ROOT/dogfood/drivers/$f ]] || fail "missing driver $f"
done
for f in automation-wrapper pr-attempt pr-settlement pr-review-state; do
  [[ -f $ROOT/dogfood/lib/$f ]] || fail "missing lib $f"
done
[[ ! -e $ROOT/dogfood/lib/verify-merged-pr ]] || fail "imported out-of-scope verify-merged-pr"
[[ ! -e $ROOT/dogfood/lib/verify-merged-pr.test.sh ]] || fail "imported out-of-scope verify-merged-pr.test.sh"

if grep -R -n '/home/sf/workspace/oh-my-pi/.systemd-ops' "$ROOT/tests"/*.sh; then
  fail "tests still default to live OMP copy"
fi
echo "dogfood-source ok"
```

```bash
chmod +x tests/dogfood-source.sh
```

- [x] **Step 2: Run the proof to verify it fails**

Run: `bash tests/dogfood-source.sh`
Expected: FAIL with `missing /home/sf/workspace/systemd-ops/dogfood/drivers`

- [x] **Step 3: Copy the tracked files from live OMP (named files only)**

```bash
mkdir -p dogfood/drivers dogfood/lib
src=/home/sf/workspace/oh-my-pi/.systemd-ops
for f in pr-run pr-observe capability-run capability-observe runtime-run runtime-observe runtime-reconcile omp-release-watch project-lead-watch fork-main-sync; do
  cp -a "$src/drivers/$f" "dogfood/drivers/$f"
  chmod a+rX "dogfood/drivers/$f"
done
for f in automation-wrapper pr-attempt pr-settlement pr-review-state; do
  cp -a "$src/lib/$f" "dogfood/lib/$f"
  chmod a+rX "dogfood/lib/$f"
done
# do not copy verify-merged-pr*
test ! -e dogfood/lib/verify-merged-pr
```

Do not rewrite driver policy in this step. Byte copy plus preserve executability.

- [x] **Step 4: Run the proof; expect the tests-still-default failure**

Run: `bash tests/dogfood-source.sh`
Expected: FAIL with `tests still default to live OMP copy`

- [x] **Step 5: Retarget test defaults**

In every listed test, keep `ROOT=$(cd "$(dirname "$0")/.." && pwd)` and set defaults from `$ROOT/dogfood`.

`tests/pr-attempt.sh` lines 7-8 become:

```bash
DOGFOOD_ROOT=${DOGFOOD_ROOT:-$ROOT/dogfood}
SOURCE_DRIVER=${SOURCE_DRIVER:-$DOGFOOD_ROOT/drivers/pr-run}
SOURCE_LIB=${SOURCE_LIB:-$DOGFOOD_ROOT/lib/automation-wrapper}
```

Replace the hardcoded copy (currently line 78) with:

```bash
  cp "$DOGFOOD_ROOT/lib/pr-attempt" "$scope/.systemd-ops/lib/pr-attempt"
```

`tests/wrapper-contract.sh` lines 7-8 and the hardcoded `pr-attempt` copy (currently line 43): same `DOGFOOD_ROOT` / `SOURCE_DRIVER` / `SOURCE_LIB` pattern and `cp "$DOGFOOD_ROOT/lib/pr-attempt" ...`.

`tests/capability-release-run.sh` lines 7-10:

```bash
DOGFOOD_ROOT=${DOGFOOD_ROOT:-$ROOT/dogfood}
SOURCE_DRIVER=${SOURCE_DRIVER:-$DOGFOOD_ROOT/drivers/capability-run}
SOURCE_LIB=${SOURCE_LIB:-$DOGFOOD_ROOT/lib/automation-wrapper}
SETTLE_LIB=${SETTLE_LIB:-$DOGFOOD_ROOT/lib/pr-settlement}
REVIEW_LIB=${REVIEW_LIB:-$DOGFOOD_ROOT/lib/pr-review-state}
```

`tests/capability-readiness.sh` line 6:

```bash
LIB=${WRAPPER_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/automation-wrapper}
```

`tests/runtime-wave.sh` line 6: same `WRAPPER_LIB` default.

`tests/pr-settlement.sh` line 6:

```bash
LIB=${SETTLEMENT_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/pr-settlement}
```

`tests/pr-severity.sh` line 6:

```bash
LIB=${REVIEW_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/pr-review-state}
```

`tests/pr-critical.sh` lines 6-7:

```bash
LIB=${REVIEW_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/pr-review-state}
SETTLE=${SETTLE_LIB:-${DOGFOOD_ROOT:-$ROOT/dogfood}/lib/pr-settlement}
```

- [x] **Step 6: Exclude dogfood from the crate package**

In `Cargo.toml`, change:

```toml
exclude = ["/tests", "/.github", "/omp"]
```

to:

```toml
exclude = ["/tests", "/.github", "/omp", "/dogfood"]
```

- [x] **Step 7: Run proofs to verify they pass**

Need a debug binary for wrapper proofs:

```bash
cargo build
bash tests/dogfood-source.sh
bash tests/capability-readiness.sh
bash tests/runtime-wave.sh
bash tests/pr-settlement.sh
bash tests/pr-severity.sh
bash tests/pr-critical.sh
bash tests/wrapper-contract.sh
bash tests/pr-attempt.sh
bash tests/capability-release-run.sh
```

Expected: each prints its `ok` line (`dogfood-source ok`, `capability-readiness ok`, `runtime-wave ok`, and the corresponding success echo from the others). None may mention a missing live OMP path.

Negative check:

```bash
DOGFOOD_ROOT=/tmp/systemd-ops-dogfood-missing WRAPPER_LIB=/tmp/systemd-ops-dogfood-missing/lib/automation-wrapper bash tests/capability-readiness.sh
```

Expected: FAIL with `missing`.

- [x] **Step 8: Commit**

```bash
git add dogfood tests/dogfood-source.sh tests/pr-attempt.sh tests/wrapper-contract.sh tests/capability-release-run.sh tests/capability-readiness.sh tests/pr-critical.sh tests/pr-settlement.sh tests/pr-severity.sh tests/runtime-wave.sh Cargo.toml
git commit -m "$(cat <<'EOF'
test(automation): track OMP dogfood scripts in-repo

Proofs default to dogfood/{drivers,lib} instead of the live OMP tree.
Crate packaging still excludes the dogfood tree.
EOF
)"
```

Do not add `.ai-bridge/`, `.systemd-ops/scope.toml`, or `verify-merged-pr*`.

---

### Task 2: MERGED-before-pass in `pr-run`

**Owner:** builder
**Depends on:** Task 1
**Files:**
- Modify: `dogfood/drivers/pr-run` (after processed-input lock, before iteration-start)
- Modify: `tests/pr-attempt.sh` (append one case before the final `echo "pr-attempt ok"`)
- Test: `tests/pr-attempt.sh`

**Verification (anti-gameable):** `bash tests/pr-attempt.sh` passes, including a case where fake `GH_BIN` returns `MERGED` and fake-OMP is **not** invoked, no attempt directory remains, `automation complete --reason merged upstream` is recorded, and `user-dirty.txt` still exists. Existing `PROOF_SKIP_GH=1` cases still create attempts as today.

- [x] **Step 1: Write the failing MERGED-before-pass case**

In `tests/pr-attempt.sh`, immediately before `echo "pr-attempt ok"`, append:

```bash
# --- MERGED before pass: complete without a model invocation ---
scope=$(make_scope merged-before-pass)
mkdir -p "$scope/bin"
cat >"$scope/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == pr && "${2:-}" == view ]]; then
  echo MERGED
  exit 0
fi
exit 1
EOF
chmod +x "$scope/bin/gh"
real_bin=$BIN
cat >"$scope/bin/systemd-ops" <<EOF
#!/usr/bin/env bash
set -euo pipefail
args=("\$@")
joined=" \${args[*]} "
if [[ "\$joined" == *" automation complete "* ]]; then
  printf '%s\n' "\${args[@]}" >"$scope/complete-args"
  echo '{"schema_version":1,"ok":true,"data":{"completed":true}}'
  exit 0
fi
exec "$real_bin" "\${args[@]}"
EOF
chmod +x "$scope/bin/systemd-ops"
write_omp "$scope/fake-omp" 'echo called >>"${SYSTEMD_OPS_SCOPE_ROOT}/agent-calls"
"$SYSTEMD_OPS_BIN" --json --manager user automation report --headline "should not run" --summary '\''["no"]'\'' --outcome ready >/dev/null'
WORKTREE="$scope/user-worktree" \
SYSTEMD_OPS_SCOPE_ROOT="$scope" \
SYSTEMD_OPS_BIN="$scope/bin/systemd-ops" \
OMP_BIN="$scope/fake-omp" \
AGENT_CWD="$scope/agents" \
UPSTREAM_GENERATION="${UPSTREAM_GENERATION:-deadbeef}" \
PR_ATTEMPT_ROOT="$scope/attempts" \
GIT_BASE="$scope/user-worktree" \
GH_BIN="$scope/bin/gh" \
"$scope/.systemd-ops/operations/$STEM/run" \
  >/tmp/pr-attempt-merged.out 2>/tmp/pr-attempt-merged.err || fail "MERGED-before-pass wrapper failed: $(cat /tmp/pr-attempt-merged.err)"
[[ ! -e $scope/agent-calls ]] || fail "MERGED-before-pass invoked OMP"
[[ $(find "$scope/attempts" -mindepth 2 -maxdepth 2 -type d 2>/dev/null | wc -l) -eq 0 ]] || fail "MERGED-before-pass created an attempt"
grep -q 'automation complete' "$scope/complete-args" || fail "MERGED-before-pass did not complete: $(cat "$scope/complete-args" 2>/dev/null || true)"
grep -q 'merged upstream' "$scope/complete-args" || fail "complete reason was not merged upstream"
[[ -f $scope/user-worktree/user-dirty.txt ]] || fail "MERGED-before-pass destroyed user worktree"
```

Do not edit `dogfood/drivers/pr-run` yet.

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo build && bash tests/pr-attempt.sh`
Expected: FAIL with `MERGED-before-pass wrapper failed` or `MERGED-before-pass invoked OMP` or `MERGED-before-pass did not complete` (it currently pays for a pass / skips gh via `PROOF_SKIP_GH` in `run_pr`; this case does not use `run_pr` and will still launch OMP because `pr-run` has no pre-pass complete).

- [x] **Step 3: Implement bounded pre-pass complete**

In `dogfood/drivers/pr-run`, after the second `input_is_processed` block (the one inside the lock) and **before** `upstream_generation=$(resolve_target_generation)` / `operator iteration-start` / `create_pr_attempt`, insert:

```bash
GH_BIN=${GH_BIN:-/home/sf/.local/bin/gh}
if [[ -z "${PROOF_SKIP_GH:-}" && -x $GH_BIN ]]; then
  pr_state=$("$GH_BIN" pr view "$PR_NUMBER" --repo can1357/oh-my-pi --json state --jq '.state') || pr_state=
  if [[ "$pr_state" == MERGED ]]; then
    ops automation complete --unit "$OPERATION_STEM" --reason "merged upstream" || exit $?
    notify_parent completed
    exit 0
  fi
fi
```

Keep the existing post-success MERGED complete block. Do not complete on `CLOSED`. Do not call `create_pr_attempt` on this path. Do not retire capabilities.

Optionally, in the existing post-success block, use `"$GH_BIN"` instead of the hardcoded `/home/sf/.local/bin/gh` so both paths share one default. Do not change complete reason text.

- [x] **Step 4: Run the test to verify it passes**

Run: `cargo build && bash tests/pr-attempt.sh`
Expected: `pr-attempt ok` on stdout; `/tmp/pr-attempt-merged.err` has no attempt-create failure.

- [x] **Step 5: Commit**

```bash
git add dogfood/drivers/pr-run tests/pr-attempt.sh
git commit -m "$(cat <<'EOF'
fix(dogfood): complete already-merged PRs before a model pass

Skip the disposable attempt and OMP invocation when gh reports MERGED.
Existing post-pass complete remains as a backstop.
EOF
)"
```

---

### Task 3: Docs, package metadata, and CI (after Tasks 2, 4, 5)

**Owner:** builder
**Depends on:** Task 2 (changelog must mention MERGED-before-pass). Tasks 4 and 5 do not edit these files; wait for them only if the lead serializes, otherwise this task may start after Task 2 even if 4/5 are still running, because file ownership is disjoint. Prefer waiting until 4/5 finish so the changelog sentence about 10922 is factual.
**Files:**
- Modify: `CHANGELOG.md:7`
- Modify: `docs/SCOPES.md` (manifest `lead` bullet; waiting/runtime/PR cwd paragraphs)
- Modify: `docs/DESIGN.md` (add a short dogfood / Runtime / PR cwd section at the end of known operator-facing design, before any limits that are MCP-only if that is the natural place; if no such heading, append `## OMP dogfood and automation semantics`)
- Modify: `docs/PACKAGING.md` (dependencies row; “two crates”; installed docs)
- Modify: `docs/TESTING.md` (dogfood proof commands)
- Modify: `README.md` only if the Installing/Documentation sections omit `dogfood/` as git-tracked source; do not invent tools
- Modify: `AGENTS.md` layout table
- Modify: `Makefile:35`
- Modify: `.github/workflows/ci.yml` staged-install loop and check job
- Test: `bash tests/docs.sh`; staged install file list; `cargo package --list --locked`

**Verification (anti-gameable):** `grep -n 'no .*`lead`' docs/SCOPES.md` no longer claims there is no coordination lead. `docs/PACKAGING.md` names `toml` and `ratatui`. `Makefile` `DOCS` includes `docs/PACKAGING.md SECURITY.md CHANGELOG.md AGENTS.md`. Staged install produces those files. `cargo package --list --locked` contains no `dogfood/` path. `CHANGELOG.md` starts with `## 0.7.0 (not published)` and does not claim a tag.

- [x] **Step 1: Write a failing staged-docs assertion locally**

Run (expect missing files before Makefile change):

```bash
make DESTDIR=/tmp/systemd-ops-stage prefix=/usr install
test -f /tmp/systemd-ops-stage/usr/share/doc/systemd-ops/PACKAGING.md
```

Expected: FAIL (`No such file or directory` for `PACKAGING.md`) after a successful install of the current `DOCS` list.

- [x] **Step 2: Expand Makefile DOCS**

```makefile
DOCS    := README.md docs/TOOLS.md docs/DESIGN.md docs/TESTING.md docs/SCOPES.md docs/PACKAGING.md SECURITY.md CHANGELOG.md AGENTS.md
```

- [x] **Step 3: Re-run staged install**

```bash
rm -rf /tmp/systemd-ops-stage
make DESTDIR=/tmp/systemd-ops-stage prefix=/usr install
for f in README.md TOOLS.md DESIGN.md TESTING.md SCOPES.md PACKAGING.md SECURITY.md CHANGELOG.md AGENTS.md; do
  test -f /tmp/systemd-ops-stage/usr/share/doc/systemd-ops/$f
done
```

Expected: all exist. `make DESTDIR=/tmp/systemd-ops-stage prefix=/usr uninstall` still removes `$(docdir)`.

- [x] **Step 4: Reconcile prose (required sentences, not paraphrases of product)**

`docs/SCOPES.md` Rules bullet that currently says `no ... lead ... fields`: replace so it still forbids `owner_agent`, `hcom_agent`, `omp_profile`, and `manager`, and **adds** that optional `[coordination] lead = "hcom:xxxx"` is an opaque four-letter HCom handle, not a registry, and rust never shells HCom.

Keep the existing WAITING sentence. Immediately after it, add that OMP Runtime composition is a wrapper-domain gate (`capability_sources_ready_for_generation`): direct `capability-maintainer` children, structured checkpoint for the current generation, idle, no current blocker. That gate is not `derive_semantic_states`. Zero declared children remain vacuously READY in core waiting. Do not describe Runtime as a lead-chosen cycle.

Add: future PR-maintainer units set `WorkingDirectory` and the bound `pr-run` worktree argument to the scope project root. Durable identity is `fork/$HEAD_BRANCH`. Each intelligent pass uses a disposable attempt worktree. Recovered `fix/*` worktrees are not the normal PR path.

`docs/DESIGN.md`: tracked source is `dogfood/{drivers,lib}` in this repository; live OMP `.systemd-ops/{drivers,lib}` is a deployed copy. Tests must not default to the live tree.

`docs/PACKAGING.md` Facts table:

```
| Dependencies | serde, serde_json, toml. The TUI adds ratatui. No libsystemd, no D-Bus library, no async runtime |
```

Change “two crates, both packaged everywhere” to name those four crates. Under `$(docdir)/`, list the nine installed documentation files (the `DOCS` list). State that `dogfood/` is git-tracked OMP automation and is excluded from the crates.io package like `tests/` and `omp/`.

`docs/TESTING.md` Fast gates: after `bash tests/docs.sh`, document:

```
cargo build
bash tests/dogfood-source.sh
bash tests/capability-readiness.sh
bash tests/runtime-wave.sh
bash tests/pr-settlement.sh
bash tests/pr-severity.sh
bash tests/pr-critical.sh
bash tests/wrapper-contract.sh
bash tests/pr-attempt.sh
bash tests/capability-release-run.sh
```

These need `jq` and `git`; they use temporary scopes and do not mutate the operator’s live OMP units.

`AGENTS.md` layout table: add a row `dogfood/drivers`, `dogfood/lib` — tracked OMP dogfood scripts; tests default here; live OMP is a deploy copy.

`CHANGELOG.md`: replace the heading `## unreleased (not published)` with `## 0.7.0 (not published)`. Keep existing unreleased notes. Append a paragraph that this boundary tracks OMP dogfood scripts under `dogfood/`, points tests at that tree, documents `[coordination] lead`, generic waiting versus OMP Runtime composition, PR project-root cwd plus disposable attempts, installed docs matching README, and cheap complete of already-MERGED PRs before a model pass. State that crate/tag/GitHub publication is not part of this commit.

`README.md`: in Documentation or Installing, one sentence that git (not the crates.io tarball) contains `dogfood/` for the OMP automation scripts the proofs load.

- [x] **Step 5: CI**

In `.github/workflows/ci.yml` `check` job, after `cargo test` and before or after man-page lint, add:

```yaml
      - name: Dogfood proofs
        run: |
          sudo apt-get install -y -qq jq
          cargo build
          bash tests/dogfood-source.sh
          bash tests/capability-readiness.sh
          bash tests/runtime-wave.sh
          bash tests/pr-settlement.sh
          bash tests/pr-severity.sh
          bash tests/pr-critical.sh
          bash tests/wrapper-contract.sh
          bash tests/pr-attempt.sh
          bash tests/capability-release-run.sh
```

In the staged install `for f in` list, add:

```
                   /tmp/stage/usr/share/doc/systemd-ops/README.md \
                   /tmp/stage/usr/share/doc/systemd-ops/TOOLS.md \
                   /tmp/stage/usr/share/doc/systemd-ops/DESIGN.md \
                   /tmp/stage/usr/share/doc/systemd-ops/TESTING.md \
                   /tmp/stage/usr/share/doc/systemd-ops/SCOPES.md \
                   /tmp/stage/usr/share/doc/systemd-ops/PACKAGING.md \
                   /tmp/stage/usr/share/doc/systemd-ops/SECURITY.md \
                   /tmp/stage/usr/share/doc/systemd-ops/CHANGELOG.md \
                   /tmp/stage/usr/share/doc/systemd-ops/AGENTS.md
```

- [x] **Step 6: Package list and man lint**

```bash
bash tests/docs.sh
cargo package --list --locked | tee /tmp/systemd-ops-package.list
```

Expected: `tests/docs.sh` prints `== roff` and succeeds. `/tmp/systemd-ops-package.list` has **no** line beginning `dogfood/`. It still includes `docs/SCOPES.md` and `CHANGELOG.md`.

- [x] **Step 7: Commit**

```bash
git add CHANGELOG.md docs/SCOPES.md docs/DESIGN.md docs/PACKAGING.md docs/TESTING.md README.md AGENTS.md Makefile .github/workflows/ci.yml
git commit -m "$(cat <<'EOF'
docs: close the 0.7.0 documentation and package boundary

Installed docs match README. Tests and CI load tracked dogfood.
Publication (tag, crate, GitHub release) stays for the operator.
EOF
)"
```

---

### Task 4: Agent contracts (parallel with Task 2)

**Owner:** builder
**Depends on:** none (does not edit systemd-ops). May run in parallel with Task 1 only if the builder does not touch `dogfood/` or `tests/`.
**Files:**
- Modify: `/home/sf/worlds/base/agents/project-lead.md` critical/wake bullets
- Modify: `/home/sf/worlds/automation-agents/.omp/agents/pr-maintainer.md`
- Modify: `/home/sf/worlds/automation-agents/.omp/agents/capability-maintainer.md` only if it still tells operators to rebind recovered PR worktrees as the normal path (do not rewrite capability worktree instructions)
- Modify: `/home/sf/worlds/automation-agents/.omp/agents/runtime-maintainer.md` Runtime composition wording
- Test: none in systemd-ops; verifier greps the files

**Verification (anti-gameable):** `grep -n 'preserve dirty PR worktrees' /home/sf/worlds/base/agents/project-lead.md` is empty. `grep -n 'project-root\|project root' /home/sf/worlds/base/agents/project-lead.md /home/sf/worlds/automation-agents/.omp/agents/pr-maintainer.md` matches. Runtime-maintainer still says Runtime sources are capability-maintainer children and that the project lead does not write `fork/runtime`. Capability-maintainer still forbids force-push and does not gain auto-retire-on-merge.

- [x] **Step 1: Show the stale sentences**

```bash
grep -n 'preserve dirty PR worktrees\|rebind\|dirty/missing worktrees\|manual' \
  /home/sf/worlds/base/agents/project-lead.md \
  /home/sf/worlds/automation-agents/.omp/agents/*.md
```

Expected: the project-lead critical bullet `preserve dirty PR worktrees. recover by creating a clean replacement and rebinding cwd, never by resetting.` is present before the edit.

- [x] **Step 2: Replace project-lead recovered-worktree instructions**

In `/home/sf/worlds/base/agents/project-lead.md` `<critical>`:

Delete:

```
- preserve dirty PR worktrees. recover by creating a clean replacement and rebinding cwd, never by resetting.
```

Insert:

```
- PR-maintainer units use the project root as WorkingDirectory and bound git base. Each intelligent pass uses a disposable attempt worktree. Do not treat recovered dirty PR worktrees as the normal PR path. Never reset unknown dirty state. Never delete historical recovered worktrees.
```

In `<wake>` step 2, replace `dirty/missing worktrees` with `current-generation checkpoints` (keep owned operations and lead-routed blockers). Do not add a self-updating systemd-ops loop. Do not tell the lead to compose Runtime each cycle.

- [x] **Step 3: PR-maintainer contract**

Keep disposable-attempt wording. Add one sentence: unit cwd / bound git base is the project root, not a long-lived `fix/*` worktree. Keep “Never delete historical recovered worktrees.” Keep “Never reset unexpected local state outside this attempt.”

- [x] **Step 4: Runtime-maintainer contract**

If the file still implies the lead picks composition each cycle, replace that with: Runtime is automatic composition of accepted Capability checkpoints for the current release-watch generation; the lead does not write `fork/runtime` and does not manually compose a normal cycle. Keep “PRs are not Runtime sources.” Do not add capability auto-retire on merge.

- [x] **Step 5: Capability-maintainer**

No recovered-PR-worktree rewrite. No auto-retire. If the file already matches, make no edit.

- [x] **Step 6: Commit if those files are in a git repo; otherwise leave them on disk for the lead**

```bash
# only if the worlds trees are git checkouts the builder is allowed to commit
git -C /home/sf/worlds/base status --short agents/project-lead.md
git -C /home/sf/worlds/automation-agents status --short .omp/agents/
```

If they are dirty in a worlds git repo, commit there with message `docs(agents): PR project-root cwd; Runtime is checkpoint composition`. If they are not a git checkout, do not invent a repo; leave the files edited and report the paths in the builder yield.

---

### Task 5: Normalize `managed-omp-pr-10922` cwd

**Owner:** builder
**Depends on:** none (live unit only). Parallel with Tasks 2 and 4.
**Files:**
- Live unit only: `managed-omp-pr-10922.service` / `.timer` via systemd-ops plan/apply
- Do not edit git source in systemd-ops
- Do not delete `/home/sf/worktrees/omp/fix/copy-outline-lazy-read`

**Verification (anti-gameable):** `systemctl --user show -p WorkingDirectory --value managed-omp-pr-10922.service` prints `/home/sf/workspace/oh-my-pi`. `systemctl --user show -p ExecStart --value managed-omp-pr-10922.service` contains `pr-run 10922 fix/copy-outline-lazy-read managed-omp-pr-10922 /home/sf/workspace/oh-my-pi`. `test -d /home/sf/worktrees/omp/fix/copy-outline-lazy-read`. `git -C /home/sf/worktrees/omp/fix/copy-outline-lazy-read status --porcelain` is still empty (no reset). Origin comments may stay as they are.

- [x] **Step 1: Safety checks (do not apply yet)**

```bash
git -C /home/sf/worktrees/omp/fix/copy-outline-lazy-read status --porcelain
systemctl --user show -p ActiveState --value managed-omp-pr-10922.service
```

Expected: porcelain empty. If `ActiveState` is `activating` or `running` or `active` (oneshot linger would be unexpected; this unit is oneshot), wait until `inactive` or `dead`. Do not `reset-failed`, stop, or start.

If porcelain is **not** empty, **stop and yield blocked to the lead** with the dirty file list. Do not reset. Do not apply.

- [x] **Step 2: Read editable spec**

```bash
systemd-ops --json --manager user --write-prefix 'managed-*' \
  --scope-root /home/sf/workspace/oh-my-pi \
  --cwd /home/sf/workspace/oh-my-pi \
  inspect get-operation --unit managed-omp-pr-10922 \
  | tee /tmp/omp-pr-10922-before.json
```

Expected: `.ok == true` and `.data.editable_spec.cwd` is `/home/sf/worktrees/omp/fix/copy-outline-lazy-read`.

- [x] **Step 3: Plan update**

```bash
jq '.data.editable_spec
    | .cwd = "/home/sf/workspace/oh-my-pi"
    | .exec.argv[-1] = "/home/sf/workspace/oh-my-pi"' \
  /tmp/omp-pr-10922-before.json > /tmp/omp-pr-10922-spec.json
systemd-ops --json --manager user --write-prefix 'managed-*' \
  --scope-root /home/sf/workspace/oh-my-pi \
  --cwd /home/sf/workspace/oh-my-pi \
  automation plan-update --spec "$(cat /tmp/omp-pr-10922-spec.json)" \
  | tee /tmp/omp-pr-10922-plan.json
```

Expected: `.ok == true` and a `plan_token`. If `plan-update` rejects the spec (missing fields), pass the full object `jq '.data.editable_spec + {unit:"managed-omp-pr-10922"}'` with the two field edits; do not change schedule, exec.path, tags, or purpose.

- [x] **Step 4: Apply**

```bash
token=$(jq -er '.data.plan_token' /tmp/omp-pr-10922-plan.json)
systemd-ops --json --manager user --write-prefix 'managed-*' \
  --scope-root /home/sf/workspace/oh-my-pi \
  --cwd /home/sf/workspace/oh-my-pi \
  automation apply --plan-token "$token" \
  | tee /tmp/omp-pr-10922-apply.json
```

Expected: `.ok == true`.

- [x] **Step 5: Confirm and do not touch the recovered tree**

```bash
systemctl --user daemon-reload
systemctl --user cat managed-omp-pr-10922.service | tee /tmp/omp-pr-10922-after.service
grep -F 'WorkingDirectory=/home/sf/workspace/oh-my-pi' /tmp/omp-pr-10922-after.service
grep -F '/home/sf/workspace/oh-my-pi/.systemd-ops/drivers/pr-run 10922 fix/copy-outline-lazy-read managed-omp-pr-10922 /home/sf/workspace/oh-my-pi' /tmp/omp-pr-10922-after.service
test -d /home/sf/worktrees/omp/fix/copy-outline-lazy-read
git -C /home/sf/worktrees/omp/fix/copy-outline-lazy-read status --porcelain
```

Expected: greps match; worktree directory exists; porcelain empty. No git commit in systemd-ops for this task (no source files). Record `/tmp/omp-pr-10922-after.service` path in the builder yield.

---

### Task 6: Deploy tracked dogfood names onto live OMP

**Owner:** builder
**Depends on:** Tasks 1 and 2 (deploy the MERGED-before-pass `pr-run`).
**Files:**
- Overwrite only these live paths:
  - `/home/sf/workspace/oh-my-pi/.systemd-ops/drivers/{pr-run,pr-observe,capability-run,capability-observe,runtime-run,runtime-observe,runtime-reconcile,omp-release-watch,project-lead-watch,fork-main-sync}`
  - `/home/sf/workspace/oh-my-pi/.systemd-ops/lib/{automation-wrapper,pr-attempt,pr-settlement,pr-review-state}`
- Must remain: `lib/verify-merged-pr`, `lib/verify-merged-pr.test.sh`

**Verification (anti-gameable):** `cmp dogfood/drivers/pr-run /home/sf/workspace/oh-my-pi/.systemd-ops/drivers/pr-run` is silent. `test -e /home/sf/workspace/oh-my-pi/.systemd-ops/lib/verify-merged-pr`. `diff -q` for each tracked name is empty. No `rsync --delete`. No new timer.

- [x] **Step 1: Copy named files**

```bash
OMP_ROOT=/home/sf/workspace/oh-my-pi
for f in pr-run pr-observe capability-run capability-observe runtime-run runtime-observe runtime-reconcile omp-release-watch project-lead-watch fork-main-sync; do
  cp -a "dogfood/drivers/$f" "$OMP_ROOT/.systemd-ops/drivers/$f"
done
for f in automation-wrapper pr-attempt pr-settlement pr-review-state; do
  cp -a "dogfood/lib/$f" "$OMP_ROOT/.systemd-ops/lib/$f"
done
```

- [x] **Step 2: Prove extra live files survived and tracked names match**

```bash
test -e /home/sf/workspace/oh-my-pi/.systemd-ops/lib/verify-merged-pr
test -e /home/sf/workspace/oh-my-pi/.systemd-ops/lib/verify-merged-pr.test.sh
for f in pr-run pr-observe capability-run capability-observe runtime-run runtime-observe runtime-reconcile omp-release-watch project-lead-watch fork-main-sync; do
  cmp "dogfood/drivers/$f" "/home/sf/workspace/oh-my-pi/.systemd-ops/drivers/$f"
done
for f in automation-wrapper pr-attempt pr-settlement pr-review-state; do
  cmp "dogfood/lib/$f" "/home/sf/workspace/oh-my-pi/.systemd-ops/lib/$f"
done
```

Expected: all `cmp` silent; verify-merged-pr files still exist.

- [x] **Step 3: No systemd-ops git commit** unless a tiny `dogfood/deploy.sh` was added. Prefer **not** adding a script if the commands above are in TESTING.md (Task 3). If Task 3 already documented the copy, this task is operations only.

---

### Task 7: Union verification

**Owner:** verifier
**Depends on:** Tasks 1–6
**Files:** none (read-only checks)

**Verification (anti-gameable):** Re-run the commands below yourself. Do not trust builder “done”. Fail the task if any command disagrees.

- [ ] **Step 1: Git hygiene**

```bash
cd /home/sf/workspace/systemd-ops
git status --short --untracked-files=all
git log --oneline origin/main..HEAD
```

Expected: no staged/unstaged source edits. Untracked may still include `.ai-bridge/` and `.systemd-ops/scope.toml` only. History still contains `2d86613` and `aafe91e` plus the Task 1–3 commits. No tag created (`git describe --tags --abbrev=0` still `v0.6.0` unless the operator tagged outside this work).

- [ ] **Step 2: Rust gates**

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Expected: all pass / `Finished` with zero clippy warnings; fmt check exits 0.

- [ ] **Step 3: Dogfood proofs**

```bash
cargo build
bash tests/dogfood-source.sh
bash tests/capability-readiness.sh
bash tests/runtime-wave.sh
bash tests/pr-settlement.sh
bash tests/pr-severity.sh
bash tests/pr-critical.sh
bash tests/wrapper-contract.sh
bash tests/pr-attempt.sh
bash tests/capability-release-run.sh
bash tests/docs.sh
```

Expected: each ok. `tests/dogfood-source.sh` greps no live OMP `.systemd-ops` path in `tests/*.sh`.

- [ ] **Step 4: Staged install and crate list**

```bash
rm -rf /tmp/systemd-ops-verify-stage
make DESTDIR=/tmp/systemd-ops-verify-stage prefix=/usr install
for f in README.md TOOLS.md DESIGN.md TESTING.md SCOPES.md PACKAGING.md SECURITY.md CHANGELOG.md AGENTS.md; do
  test -f /tmp/systemd-ops-verify-stage/usr/share/doc/systemd-ops/$f
done
test -x /tmp/systemd-ops-verify-stage/usr/bin/systemd-ops
cargo package --list --locked | grep -E '^dogfood/' && exit 1 || true
```

Expected: docs and binary exist; grep for `^dogfood/` matches nothing (the `&& exit 1` must not fire).

- [ ] **Step 5: Live 10922 and deploy**

```bash
systemctl --user show -p WorkingDirectory --value managed-omp-pr-10922.service
cmp /home/sf/workspace/systemd-ops/dogfood/drivers/pr-run /home/sf/workspace/oh-my-pi/.systemd-ops/drivers/pr-run
test -e /home/sf/workspace/oh-my-pi/.systemd-ops/lib/verify-merged-pr
test -d /home/sf/worktrees/omp/fix/copy-outline-lazy-read
```

Expected: WorkingDirectory is `/home/sf/workspace/oh-my-pi`; `cmp` silent; extra live lib still present; recovered worktree still on disk.

- [ ] **Step 6: Agent contracts**

```bash
grep -n 'preserve dirty PR worktrees' /home/sf/worlds/base/agents/project-lead.md && exit 1 || true
grep -n '\[coordination\] lead' /home/sf/workspace/systemd-ops/docs/SCOPES.md
```

Expected: first grep matches nothing; SCOPES documents the lead field.

---

### Task 8: Push unpublished 0.7.0 git history; FOR USER publication

**Owner:** builder
**Depends on:** Task 7 green
**Files:** none beyond `git push`

**Verification (anti-gameable):** `git status -sb` shows `main` in sync with `origin/main` after push, still no `v0.7.0` tag, still no crates.io publish.

- [ ] **Step 1: Push**

```bash
git -C /home/sf/workspace/systemd-ops push origin main
```

Expected: push succeeds. Include the pre-existing test commits `2d86613` and `aafe91e`. Do not `--force`. Do not `git tag`. Do not `cargo publish`. Do not run `.github/workflows/release.yml` by fabricating a tag.

- [ ] **Step 2: Leave FOR USER publication**

Create no code. The lead files a blocked FOR USER item:

```
FOR USER: systemd-ops 0.7.0 git is pushed unpublished.
Cargo.toml is already 0.7.0. Tag v0.7.0, crates.io publish, and GitHub release need your approval.
Do not publish from this pass.
```

---

## Notes for builders

- Do not implement Task 7 or Task 8 before the lead says the previous task passed review.
- Do not run `git add -A`. Untracked `.ai-bridge/` and `.systemd-ops/scope.toml` stay untracked.
- Do not import or delete `verify-merged-pr*`.
- Do not change `src/automation.rs` waiting logic.
- Do not rewrite capability unit `WorkingDirectory` values.
