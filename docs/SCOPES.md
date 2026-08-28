# Responsibility scopes

systemd-ops is not an all-systemd dashboard. The normal surface is an
operational console for one **responsibility scope**.

```
systemd                 executes truth
agents                  first-class author/control
TUI                     human operational awareness
```

CLI `--json` and the TUI consume the same derived core model. Neither
shells the other. MCP remains an optional frontend over the same core.
OMP continues to shell `systemd-ops --json`.
OMP prefers the session cwd over its factory cwd, passes that value as
both direct CLI `--cwd` and child process cwd, and inherits
`SYSTEMD_OPS_SCOPE_ROOT` from the parent environment. It adds no separate
scope-root parameter.

## Scope

A scope is a project-local identity, not a security ACL.

| Field | Meaning |
|---|---|
| `id` | stable identifier chosen once (`speech`, `personal`, `proxy`) |
| root | directory containing `.systemd-ops/scope.toml` or the legacy `.systemd-ops.toml` (not a config field) |
| owned | operation-stem globs this scope is responsible for |
| critical | explicit owned stems that fail the scope when they fail |
| watch | explicit stems this scope considers relevant but does not own |
| `automation.agent_root` | optional absolute discovery root for reusable OMP agent definitions |

Ownership here is operational responsibility. The hard mutation
boundary remains the configured `--write-prefix` (typically
`managed-*`). Cross-scope writes are possible and produce a warning;
they are not denied.

The scope is the durable owner identity. The manifest does not name
agents, HCom identities, OMP profiles, or a global registry.

## Manifest and resolution

The preferred manifest is `.systemd-ops/scope.toml`. The legacy
`.systemd-ops.toml` remains readable for compatibility. If both names
exist at the same root, resolution reports an ambiguity error. It does
not silently prefer either file.

Scope resolution uses the first applicable source:

1. direct CLI `--scope-root PATH`
2. `SYSTEMD_OPS_SCOPE_ROOT`
3. upward discovery from `--cwd PATH`, or the process cwd when `--cwd`
   is absent

An explicit root must contain one manifest. Upward discovery checks each
directory for the preferred or legacy name and stops at the first root.

```toml
[scope]
id = "speech"
owned = ["managed-speech-*"]
critical = [
  "managed-speech-asr",
  "managed-speech-tts",
]

[[watch]]
operation = "managed-proxy-health"

[automation]
agent_root = "/srv/automation-agents"
```

Rules:

- `scope.id` is required, 1..64 characters, `[a-z][a-z0-9-]*`.
- `owned` is one or more globs (`*` and `?`, same matcher as write-prefix).
- `critical` is under `[scope]`, explicit stems, not globs. Each must match an owned glob.
- `watch` entries are explicit stems. A stem must not be both owned and watched.
- unknown keys are refused. There is no top-level `critical`.
- no `owner_agent`, `hcom_agent`, `omp_profile`, `lead`, or `manager` fields.

Once a manifest exists it is canonical scope identity. Agents may infer
a first id and owned prefix at project setup; they must not keep guessing
afterward.

`systemd-ops scope show` and `systemd-ops scope validate` are the
minimum CLI. The same global `--scope-root` selection applies to `scope`,
`operator`, its iteration commands, and `tui`. Author and control
provenance continue to use `--cwd`. There is no `scope init` in this slice.

## Naming

New local automations use `managed-<scope>-<operation>`. The second
component is the responsibility scope, not a tag. Tags stay in
`# systemd-ops-tags:`. Existing live units are not renamed by this
work.

## Provenance

Exact `origin_cwd` remains factual. New systemd-ops-managed units also
record, when a manifest is discoverable at create time:

```
# systemd-ops-origin-scope: speech
# systemd-ops-origin-scope-root: /path/to/speech-core
```

These comments do not grant definition ownership. Only
`# managed: systemd-ops 1` does.

Context warnings, never blocks:

1. origin_scope and caller scope both present: same id, no warning;
   different id, cross-scope warning.
2. otherwise: existing origin_cwd vs context.cwd comparison.

A caller under `/project/speech-core/src` belongs to the scope whose
preferred manifest is at
`/project/speech-core/.systemd-ops/scope.toml`. Raw cwd equality is not
used when both sides have a scope id. A legacy root may instead contain
`/project/speech-core/.systemd-ops.toml`.

## Scope root, operation home, and execution cwd

Each term names a different directory:

| Term | Meaning |
|---|---|
| scope root | project directory that contains the responsibility manifest |
| operation home | `<scope-root>/.systemd-ops/<operation-stem>/`, for advisory state associated with one owned operation |
| execution cwd | directory configured in the operation's systemd definition and used when its command runs |

An operation home is project-local storage, not an OperationSpec database.
It does not replace the unit files, systemd enablement, or runtime state.
Durable operational truth remains the canonical `.service` / `.timer`
text plus systemd's live state. OperationSpec is typed authoring input.
OperationView is a derived read of what exists.

