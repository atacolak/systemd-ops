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

## Scope

A scope is a project-local identity, not a security ACL.

| Field | Meaning |
|---|---|
| `id` | stable identifier chosen once (`speech`, `personal`, `proxy`) |
| root | directory containing `.systemd-ops.toml` (not a config field) |
| owned | operation-stem globs this scope is responsible for |
| critical | explicit owned stems that fail the scope when they fail |
| watch | explicit stems this scope considers relevant but does not own |

Ownership here is operational responsibility. The hard mutation
boundary remains the configured `--write-prefix` (typically
`managed-*`). Cross-scope writes are possible and produce a warning;
they are not denied.

The scope is the durable owner identity. The manifest does not name
agents, HCom identities, OMP profiles, or a global registry.

## `.systemd-ops.toml`

Discovered by walking upward from cwd. The directory that contains the
file is the scope root.

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
```

Rules:

- `scope.id` is required, 1..64 characters, `[a-z][a-z0-9-]*`.
- `owned` is one or more globs (`*` and `?`, same matcher as write-prefix).
- `critical` is explicit stems, not globs. Each must match an owned glob.
- `watch` entries are explicit stems. A stem must not be both owned and watched.
- no `owner_agent`, `hcom_agent`, `omp_profile`, `lead`, or `manager` fields.

Once the file exists it is canonical. Agents may infer a first id and
owned prefix at project setup; they must not keep guessing afterward.

`systemd-ops scope show` and `systemd-ops scope validate` are the
minimum CLI. There is no `scope init` in this slice.

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
manifest is at `/project/speech-core/.systemd-ops.toml`. Raw cwd
equality is not used when both sides have a scope id.

## OperationSpec / OperationView

No OperationSpec database. No per-operation spec files. Durable truth
is the canonical `.service` / `.timer` text.

OperationSpec is typed authoring input. OperationView is a derived
read of what exists.

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
  actual result success → `healthy`; failure → `failed`.
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
  attention[] { operation, relationship, reason }
```

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

`systemd-ops tui` discovers the nearest manifest. No manifest: error,
not an all-systemd dashboard.

v1 is read-only: navigate, select, filter, refresh, inspect details,
inspect logs. No start/stop/restart/enable/disable, no create/edit/
retire, no OperationSpec forms, no plan dialogs.

Human mutation via the TUI is deliberately undecided and not built.

## Deferred

- functional health probes
- generic failure-handler command / OnFailure helper units
- HCom, agent wakeup, owner-agent fields
- subscriber notifications
- retry / incident management
- `scope init` onboarding
- mass-rename of live units
- TUI mutation
