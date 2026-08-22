# Changelog

Versions are the crate versions in `Cargo.toml`. Dates are the release
dates; entries describe what changed on the wire or on disk, since that
is what a downstream package has to care about.

## unreleased (not published)

The crate is `systemd-ops`. Direct CLI `systemd-ops` is the default
frontend. Optional `systemd-ops-mcp` still speaks MCP over stdio.
Process-local numeric plan ids are gone; plan/apply uses HMAC-sealed
`plan_token` values with short expiry and stale/precondition checks.
The token payload includes the systemd manager; apply rejects a
user-token on the system manager and the reverse. No nonce ledger.
Writes without a configured write-prefix are refused. There is no
implied unit namespace. New authored units carry `# managed: systemd-ops 1`.
Packaging files are `systemd-ops-mcp.1`, `systemd-ops-mcp.socket`, and
`systemd-ops-mcp@.service`. OperationView exposes exec/cwd/schedule,
timer enablement, and never-run as `last_result: null`. CLI and MCP
default manager is user. Live PID-1 suites pass `--manager system`.
`ExecStart` argv refuses only NUL and newlines; `$` and `%` are quoted
so they survive systemd expansion. LICENSE is standard MIT; NOTICE
records the systemd-mcpd ancestry.




## 0.6.0, 2026-08-12

On disk: `systemd-mcpd.service` is replaced by `systemd-mcpd.socket` and
`systemd-mcpd@.service`. The old unit started a stdio program with no
client on the other end of standard input, so it read EOF and exited 0
having done nothing. The socket unit sets `Accept=yes` and mode 0600,
and instantiates the template per connection. Packages install both and
should enable neither.

Wire: `critical_chain` returns the whole tree again; under `LC_ALL=C`,
which the server now pins, systemd-analyze draws it in ASCII and the
parser read only the box-drawing form, reducing every chain to its
root. `boot_times` no longer reports a fabricated kernel phase in a
container, and marks the reply `container: 1`. `unit_dependencies`
reports "no such unit" instead of answering with fifteen empty
relations. `unit_logs` keeps answering for units that no longer exist,
because the journal outlives them, but carries a `note` when there is
nothing to show and no such unit is loaded. The varlink and CLI
backends agree again on `description` for units without an explicit
`Description=`.

Protocol: a JSON-RPC batch, or any line of valid JSON that is not an
object, is answered with -32600 instead of silence. A line that is not
valid UTF-8 is answered with -32700 and the session continues, where
before it ended the session with a message on stderr.

`unit_properties` and `plan_change` report "no such unit" for a name
the manager does not know. `systemctl show` synthesizes such a unit
with `LoadState=not-found` and exit 0, so both previously answered as
though it existed. Unit names may begin with `-`, which `-.mount` and
`-.slice` require.

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
