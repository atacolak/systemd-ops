# Inspect reads not write-prefix Technical Spec

**Design Brief:** `/home/sf/worlds/personal/designs/systemd-ops/inspect-reads-not-write-prefix.md`
**Bead:** `sdo-inspect-read-write-prefix-du5`
**Status:** sound
**Repo:** `/home/sf/workspace/systemd-ops` (HEAD `4e926a529dc99d3df3df57ce81640d49479bf31b` on `main`)

## Intent (from the Brief — do not rewrite)

> Read inspect must follow this session's owned/visible operations, not the write-prefix allowlist. Write tools stay gated on write-prefix. Docs/schema must match behavior.

Success criteria (quoted):

- From cwd `/home/sf/worlds/personal`, `get_operation` for `managed-personal-youtube-poll`, `managed-personal-youtube-summary`, and `managed-personal-wa-harvest` returns the operation view (same class of fields as for `ata-cheapeats-scrape` / `managed-omp-pr-9969`).
- `list_operations pattern=managed-personal-*` returns those owned personal operations, not `[]`.
- `list_operations pattern=managed-memory-*` is no longer silently empty when matching units exist.
- `list_units` / `get_unit` / `unit_logs` / `list_timers` / `scope_show` behavior for those units remains working.
- Control/author/apply writes remain refused for units outside write-prefix.
- Docs and MCP/CLI descriptions no longer claim `list_operations`/`get_operation` are write-prefix inventory.
- Related: `operator_show` with no unit currently errors `operator unit must be a bare stem without suffix; got ''`. If that is the same inspect-read class of bug and a tiny bounded fix, include it; otherwise leave a note. Do not expand into operator redesign.

Constraints (quoted):

- Do not change write-prefix semantics for plan/apply / control / author.
- Do not widen `~/.config/systemd-ops/config.toml` write_prefix to `managed-*` as the fix. That would grant writes over personal units.
- Do not implement the youtube 413 job in this bead.
- Prefer documentation clarification only if code already matches the Want; observed behavior does not.
- Consolidation 0.7.0 pass is in flight on the same repo. Do not absorb unrelated dirty `.ai-bridge/` or `.systemd-ops/scope.toml`. Do not revert dogfood/docs commits.

In scope (quoted): inspect `get_operation` / `list_operations` (and matching CLI) read gating; docs/schema; tests proving personal stems are readable while writes stay prefix-gated; optional tiny `operator_show` missing-unit error if clearly the same class.

Out of scope (quoted): youtube 413; write-prefix redesign; making systemd-ops a managed scope; publication; auto-retire.

User-visible behavior (quoted):

> An inspect caller in a personal scope can name an owned `managed-personal-*` operation and get its view. Patterned list_operations returns matching existing operations even when they are outside write-prefix. Attempts to plan-update or control those units still refuse unless write-prefix matches.

Planner open question from the Brief (quoted), settled by the Brief's preference plus the lead packet:

> Whether `get_operation` of a unit that exists but is neither owned by the resolved scope nor matching write-prefix should still return (general inspect) or require scope visibility. Prefer general inspect (same as `get_unit`) unless a security invariant in code forbids it.

## Mapping onto the current system

Claims below are labeled **observed** / **inferred** / **unknown**.

### Local configuration and personal scope (observed)

- `~/.config/systemd-ops/config.toml` has `write_prefix = "ata-*,managed-omp-*"`.
- Personal world `/home/sf/worlds/personal/.systemd-ops.toml` is `id = "personal"`, `owned = ["managed-personal-*"]`.
- Repo HEAD is `4e926a5` on `main` (0.7.0 consolidation in flight, unpublished, ahead of origin). Untracked `.ai-bridge/`, `.beads/`, `.systemd-ops/`, `tests/pr-merged-complete.sh` are not this bead.

### Write-prefix is a mutation gate (observed)

- `src/systemd.rs` `write_unit_allowed` / `require_write_unit`: no prefix means writes refused; a prefix restricts writes to matching unit names.
- `src/write.rs` `plan` calls `require_write_unit` before planning lifecycle.
- `src/operations.rs` `validate_stem` (authoring) requires `is_operation_stem` and `require_write_unit`. `is_operation_stem` is `validate_unit_name` plus `write_unit_allowed` on `{stem}.service`.
- CLI `--write-prefix` help already says `restrict writes; no prefix means writes refused` (`src/bin/systemd_ops.rs` USAGE). MCP author/control tool descriptions still correctly say write-prefix operations.
- README rule 1: no prefix means reads still work and writes are refused.

