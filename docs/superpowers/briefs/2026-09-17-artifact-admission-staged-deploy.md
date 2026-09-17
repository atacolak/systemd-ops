# Design Brief: systemd-ops artifact admission + staged runtime deploy

**Operator:** Ata, 2026-09-17 (session attachment)
**Bead:** sdo-a89
**Repo:** /home/sf/workspace/systemd-ops
**Peer product epic:** omp-tytn (livu)  -  hop graph only; this brief owns the machine
**lead-sdd:** Technical Spec then Implementation Plan under
docs/superpowers/{specs,plans}/2026-09-17-artifact-admission-staged-deploy.md
No product code in this child.

## Intent (operator words  -  do not rewrite)

own the systemd-ops half of omp artifact ingestion + runtime deployment as one
coherent state machine. you own the machinery, not the product decisions and
not hand-integration.

midi/product leads decide which capability a verified artifact belongs to.
systemd-ops makes that decision operational.

## Implement (operator list)

1. capability artifact admission
   typed admission primitive/API for: verified artifact + originating work
   identity + target capability.
   existing capability: durably add the verified artifact/revision to that
   capability's desired maintained input. this must change the capability
   observation/fingerprint and wake its maintainer.
   new capability: create the capability branch/worktree + capability-maintainer
   automation as a direct child of runtime. admit the verified artifact as its
   first desired input. wake the capability maintainer.
   capability maintainer owns HOW the artifact is reconciled into its maintained
   capability against the current upstream generation. project lead does not
   cherry-pick into runtime.
   this must compose naturally with ordinary upstream upgrades: capability
   desired artifacts stay constant while target upstream generation changes;
   maintainer reconciles both and emits a fresh ready checkpoint.

2. runtime deployment state machine
   replace immediate publication with:
   composed -> candidate -> canary -> promoted -> rolling_out -> converged
   -> old_generation_retired
   runtime integrator may build/smoke-test immutable generation G, but do NOT
   immediately make it fleet-current.
   candidate G: immutable generation exists and passes current postconditions.
   notify canonical omp lead midi with exact G. await targeted recycle/canary ACK.
   canary: midi must prove same actor continuity + exact generation G. failure
   keeps previous promoted generation current.
   promotion: only after midi canary passes, atomically make G the promoted/current
   generation. emit typed promoted-generation event.
   rollout: maintain explicit migration cohort/acks for persistent actors that
   need G. midi/worlds handles per-actor quiescent recycle and reports
   exact-generation ACKs. old promoted generation remains available while any
   required live actor remains on it.
   gc: retire generation only when rollout semantics prove nobody requiring
   preservation uses it, plus a conservative final process/reference check.
   replace the current "current + two recent generations" correctness assumption.

3. pr normalization
   merged PR is no longer an active organism component.
   on verified upstream merge: send one system notification to project lead with
   relevant result/pr identity. delete/retire the PR-maintainer
   timer/service/automation definition/state from active systemd-ops. delete its
   branch/worktree where appropriate. it must disappear from normal TUI.
   github/git are the durable historical record; do not preserve a local
   automation mausoleum merely for archaeology.

4. capability normalization
   zero PR children alone does NOT retire a capability.
   retire a capability only when: no active PR-maintainer children remain, AND
   its maintained product delta against current upstream is empty / behavior
   has normalized upstream.
   then remove capability automation/worktree/branch and therefore its membership
   as a runtime source.

5. topology
   rewrite omp topology.txt to describe the invariant machine, not instance names.
   loops such as:
   upstream advance -> capability reconcile -> runtime compose -> candidate ->
   canonical canary -> promote -> fleet recycle -> generation gc
   and
   capability publication -> pr maintenance -> upstream merge -> capability
   normalization -> ephemeral machinery retirement
   no concrete PR numbers/capability names in topology.

6. acceptance
   dogfood .66 for real with midi.
   start with the existing verified artifact that is absent from runtime. it must
   be admitted, consumed by capability maintenance, composed, candidate-built,
   canaried on midi, promoted, rolled out/live-proven, and only then product-closed.
   immediately repeat with omp-vuz.1.
   do not fake acceptance by manually moving commits around. the machine itself
   must perform the state transitions.
   surface every state programmatically so systemd-ops tui and actor/talker
   surfaces can explain exactly where delivery is stalled.

## Observed current machine (do not guess past this)

- Observation fingerprint (src/automation.rs:330-331):
  sha256("world={world_fingerprint}\\nbrain={brain_revision}\\n")
  observe_input in dogfood/lib/automation-wrapper:25-27 is
  `ops automation observe | jq -er .data.observation.input_fingerprint`.
  brain_revision is computed from live files under brain_paths, not from the
  repo commit. A desired-artifact admission that does not change world or brain
  will not wake the maintainer. The spec MUST say how desired artifacts enter
  that hash.
- Runtime publish (dogfood/drivers/runtime-run:200-210, 373-377): after READY
  postconditions, publish_runtime_generation atomically flips
  generations/current and ~/.bun/bin/omp, writes published-generation.json
  {version:1, generation, output_revision, published_at, path}, then prunes
  to newest 3 excluding current. This is the behaviour item 2 replaces.
