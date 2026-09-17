# Artifact Admission and Staged Runtime Deploy Technical Spec

**Design Brief:** `docs/superpowers/briefs/2026-09-17-artifact-admission-staged-deploy.md` (operator charter 2026-09-17, bead `sdo-a89`)
**Peer product epic:** `omp-tytn` (livu). Product classification is midi's. This spec owns the machine.
**Status:** sound
**Repo:** `/home/sf/workspace/systemd-ops`

## Intent (from the Brief - do not rewrite)

Quoted from the Brief, not paraphrased.

> own the systemd-ops half of omp artifact ingestion + runtime deployment as one
> coherent state machine. you own the machinery, not the product decisions and
> not hand-integration.
>
> midi/product leads decide which capability a verified artifact belongs to.
> systemd-ops makes that decision operational.

The six implement items in the Brief are binding: typed admission, staged
deploy (`composed -> candidate -> canary -> promoted -> rolling_out ->
converged -> old_generation_retired`), PR normalization, capability
normalization, invariant topology, and machine-performed dogfood of `.66`
then `omp-vuz.1`.

Midi #98247 (2026-09-17) is a hygiene split, not a restore demand. It does
not replace the charter. It names who invokes which verb:

- systemd-ops: the admission primitive; candidate G; inform midi; promote
  only after midi canary ACK; GC using occupancy midi publishes; stopped
  and retired actors are not eternal blockers.
- midi: classify existing-vs-new capability; invoke admission (not
  cherry-pick); generation-targeted recycle; canary self-recycle onto G;
  occupancy and ACK state; one typed generation-upgrade event after promote.

## Mapping onto the current system

Claims below are labelled **observed** (read in the source at the cited
line), **inferred** (derived from observed code), or **unknown**.

### Observation does not see desired artifacts

**Observed** - `src/automation.rs:330-331` hashes only

    world={world_fingerprint}
    brain={brain_revision}

`brain_revision` (`src/automation.rs:1010-1033`) is `sha256` of canonical
`automation.toml`, the agent role file, and each `brain_paths` regular
file under the scope root. Operation state is not an input.

**Observed** - `dogfood/lib/automation-wrapper:25-27` reads
`.data.observation.input_fingerprint` from `automation observe`. A driver
no-op-skips when that hash equals `processed.json`.

**Inferred** - writing a file under `operations/<stem>/state/` does not
move `input_fingerprint`. Admission that only records desired artifacts
in state will be skipped on the next tick unless the hash formula changes.

### Publish is immediate

**Observed** - `dogfood/drivers/runtime-run:181-210`
`publish_runtime_generation` builds the detached generation worktree,
atomically flips `generations/current`, points `~/.bun/bin/omp` at
`current`, writes `state/published-generation.json`
`{version,generation,output_revision,published_at,path}`, then
`prune_runtime_generations` (`:153-178`) keeps `current` plus the two
newest other directories.

**Observed** - `tests/runtime-generation-publish.sh:92-96` pins that
`current` and the omp install flip on publish.

**Observed** - live 2026-09-17 17:1x:
`generations/current -> e2057f4b2f1bcaf52a4ce8c0aa026faa55a2b377`,
`published-generation.json` generation `1c0303b1...` published
`2026-09-17T09:46:42Z`. No `deployment.json` exists.

### Authoring is the only unit writer

**Observed** - `AGENTS.md:92-94`: the only system-state mutation is
`systemd::apply_verb`, reached only from `write::apply`. New mutations
route through plan/apply.

**Observed** - CLI (`src/bin/systemd_ops.rs:47-86`) has `author
plan-create|update|retire` and `automation plan-create|update|retire`
plus `automation apply`. There is no admit verb, no deploy-phase verb,
no canary-ack verb.

**Observed** - `automation plan-create` (`src/automation.rs:1680-1751`)
nests an author plan token and writes `automation.toml` on apply. That
is the composition admission must reuse. Do not add a second unit writer.

**Observed** - `automation.toml` is read at `src/automation.rs:220` and
has no other write site in Rust besides the automation apply path.
Drivers never write it.

### Capability no-op / empty-delta already exists