### Inspect reads currently reuse that mutation gate (observed)

- `get_operation` (`src/operations.rs` 1640–1654) name-validates, then errors `not a '{write_prefix}' operation: {name}` when `is_operation_stem` fails, else calls `get_operation_any`.
- `get_operation_any` (1656–) looks up a stem via `{stem}.service` / `{stem}.timer` / `{stem}*` (and also extends inventory with write-prefix globs, which is collection not a gate). Empty constituents → `no such operation: {stem}`.
- `list_operations` `inventory_globs` (1561–1571): when write-prefix is set, returns those globs and **ignores** the caller pattern except as a later `stems.retain` post-filter. So `pattern=managed-personal-*` against prefix `ata-*,managed-omp-*` inventories the prefix, then filters to personal stems, yielding `[]`.
- `list_operations` also `.filter(|s| is_operation_stem(s))` (1602), a second write-prefix gate on the result set.
- When write-prefix is unset, `inventory_globs` uses the caller pattern, or errors `list_operations requires a pattern when --write-prefix is not set`.

### General inspect already exists beside the gated wrappers (observed)

- `list_units` / `get_unit` / `list_timers` / `unit_logs` take a name or pattern and do not consult write-prefix (`src/systemd.rs`). `get_unit` of a loaded unit returns the unit view or `no such unit`.
- `list_operations_matching(globs)` (1618–1638) inventories those globs with **no** `is_operation_stem` filter. `scope_show` uses it for `manifest.owned` (`src/scope.rs` 368–369).
- `get_operation_any` is already used by `operator` revision, `scope_show` watch rows, and `automation` inspect — not by MCP/CLI inspect.
- MCP `list_operations` / `get_operation` call the gated wrappers (`src/mcp.rs` 585–586). CLI `inspect list-operations` / `inspect get-operation` do the same (`src/bin/systemd_ops.rs` 454–462).
- OMP `systemd_inspect` already documents a general read surface (`omp/systemd.ts`) and shells `systemd-ops --json` with session cwd. `inspectArgv` for `list_operations` / `get_operation` is already the CLI inspect path.

### No security invariant forbids general inspect reads (observed)

- `docs/SCOPES.md`: “A scope is a project-local identity, not a security ACL.” Ownership is operational responsibility; “The hard mutation boundary remains the configured `--write-prefix`.”
- `SECURITY.md` claims: MCP grants, plan/apply as the only mutation, no shell, no extra privilege. It does not claim write-prefix as a read ACL.
- `get_unit` has no owned-scope or write-prefix check. The Brief prefers the same for `get_operation`.

### Docs still describe the gated inventory (observed)

- MCP `list_operations` description: “List operations matching --write-prefix…”
- MCP `get_operation` description: “One write-prefix operation by stem…”
- `docs/TOOLS.md`: `list_operations` “Aggregates unit files matching `--write-prefix`”; “Without `--write-prefix`, a pattern is required.”
- `src/operations.rs` module comment: “The read path covers every unit file matching `--write-prefix`.”
- CLI `--write-prefix` help does **not** need a product change; it already restricts writes.

### `operator_show` empty unit (observed) — leftover, not this class

- `operator::validate_stem` errors `operator unit must be a bare stem without suffix; got '{stem}'` for empty / dotted / slashed stems (`src/operator.rs` 172–176).
- CLI `operator show` requires `--unit`. The `got ''` path is OMP `inspectArgv` for `operator_show` always passing `--unit` as `String(args.unit ?? "")` (`omp/systemd.ts` 447–448).
- This is missing-argument validation on the operator surface, not write-prefix gating of inspect inventory. **Leave it.** Do not include in this spec's implementation.

### 0.7.0 in-flight (observed)

- Do not revert HEAD through the consolidation commits. Do not absorb untracked `.ai-bridge/` or `.systemd-ops/`. This bead is inspect read gating only.

## Architecture

Inspect `get_operation` and `list_operations` (CLI and MCP) become read surfaces over existing operation stems, the same way `get_unit` / `list_units` are read surfaces over units.

Write-prefix stays the mutation allowlist: `require_write_unit`, `validate_stem` / `is_operation_stem`, control `plan`, author plan/apply. Local `write_prefix` is not widened.