An agent-backed operation may contain `automation.toml` directly under its
operation home:

```toml
version = 1
agent = "pr-maintainer"
parent = "managed-omp-capability"
brain_paths = [".systemd-ops/pr-driver"]
```

`parent` is a producer-consumer relation, not ownership transfer, readiness,
or scheduling. It is optional, same-scope, acyclic, and references an existing
agent-backed operation. ScopeView derives bounded parent and direct-child
summaries without copying logs or iteration history.

The brain revision covers canonical `automation.toml`, exact resolved agent
definition bytes, and only the listed `brain_paths`. It does not hash directory
listings. Deterministic operations have no automation metadata or brain
revision.

Completed lifecycle state is stored at
`<operation-home>/state/lifecycle.json`. Completion preserves the definition,
operation home, operator history, fingerprint, relations, and TUI row, but
stops and disables future timer activation. Retirement removes the managed
definition. These are deliberately different operations.

For systemd-ops-managed operations, OperationView includes
`editable_spec`: a reconstruction of every durable supported authoring
field from the unit files (and systemd enablement for `enabled`).
Project-managed operations have `editable_spec: null`.

`start_now` is create/apply intent, not durable configuration. It is
omitted from `editable_spec`. An update that omits it is `false`
(existing parse default) and does not start the unit.

## Health (v1)

Lifecycle and schedule facts only. No probes, heartbeats, log LLM,
HTTP checks, or incident state.

Operation: `healthy` | `failed` | `unknown`.

- Scheduled oneshot: never-run is `unknown`, never success. Last
  actual result failure → `failed`. Last success is `healthy` only if
  the timer is enabled and a future trigger exists; otherwise
  `unknown` (`health_basis=not-scheduled`).
- Long-running (`simple`) expected active: `active`/`running` →
  `healthy`; `failed` state → `failed`; enabled but inactive →
  `failed`; disabled and inactive → `unknown`.
- Unscheduled oneshot: never-run `unknown`; last success `healthy`;
  last failure `failed`.

This is not application-level functional correctness.

Criticality is a scope relationship, not an operation field.

## ScopeView

Derived, never persisted. CLI `scope show --json` and `systemd-ops tui`
both call the same function.

```
ScopeView
  id, root
  health: healthy | degraded | failed | unknown
  owned[]     operation views + critical flag
  watching[]  operation views
  attention[] { operation, relationship, code, reason }
  warnings[]  soft/non-fatal notes (e.g. malformed operator state)
```

```
  definition_revision   sha256 over ordered definition fragment files, or null
  operator              advisory OperatorSurface v1, or null
    version, about, headline, body, updated_at, basis_revision
    activity[{at,text}]
    active_iteration    {id,started_at,observed_updated_at}, or null
    iterations          latest 20 finished sessions, newest first:
                        {id,started_at,finished_at,exit_code,
                         reconsolidated,headline,summary}
  operator_state        missing|unbased|current|outdated|error
                        (null on watching entries)
```

Activity, active and finished iterations, and operator brief fields are
advisory. They never affect operation or scope health. Runtime is
objective state derived from systemd. Watching entries never read another
scope's operator files.

Aggregation:

| Condition | Scope health |
|---|---|
| owned critical failed | `failed` |
| owned non-critical failed | `degraded` |
| watched failed | `degraded` (relationship=`watching`) |
| owned critical unknown, and no failure | `unknown` |
| everything relevant healthy | `healthy` |

Attention is not a health value. It lists what an operator/agent
should notice. No acknowledgement lifecycle.

Watched failure is an external dependency, not something this scope
owns. Subscriptions live on the consumer manifest. Provider units do
not store subscriber lists. No notification behavior in this slice.

## TUI

`systemd-ops tui` resolves the scope by `--scope-root`, then
`SYSTEMD_OPS_SCOPE_ROOT`, then cwd discovery. No manifest is an error,
not an all-systemd dashboard.

Modes and drawers:

| key | surface | content |
|---|---|---|
| (default) | COCKPIT | description, NOW, AGENT BRIEF, RECENT ITERATIONS, NOTABLE ACTIVITY, objective RUNTIME |
| `d` | WIRING detail | identity, responsibility, execution, activation |
| `l` | DIAGNOSTICS drawer | raw journal for the selected service, loaded lazily below the current detail |

Esc or `d` returns wiring detail to cockpit; Esc or `l` closes the diagnostics
drawer. Wiring and diagnostics toggle independently. Opening the TUI or moving
the selection with the drawer closed does not fetch journald. `j`/`k` and
Down/Up change the selected operation. PageDown/PageUp scroll the selected
detail by one visible page; End/Home jump to the bottom/top. Detail scroll
resets on selection, refresh, filter rebuild, and wiring change, and clamps to
the rendered detail height. The mouse wheel routes to the list, selected detail,
or logs according to pointer region. `/` filters, `r` refreshes, and `q` or Esc
exits when no alternate surface is open.