**Observed** - `dogfood/drivers/capability-run:43-67` condition 4 is
`git merge-tree "$base" HEAD "$generation"` printing nothing, with a
clean tree and the target an ancestor of HEAD. The file states condition
4 is currently implied by condition 3 and is kept because it is the
runtime owner's specified no-op command.

Capability retirement reuses this predicate. It does not invent a
second empty-delta definition.

### Notify is a seam, not a store

**Observed** - `dogfood/lib/system-notify` sends `hcom send --as-system`
with a stable source id. Source ids in use: `omp-runtime`,
`omp-capability`, `systemd-ops:<operation-stem>`. Never `--from`, never
an unaddressed send, never `overseer` as a source id.

Deploy phase, desired artifacts, and cohort ACKs live in operation
state. HCOM carries the inform. It is not the durable record.

### Topology is instance-named

**Observed** - `/home/sf/workspace/oh-my-pi/.systemd-ops/topology.txt`
names concrete PRs (9833, 9365, 9367, 9969, 9363) and capability
instance names. Topology is human-authored and is not hashed into
automation inputs.

### First dogfood identities

**Observed** - midi #98247: `.66` is the first specimen, already
verifier-passed, marooned at `/home/sf/.omp/wt/ayn-66-f6de4bd`. Do not
manually rescue it. Then `omp-vuz.1` on the same path.

**Observed** - that worktree is detached at
`969062200754ea02cfac922e5ebb8c608c079e15` with uncommitted edits in
`packages/coding-agent/src/session/retry-fallback-chains.ts` and
`packages/coding-agent/test/retry-fallback.test.ts`. The verified
revision midi will pass to admit may differ from that HEAD. This spec
does not pick the sha.

**Observed** - livu `/tmp/tytn-t4-hera-request.md` prepared local
`cap/*` branches and proposed `author plan-create` packets. Those hops
assumed the old cherry-pick path and wait on this primitive. This spec
forbids hand-authoring those units on the old path.