`get_operation` is general inspect: name-validate, then `get_operation_any`. A stem that exists returns the same operation-view class as prefix stems. A missing stem is `no such operation`, not `not a '{write_prefix}' operation`.

`list_operations`:

- If the caller supplies `pattern`, that glob is the inventory (existing operations matching the pattern), not write-prefix. This is what makes `pattern=managed-personal-*` and `pattern=managed-memory-*` return matching units when they exist.
- If there is no pattern, inventory is the resolved scope's `owned` globs (`list_operations_matching`, already used by `scope_show`). CLI inspect honors global `--cwd` / `--scope-root` / `SYSTEMD_OPS_SCOPE_ROOT` the same way `scope show` does. MCP uses process cwd / env (no inspect `context.cwd` today).
- If there is no pattern and no resolved scope, error that a pattern is required. Do **not** fall back to write-prefix. That fallback is the current bug.

`list_units` / `get_unit` / `unit_logs` / `list_timers` / `scope_show` are unchanged.

## Components and interfaces

### `src/operations.rs`

Keep:

- `is_operation_stem` / `validate_stem` for **authoring** only.
- `get_operation_any` as the ungated lookup (already exists).
- `list_operations_matching` as the ungated list (already exists).

Change:

- Replace `inventory_globs` with a pure `read_inventory_globs(pattern, owned) -> Result<Vec<String>, BackendError>` that never reads write-prefix.
- `list_operations(pattern, explicit_root, cwd)`:
  - validate pattern length (existing 1..256 rule);
  - if `pattern` is `Some`, globs = that pattern;
  - else resolve scope (`scope::resolve` with explicit root, `SYSTEMD_OPS_SCOPE_ROOT`, cwd); if owned globs are non-empty, use them; if resolve fails because no manifest was found walking up, error that a pattern is required; other resolve errors propagate;
  - return `Value::Array(list_operations_matching(&globs)?)`.
  - **Do not** filter with `is_operation_stem`.
- `get_operation(name)`: keep existing name validation and `stem_of`; **remove** the `is_operation_stem` error; call `get_operation_any`.

Do not change `get_operation_any`'s extra write-prefix glob extend. `operation_view` already keeps constituents for the requested stem only (**inferred** leftover, not a gate).

### `src/bin/systemd_ops.rs`

- `inspect list-operations` must pass global `--scope-root` and resolved `--cwd` into `list_operations` so unpatterned list follows the same session as `scope show`.
- `inspect get-operation` stays a name lookup (no scope argument).
- USAGE `--write-prefix` line stays “restrict writes”.

### `src/mcp.rs`

- `list_operations` handler: `operations::list_operations(pattern_arg(args), None, None)`.
- `get_operation` handler: still `operations::get_operation` (now ungated).
- Tool descriptions: inspect tools are not write-prefix inventory. Author/control descriptions that mention write-prefix stay.

### Docs

- `docs/TOOLS.md` `list_operations` / `get_operation` paragraphs.
- `CHANGELOG.md` 0.7.0 (not published): inspect reads no longer use write-prefix as inventory.
- `src/operations.rs` module comment.
- Optional one-line `docs/SCOPES.md` only if needed so inspect unpatterned list is not still implied to be prefix-only. Do not rewrite the scope model.

### Tests

- Cargo tests for `read_inventory_globs` (no systemd). Existing `validate_stem` / `refuses_non_matching_stem` must still refuse outside write-prefix.
- Live CLI proof with a **disposable** user unit whose stem is outside the test write-prefix (`ata-*`). Do not mutate live personal youtube units. Do not widen the operator config file.

## Data / control flow

```
inspect get_operation(name)
  validate_unit_name
  stem = stem_of(name)
  get_operation_any(stem)     # no write_unit_allowed
    collect_inventory(stem files)
    operation_view or "no such operation"

inspect list_operations(pattern?, scope-root?, cwd?)
  if pattern:
    globs = [pattern]
  else:
    resolve scope → owned globs, or error "pattern required"
  list_operations_matching(globs)   # no is_operation_stem

control plan / author plan-* / apply
  require_write_unit / validate_stem   # unchanged
```

OMP `systemd_inspect` is already this CLI. No adapter argv change.

## Error handling

