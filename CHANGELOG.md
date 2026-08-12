# Changelog

Versions are the crate versions in `Cargo.toml`. Dates are the release
dates; entries describe what changed on the wire or on disk, since that
is what a downstream package has to care about.

## 0.6.0, 2026-08-12

On disk: `systemd-mcpd.service` is replaced by `systemd-mcpd.socket` and
`systemd-mcpd@.service`. The old unit started a stdio program with no
client on the other end of standard input, so it read EOF and exited 0
having done nothing. The socket unit sets `Accept=yes` and mode 0600,
and instantiates the template per connection. Packages install both and
should enable neither.

Wire: `unit_properties` and `plan_change` now report "no such unit" for
a name the manager does not know. `systemctl show` synthesizes such a
unit with `LoadState=not-found` and exit 0, so both previously answered
as though it existed. Unit names may now begin with `-`, which
`-.mount` and `-.slice` require.

- Speaks MCP revision 2026-07-28 alongside the `initialize` handshake
  era. A request declaring `io.modelcontextprotocol/protocolVersion` in
  `_meta` is served under the new revision; one without it is served as
  before. Adds `server/discover`, `resultType` on modern results,
  `structuredContent`, caching hints on `server/discover` and
  `tools/list`, and `UnsupportedProtocolVersion` (-32022).
- 2025-11-25 added to the advertised handshake revisions.
- Argument validation errors moved from JSON-RPC protocol errors to
  tool errors (`isError: true`), per SEP-1303. Clients are not required
  to show protocol errors to the model.
- Legacy replies are unchanged.

## 0.5.0, 2026-08-12

- `list_units`, `list_timers`, `list_sockets` and `list_unit_files`
  take a `pattern` glob over the unit name.
- `unit_properties` takes a `properties` array and returns only those.
- `boot_blame` takes `limit` (default 25) and reports `returned` and
  `total`.
- `list_timers` reports `next` and `last` as RFC 3339 timestamps and no
  longer reports systemctl's `left` and `passed`, which it fills in
  only on some paths.

## 0.4.1, 2026-08-11

- `unit_logs`: a filter matching no entries is an empty result rather
  than an error. journalctl exits 1 for that case.
- `list_units`: the row shape no longer depends on which backend
  answered or on whether a job was queued.

## 0.4.0, 2026-08-11

- `unit_logs` takes `priority`, `since`, `until`, `boot` and `grep`.
- New tools: `list_boots`, `unit_log_control`.
- `plan_change` gains the `log-level` and `log-target` actions.

## 0.3.0, 2026-08-10

- `plan_change` gains the enablement actions: enable, disable, mask,
  unmask.

## 0.2.0, 2026-08-10

- The write path: `plan_change` and `apply_plan` behind `units:write`,
  with stale-plan refusal.
- New read tools: timers, sockets, unit files, dependencies, security
  analysis.

## 0.1.0, 2026-08-09

- First version: unit state, properties and journal reads over MCP,
  behind `units:read`, `journal:read` and `boot:read`.
- Native varlink backend for unit listing and boot timestamps on
  systemd 258 and later, falling back to the command-line tools.