RECENT ITERATIONS renders the active session first with `●` and
`iteration in progress`. Finished sessions follow newest first: `✓` for
exit 0, `✖` for a nonzero exit, and `?` for an interrupted session with no
exit code. A finished session without reconsolidated brief state says
`exited before reconsolidating a brief`; an interrupted one says
`interrupted before producing a final brief`.

Header: `SCOPE_ID   HEALTH` then `N owned · M watching · K attention`.

## Operator soft state

The canonical file for each owned stem is:

```
<scope-root>/.systemd-ops/<stem>/state/operator.json
```

The legacy `<scope-root>/.systemd-ops/operator/<stem>.json` path is read
only when the canonical file is absent. The next successful operator write
writes the canonical file and may remove the legacy file. If both files
exist, the canonical file wins and ScopeView carries a warning; their
contents are not merged.

Schema v1 keeps bounded operator brief and activity fields and adds
`active_iteration` plus the latest 20 finished `iterations`. Activity is
a note stream. An iteration is an explicit operator work session with a
start and finish, not a timer activation, service health check, process
run, or other systemd event. Those belong to objective runtime.

CLI:

```
systemd-ops operator show --unit STEM
systemd-ops operator set --unit STEM [--about TEXT] [--headline TEXT] [--body TEXT]
systemd-ops operator append --unit STEM --text TEXT
systemd-ops operator iteration-start --unit STEM
systemd-ops operator iteration-finish --unit STEM --iteration ID --exit-code N
systemd-ops operator clear --unit STEM
```

`set` stamps `updated_at` and `basis_revision = definition_revision`.
`append` preserves those brief stamps. Iteration start and finish update
advisory work history. Direct soft writes are intentional; they are not
systemd mutations and do not use plan/apply.

## Bound autonomous operation surface

The generic narrow CLI is:

```
systemd-ops automation context
systemd-ops automation report --headline TEXT --summary JSON_ARRAY
systemd-ops automation activity --text TEXT
```

All three commands require `SYSTEMD_OPS_OPERATION` and resolve the responsibility
scope using the normal `--scope-root`, `SYSTEMD_OPS_SCOPE_ROOT`, then cwd
precedence. There is deliberately no `--unit` argument. The environment-bound
stem must match the resolved scope's `owned` globs. Missing bindings and watched
or other stems are rejected before a write.

`automation context` is focused working context: scope identity; operation
unit, title, canonical purpose, health, operator state, and definition revision;
objective state, substate, last result, next activation, and kind; current human
report; active iteration; latest 20 finished iterations; and notable activity.
It does not return raw journal data.

`automation report` writes the compatible operator `headline` and `body`, stamps
`updated_at` and `basis_revision`, and marks the active iteration as reported.
Its strict schema is:

- `headline`: required, one non-empty line, at most 80 characters
- `summary`: required array of 1 to 5 strings
- each summary string: non-empty, no CR/LF, at most 280 characters
- stored `body`: summary strings joined by `\n\n`

`automation activity` requires an active iteration and accepts one required,
non-empty, CR/LF-free line of at most 200 characters. It is optional and does not
stamp the report or reconsolidate the iteration by itself. A finished iteration
is reconsolidated only when exit code is zero and that exact active iteration
submitted a report.

The OMP adapter describes four audiences: broad inspect for project
builders/operators/admins; lifecycle control for trusted operators/admins;
definition authoring for automation builders/admins; low-level operator state
for manual administration. Ordinary autonomous maintainers use only the three
bound automation tools. Current OMP profile filtering is launcher/session based,
not a systemd-ops agent registry: dogfood wrappers pass an explicit `--tools`
allowlist, and delegated workers receive no bound automation tools unless their
launcher explicitly grants them.

The dogfood wrapper success contract is external to systemd-ops: run OMP, finish
the iteration, require `reconsolidated: true`, then recompute and atomically
advance the external fingerprint. OMP failure, missing report, or iteration
finish failure leaves the fingerprint unchanged for a later retry.

`tests/automation-cli.sh` covers the environment binding, strict report and
activity bounds, report stamps, and reconsolidation rule against the built CLI.
`tests/wrapper-contract.sh` runs the dogfood wrapper against temporary scopes
and fake OMP executables to prove report-required success, fingerprint advance
after full success, preservation after OMP/report/finish failure, and the
unchanged-fingerprint fast path.

Deleting an operation home's operator state leaves the operation
unchanged. Operation homes do not replace unit-file operational truth.


## Deferred

- functional health probes
- generic failure-handler command / OnFailure helper units
- HCom, agent wakeup, owner-agent fields
- subscriber notifications
- retry / incident management
- `scope init` onboarding
- mass-rename of live units
- TUI mutation