| Call | Today (prefix `ata-*,managed-omp-*`) | After |
|---|---|---|
| `get_operation` personal stem that exists | `not a 'ata-*,managed-omp-*' operation: {name}` | operation view |
| `get_operation` missing stem (any prefix) | prefix error if outside allowlist; else `no such operation` | `no such operation: {stem}` |
| `get_operation` existing stem neither owned nor prefix | prefix error | operation view (general inspect, same as `get_unit`) |
| `list_operations pattern=managed-personal-*` | `[]` | matching existing operations |
| `list_operations` no pattern, personal scope resolved | prefix inventory (`ata-*`, `managed-omp-*`) | owned inventory (`managed-personal-*`) |
| `list_operations` no pattern, no scope | prefix inventory if prefix set; else pattern required | error: pattern required (do not use write-prefix) |
| `control plan` / `author plan-update` outside prefix | `forbidden` via `require_write_unit` / `validate_stem` | unchanged |

Do not invent a new error code. Existing `json::classify` maps `no such` → `not_found`, `restricted`/`must match`/`refused` → `forbidden`.

## Testing (behavioral contracts; exact tests live in the plan)

1. **Glob selection (cargo, no systemd):** with write-prefix `ata-*,managed-omp-*`, `read_inventory_globs(Some("managed-personal-*"), …)` is `["managed-personal-*"]`. Unpatterned with owned `["managed-personal-*"]` returns that owned list. Unpatterned with no owned errors requiring a pattern. Caller pattern wins over owned (`managed-memory-*` vs owned personal).
2. **Writes still gated (cargo):** `validate_stem("managed-personal-youtube-poll")` with write-prefix `ata-*,managed-omp-*` is still `Err`.
3. **Live CLI (disposable unit, write-prefix `ata-*`):** drop `managed-inspect-read-<pid>.service` under the user unit dir, daemon-reload. `inspect get-operation` returns a view with `"unit"` equal to the stem. `inspect list-operations --pattern 'managed-inspect-read-*'` includes it. Unpatterned `inspect list-operations` with `--cwd`/`--scope-root` at a temp scope whose `owned` is that glob includes it. `control plan --action start` and `author plan-update` for that stem are `forbidden`. `inspect get-unit` still works. Cleanup the disposable unit. Do not use live youtube units as the only proof (host-specific); personal-cwd youtube stems remain the operator acceptance check for the lead/verifier.
4. **Docs:** MCP and TOOLS.md no longer call `list_operations`/`get_operation` write-prefix inventory. Author/control text may still say write-prefix.

## Non-goals

- Youtube poll HTTP 413.
- Widening `write_prefix` to `managed-*` or `managed-personal-*`.
- Changing write-prefix semantics for control/author/apply.
- Making systemd-ops a managed scope; publication; auto-retire.
- Reverting 0.7.0 consolidation commits; absorbing untracked `.ai-bridge/` or `.systemd-ops/`.
- Gating `get_operation` on scope `owned` (rejected: Brief prefers general inspect; `get_unit` has no such gate; scope is not a security ACL).
- Filtering patterned `list_operations` to owned-only (would make `pattern=managed-memory-*` empty again when those units exist).
- Defaulting unpatterned `list_operations` to write-prefix (would keep docs claiming prefix inventory).
- Operator redesign. **`operator_show` with missing/empty unit is a leftover:** not write-prefix inspect inventory; OMP always passes `--unit` and an empty string hits `validate_stem`. Out of this bead.
- New MCP `context.cwd` on inspect tools.
- Renaming `get_operation_any` / collapsing it.

## Implementation approach chosen (and rejected internals)

**Chosen (Brief approach A):** decouple inspect inventory from write-prefix. Reuse `get_operation_any` and `list_operations_matching`. Patterned list uses the caller pattern. Unpatterned list uses resolved owned globs. Writes stay on `write_unit_allowed`.

Rejected internals (all fail the Brief or add unrequested product):

- **Widen local write_prefix** — Brief forbids; grants writes over personal units.
- **Docs-only** — code does not match the Want.
- **Keep unpatterned list as write-prefix default** — operator-visible prefix inventory remains; docs would still describe it.
- **Require scope visibility on `get_operation`** — contradicts the Brief's general-inspect preference; `get_unit` and SECURITY/SCOPES do not forbid general reads.
- **New read-prefix config** — invented product.

YAGNI: no new types, no new tools, no OMP argv redesign, no operator_show change.

## Open questions

None. Product want is settled. `operator_show` empty-unit is leftover, not blocking.