- Live (2026-09-17 17:1x): generations/current -> e2057f4b;
  published-generation.json generation 1c0303b1 / output e2057f4b published
  2026-09-17T09:46:42Z. a268e6a1 also exists under generations/. No candidate
  or canary state file exists.
- Authoring already exists and is the only write path (AGENTS.md: one mutating
  call through write::apply):
  author plan-create/update/retire + author apply
  automation plan-create/update/retire + automation apply
  Admission MUST compose these. Do not add a second unit writer.
- CLI surface today (src/bin/systemd_ops.rs:47-86): no admit verb, no deploy
  phase verb, no canary-ack verb.
- Notification seam: system_notify <source-id> <intent> <message> via
  hcom send --as-system. Source ids already in use: omp-runtime, omp-capability,
  systemd-ops:<operation-stem>. Never --from, never unaddressed send, never
  overseer.
- Topology is human-authored, not hashed into automation inputs, not authority
  for membership (architecture page). Current file
  /home/sf/workspace/oh-my-pi/.systemd-ops/topology.txt names concrete PRs
  (9833, 9365, 9367, 9969, 9363) and capability names.
- livu/tytn already prepared local caps (NOT pushed; Ata-push is their gate)
  and a unit-request packet at /tmp/tytn-t4-hera-request.md. hera will NOT
  hand-author those units on the old path. Admission consumes them.
  First dogfood identities (product-owned):
    .66  cap/retry-fallback-unowned @ b5a0e2264b
         worktree /home/sf/worktrees/omp/cap/retry-fallback-unowned
    omp-vuz.1 cap/todo-passive-phase @ 4517803da2
         worktree /home/sf/worktrees/omp/cap/todo-passive-phase
- Existing negative boundary stays: do not auto-retire a capability merely
  because its PR children are gone. Item 4 is the empty-delta rule.
- Substrate rule: core Rust must not embed OMP graph policy (parent/child
  hierarchies, checkpoint semantics) as generic timer behaviour. OMP-shaped
  types are allowed when they are explicit operation/agent-class state, not
  when they leak into every scope.
- Passive timer invariant: timer ticks are read-only polls. Admission and
  promote are explicit verbs, not timer side effects.
- system-notify / HCOM is transport, not the durable store. Deploy phase,
  desired artifacts, and cohort ACKs live in systemd-ops operation state.

## Decisions the spec must make (do not leave as open questions)

D1. Admission API shape: one new CLI family (recommended: `automation admit`
    and `automation deploy-*`), JSON --spec, plan/apply sealed tokens. MCP
    tools only if they wrap the same verbs. No driver-only hidden path.
D2. How desired artifacts enter the observation hash. Recommended: a durable
    desired-inputs.json (or equivalent) under the capability operation state,
    listed in brain_paths or folded into world_fingerprint by the observer
    so adding/removing a revision changes input_fingerprint. Pin the exact
    hash inputs.
D3. Deploy state store: one file on the runtime-integrator operation state
    (recommended: state/deployment.json) enumerating generation -> phase,
    canary evidence, cohort ACKs. published-generation.json remains the
    promoted pointer or is superseded  -  pick one and migrate.
D4. Canary ACK: typed CLI verb that records midi's exact-G + same-actor
    proof. systemd-ops does not recycle midi. It notifies and waits.
    Specify the proof fields (generation sha, actor name, session/continuity
    token) and fail-closed rules.
D5. Wake: reuse existing timer/notify-parent. Admission of an existing
    capability must make the next capability-run tick see a new fingerprint
    (not a no-op skip). New capability must start its timer.
D6. PR retire: compose author plan-retire + automation plan-retire + one
    system_notify. Specify the verified-merge evidence the verb requires
    (do not scrape GitHub from a timer).
D7. Capability retire: same retire composition, gated on empty merge-tree
    vs current upstream AND zero PR-maintainer children. The empty-delta
    check already exists as capability-run condition 4; reuse it, do not
    invent a second definition.
D8. TUI/inspect: every phase and stall reason must be present on
    automation inspect / ScopeView so the cockpit can name the stall
    without reading journals. Specify the JSON fields.
D9. Tests: hermetic proofs for admit (existing vs new), fingerprint move,
    refuse-overseer/unaddressed, candidate-does-not-flip-current,
    canary-fail-keeps-promoted, promote-atomic, gc-refuses-live-ref,
    pr-retire-removes-from-scope-view, capability-retire-refuses-when-delta,
    topology has no PR numbers. Name the test files.
D10. Dogfood order is .66 then omp-vuz.1. The spec's acceptance section
     names those identities and forbids hand-author / cherry-pick.

## Non-goals

- Product decision of which artifact belongs to which capability
- Hand-merge onto fork/runtime or hand-repoint of ~/.bun/bin/omp
- Implementing .67 / attention-compiler / worlds hops
- Closing Ata-gated beads (bn2, r4j, du5, s7r)
- Rewriting HCOM or recycle_actor
- A second notification transport

## Output

Write both documents. Label every current-system claim observed / inferred /
unknown. No em dashes. No non-ASCII. Spec first, then plan with numbered
tasks a builder can execute one at a time.