**Unknown** - midi's exact `(originating_work, artifact_revision,
capability)` triple for `.66` and for `omp-vuz.1`. The verbs accept
the triple; they do not classify.

## Decisions

### D1. Admission API

One sealed-token family, no driver-only path, no second unit writer.

    systemd-ops --json --manager user --scope-root <scope> \
      automation plan-admit --spec '<json>'
    systemd-ops --json --manager user --scope-root <scope> \
      automation apply --plan-token <token>

`--spec` is one JSON object:

    {
      "version": 1,
      "originating_work": "<bead or epic id>",
      "artifact_revision": "<40 lowercase hex>",
      "capability": "<capability identity>"
    }

Optional, new-capability only:

      "worktree": "<absolute path>"

Charset:

- `originating_work`: `[A-Za-z0-9_.:-]+`, 1..80
- `capability`: `[A-Za-z0-9_-]+`, 1..50
- `artifact_revision`: exactly 40 `[0-9a-f]`

`capability` is the product identity (`retry-fallback-unowned`), not a
unit stem. systemd-ops maps it:

- unit `managed-omp-cap-<capability>`
- branch `cap/<capability>`
- parent `managed-omp-runtime-integrator`
- tags `omp`, `capability-maintainer`, `capability:<capability>`,
  `branch:cap/<capability>`

MCP tools, if added, wrap these two verbs only. They do not grow a
third write path.

Existing capability: apply appends the triple to
`operations/<unit>/state/desired-inputs.json`, refuses a duplicate
`(originating_work, artifact_revision)`, and does not rewrite the unit.

New capability: apply composes, in one `write::apply`:

1. `author plan-create` for the oneshot/timer (same shape as live
   `managed-omp-cap-viewport`: `kind: oneshot`, interval 3min after
   inactive, `on_boot_sec: 5min`).
2. write `automation.toml` via the existing automation-create metadata
   path.
3. write `desired-inputs.json` with the first entry.
4. enable and start the timer (author apply already does this).

`worktree` is required for a new capability when the scope has no
`capability_worktree_root` in `scope.toml`. Default, when that key is
present: `<capability_worktree_root>/<capability>`. The verb creates
the detached worktree at `artifact_revision` only when the directory
does not already exist. It does not push `fork/cap/<capability>`.
Push remains Ata's product gate; a child whose `capability-observe`
cannot resolve `fork/$BRANCH` stays unready, which is existing
observer behaviour, not a new rule.

Refuse:

- missing or malformed fields (rc 2, no write)
- `overseer` as `originating_work` or `capability`
- a capability identity that collides on `capability:` or `branch:`
  tags with a live sibling (`runtime-reconcile:24-35` already exits 4)
- apply without a matching sealed token

### D2. Desired artifacts enter the observation hash

File, per capability operation:

    <scope>/.systemd-ops/operations/<unit>/state/desired-inputs.json

    {
      "version": 1,
      "inputs": [
        {
          "originating_work": "...",
          "artifact_revision": "...",
          "admitted_at": "<RFC3339>"
        }
      ]
    }

`effective_input_fingerprint` becomes:

    sha256("world={wf}\nbrain={br}\n")

when `desired-inputs.json` is absent, and

    sha256("world={wf}\nbrain={br}\ndesired={sha256_hex(file bytes)}\n")

when it is present. Adding or removing a revision therefore moves
`input_fingerprint`. Caps that have never been admitted keep today's
hash, so the formula change is not a fleet-wide reprocess.

The capability maintainer reads `desired-inputs.json` as the set of
revisions it must reconcile into `cap/<capability>` against the current
upstream generation. How it reconciles is the maintainer's job. This
spec does not cherry-pick.

Upstream generation changes still come from the observer's
`world_fingerprint` / `generation`. Desired artifacts stay constant
across that change. Both moving independently is the point.

### D3. Deploy state store

One file on the runtime-integrator:

    <scope>/.systemd-ops/operations/managed-omp-runtime-integrator/state/deployment.json

    {
      "version": 1,
      "generations_root": "<absolute>",
      "promoted": {
        "generation": "<40 hex or empty>",
        "output_revision": "<40 hex or empty>",
        "promoted_at": "<RFC3339 or empty>"
      },
      "candidate": {
        "generation": "<40 hex>",
        "output_revision": "<40 hex>",
        "composed_at": "<RFC3339>",
        "phase": "composed|candidate|canary|rolling_out|converged"
      },
      "canary": {
        "actor": "midi",
        "result": "pass|fail|pending",
        "observed_generation": "<40 hex or empty>",
        "continuity": {},
        "recorded_at": "<RFC3339 or empty>"
      },
      "occupancy": [
        {"name": "...", "generation": "<40 hex>", "state": "live|stopped|retired", "acked_at": "<RFC3339>"}
      ],
      "stall": {
        "phase": "<phase>",
        "reason": "<one line>"
      }
    }

`published-generation.json` remains the promoted pointer. Candidate
must not write it and must not flip `generations/current` or the omp
install. Promote rewrites `published-generation.json` as today's
`write_published_generation_json` does, then flips `current`.

The "current + two recent" prune is retired. GC is a separate verb
(D3b) that reads `occupancy`.

`runtime-run` after READY postconditions and a successful
`generation_is_loadable` check:

1. leaves the generation worktree in place
2. writes `deployment.json` candidate with `phase=candidate`
3. `system_notify omp-runtime inform` midi with the exact generation
   and output revision
4. writes the composition checkpoint
5. does not call the current-flip half of `publish_runtime_generation`

Extract the flip (`ln`/`mv -T` of `current`, `ensure_omp_points_at_current`,
`write_published_generation_json`) into `dogfood/lib/runtime-promote`,
sourced by the promote apply. Timer ticks remain read-only polls.
Promote is not a timer side effect.

### D4. Canary ACK

    systemd-ops --json --manager user --scope-root <scope> \
      automation plan-deploy-canary-ack --spec '<json>'
    systemd-ops --json --manager user --scope-root <scope> \
      automation apply --plan-token <token>

    {
      "version": 1,
      "generation": "<40 hex>",
      "actor": "midi",
      "result": "pass|fail",
      "observed_generation": "<40 hex>",
      "continuity": {
        "actor": "midi",
        "session_id": "<string, 1..80>"
      }
    }

Fail-closed, no flip, previous promoted stays current, when any of:

- `actor` is not `midi`
- `result` is `fail`
- `generation` is not the current candidate
- `observed_generation` != `generation`
- `continuity.actor` is not `midi`
- `continuity.session_id` is empty
- there is no candidate

Pass: apply records canary evidence, then in the same apply promotes
(D3 flip). systemd-ops does not recycle midi. It notifies and waits.

A failed canary may be retried with a new ACK against the same
candidate. It does not invent a new generation.

### D4b. Occupancy and GC

    systemd-ops --json --manager user --scope-root <scope> \
      automation plan-deploy-occupancy --spec '<json>'
    systemd-ops --json --manager user --scope-root <scope> \
      automation apply --plan-token <token>

    {
      "version": 1,
      "generation": "<40 hex>",
      "actors": [
        {"name": "<actor>", "generation": "<40 hex>", "state": "live|stopped|retired"}
      ]
    }

This is how midi publishes occupancy. systemd-ops does not scrape
HCOM or `/proc` to build the cohort.

After promote, phase becomes `rolling_out`. It becomes `converged`
when every occupancy row is either acked on the promoted generation
or `state=retired` / `state=stopped` and not in a required-live set.
Stopped and retired are not eternal blockers.

    automation plan-deploy-gc --generation <40 hex>
    automation apply --plan-token <token>

GC refuses unless:

1. the generation is not promoted
2. no occupancy row with `state=live` names that generation
3. a conservative final check: no process whose executable resolves
   into that generation directory, and `generations/current` does not
   point at it

On pass it removes that generation worktree and drops it from
`deployment.json`. The newest-3 prune is gone.

### D5. Wake

Existing capability: the desired-inputs write moves `input_fingerprint`
(D2). The next `capability-run` tick cannot no-op-skip. No extra wake
channel.

New capability: author apply enables the timer. First tick observes
the new fingerprint and must not treat a missing checkpoint as a
verified no-op.

Admission does not `hcom send` to the maintainer. The timer is the
wake.

### D6. PR normalize

Merged PR is no longer an active organism component.

    automation plan-normalize-pr --spec '{
      "version": 1,
      "unit": "managed-omp-pr-<n>",
      "pr": "<n>",
      "merge_sha": "<40 hex>",
      "merged_at": "<RFC3339>"
    }'
    automation apply --plan-token <token>

The caller supplies verified-merge evidence. The verb does not scrape
GitHub and is not a timer side effect.

Apply, in one `write::apply`:

1. `system_notify omp-runtime inform` the coordination lead with
   `pr`, `merge_sha`, `unit`
2. `automation plan-retire` + nested `author plan-retire` (existing
   composition)
3. remove the bound worktree if `cwd` exists and is a registered
   worktree of the scope repo; refuse to `rm -rf` anything else
4. the unit disappears from `inspect list-operations` and the TUI
   because retire already drops it from the owned set

GitHub and git remain the historical record. Do not keep a retired
automation home "for archaeology".

### D7. Capability normalize

Zero PR children alone does not retire a capability.

    automation plan-normalize-capability --spec '{
      "version": 1,
      "unit": "managed-omp-cap-<capability>"
    }'
    automation apply --plan-token <token>

Refuse unless both:

1. no owned child with `agent=pr-maintainer` and `parent=<unit>`
2. `capability_pass_is_verified_noop` against the current upstream
   generation (the existing condition 4, invoked read-only)

Then the same retire composition as D6, plus delete
`cap/<capability>` worktree when it is a registered worktree, plus
drop that source from runtime membership (the child is gone, so
`capability_sources_ready_for_generation` no longer waits on it).

### D8. Inspect and stall surface

`automation inspect --unit managed-omp-runtime-integrator` gains
`deployment` equal to `deployment.json` (or null if absent).

`automation inspect --unit managed-omp-cap-<capability>` gains
`desired_inputs` equal to the file (or null) and
`desired_unconsumed` true when any admitted revision is not an
ancestor of the capability HEAD.

`scope show` attention, runtime-integrator only:

- `deploy_stalled` when `deployment.stall.reason` is non-empty
- `canary_pending` when phase is `candidate` and canary.result is
  `pending`

capability only:

- `admission_unconsumed` when `desired_unconsumed` is true

Health is not degraded by these codes. Silent `healthy` plus empty
attention is the defect this surface exists to prevent. TUI shows
the attention code and the stall reason in the existing reason
column. No new crate. MCP `get_operation` returns the same
`automation` object `inspect` does, so talker surfaces see the stall
without reading journals.

### D9. Tests

Hermetic proofs, no live oh-my-pi writes:

- `src/automation.rs` unit: fingerprint absent file equals today's
  hash; present file moves the hash; refuse malformed admit spec;
  refuse `overseer`; duplicate triple is a no-op write
- `tests/admission.sh`: existing capability desired-inputs write;
  new capability creates unit + `automation.toml` + first input +
  timer enabled; both through plan/apply
- `tests/runtime-generation-publish.sh`: rewrite the current-flip
  pins. Candidate READY must leave `generations/current` and the omp
  install on the previous promoted generation. A separate promote
  apply must flip both and rewrite `published-generation.json`.
- `tests/deploy-canary.sh`: fail ACK keeps promoted; pass ACK flips
  once and is atomic (current and published-generation match)
- `tests/deploy-gc.sh`: GC refuses a live occupancy row; GC refuses
  when `current` points at G; GC removes G when occupancy is
  retired/stopped/acked elsewhere and the process check is clean
- `tests/normalize-pr.sh`: after apply the unit is absent from
  `list-operations`; a missing `merge_sha` is refused; no GitHub
  call site is introduced
- `tests/normalize-capability.sh`: refuse when merge-tree is
  non-empty; refuse when a pr-maintainer child remains; pass
  retires the unit
- `tests/docs.sh` / a topology proof: the committed topology file
  contains no `PR <digits>` and no `managed-omp-pr-` / concrete
  capability instance names

`tests/runtime-generation-publish.sh` is already in CI. The rewrite
must stay green. New proofs go in the Dogfood proofs step next to
it (named in `docs/TESTING.md` in the same commit that adds them).

### D10. Dogfood order

1. midi classifies `.66` and invokes `plan-admit` + `apply` with the
   real triple. systemd-ops does not invent the sha and does not
   copy `/home/sf/.omp/wt/ayn-66-f6de4bd` by hand.
2. The capability maintainer consumes the desired input and emits a
   READY checkpoint. hera does not cherry-pick.
3. Runtime compose builds candidate G, notifies midi, does not flip
   `current`.
4. midi canaries onto G and ACKs.
5. Promote flips `current`. midi emits the generation-upgrade event.
   Occupancy ACKs roll the fleet. GC waits.
6. Only then is `.66` product-closed.
7. Repeat immediately with `omp-vuz.1`.

Hand-author of livu's three (now four) proposed units, hand-merge
onto fork/runtime, and hand-repoint of `~/.bun/bin/omp` are
acceptance failures.

## Non-goals

- Product decision of which artifact belongs to which capability
- Hand-merge onto fork/runtime or hand-repoint of `~/.bun/bin/omp`
- Implementing `.67` / attention-compiler / worlds hops
- Closing Ata-gated beads (`bn2`, `r4j`, `du5`, `s7r`)
- Rewriting HCOM or `recycle_actor`
- A second notification transport
- Auto-retire of a capability merely because its PR children are gone

## Acceptance (machine, not a narrative)

A change is done when:

1. The admit invoke above is the only path from a verified artifact
   into a capability's desired input, and it moves
   `input_fingerprint`.
2. A READY runtime compose leaves `generations/current` on the
   previous promoted generation.
3. A midi canary fail leaves that pointer untouched; a pass flips
   it exactly once and rewrites `published-generation.json`.
4. GC cannot delete a generation a live occupancy row still names.
5. `plan-normalize-pr` + apply removes the PR unit from ScopeView.
6. `plan-normalize-capability` refuses a non-empty merge-tree and
   refuses a remaining PR child.
7. `topology.txt` describes the two loops and names no PR number
   and no capability instance.
8. `.66` then `omp-vuz.1` have each travelled admit -> consume ->
   compose -> candidate -> canary -> promote -> rollout without a
   hand-moved commit.

Every phase and stall reason is present on `automation inspect` /
`scope show` so a talker can name the stall without a journal.
