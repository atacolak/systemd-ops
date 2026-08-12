# Tool reference

Arguments and reply shapes for every tool. The [README](../README.md)
has the summary table and the scope each tool needs.

## Narrowing the answer

`list_units`, `list_timers`, `list_sockets` and `list_unit_files` take
an optional `pattern`: a glob over the unit name supporting `*` and `?`.

| Pattern                | Matches                                    |
|------------------------|--------------------------------------------|
| `nginx*`               | a service and its instances                |
| `systemd-*.service`    | one project's services                     |
| `*.timer`              | a unit type                                |
| `user@????.service`    | four-character instance names              |

Bracket expressions are not implemented; `[` matches itself. A pattern
that matches nothing returns an empty array rather than an error. An
ordinary host has several hundred loaded units and as many unit files,
and the unfiltered `list_units` reply is the largest this server
produces.

Unit names are validated against the unit-name character set before
they reach an argument list; malformed names are refused with an error
naming the input.

## `units:read`

- **`list_units`**: optional `state`, one of `active`, `inactive`,
  `failed`, `activating`, `deactivating`, matching the unit's active
  state. Both filters are applied by the server after either backend,
  so their semantics are identical over both.
- **`failed_units`**: no arguments.
- **`unit_properties`**: required `unit`; optional `properties`, an
  array of exact property names. The full set runs to some 200
  properties and 10 KB for one service; naming three returns three. A
  name that does not exist is omitted, and a selection where none exist
  is an error naming them. The subset is selected by the server, not by
  `systemctl --property=`, which prints nothing both for a unit that
  does not exist and for a property that does not exist and would
  collapse the two cases.
- **`list_timers`**: `next` and `last` are RFC 3339 UTC timestamps,
  null for a timer that has never run or has no scheduled elapse.
  systemctl's `left` and `passed` fields are not reported: it fills
  them in only on some paths (`left` repeating the absolute `next`
  value, `passed` reading zero for timers that have run), and a wrong
  number is worse than an absent one.
- **`list_sockets`**: what each socket listens on and the unit it
  activates.
- **`list_unit_files`**: optional `state`, an enablement state
  (`enabled`, `static`, `masked`, `generated`, ...). Compared by
  equality; unit-file states are version-dependent, so an unknown value
  matches nothing rather than erroring. The `pattern` glob matches the
  unit file name.
- **`unit_dependencies`**, **`unit_security`**: required `unit`.
- **`unit_log_control`**: required `unit`. Reads through systemd's
  LogControl1 interface over D-Bus, so the service must declare
  `BusName=` and implement the interface. systemd-logind and
  systemd-resolved do; journald serves its log control over varlink
  instead and is not reachable this way, and the error says so.

## `journal:read`

- **`unit_logs`**: required `unit`; optional `lines` (1..1000, default
  50), `priority` (0..7; entries at that syslog priority or more
  severe), `since`/`until` (journalctl time syntax: `2026-08-12
  06:00:00`, `-5min`, `yesterday`), `boot` (offset: 0 current, -1
  previous; see `list_boots`), and `grep` (regular expression over the
  message). Entries carry `timestamp` (RFC 3339 UTC), `priority` and
  `pid` as integers, and `message` unchanged, since it can legitimately
  be a byte array for non-UTF-8 payloads.
- **`list_boots`**: no arguments.

A query that matches nothing is not a failure. journalctl exits 1 when
`--grep` or a time window selects no entries; `unit_logs` returns an
empty entry list for that case and reserves `isError` for a journalctl
failure, which is distinguishable by its message on stderr.

## `boot:read`

- **`boot_times`**: no arguments. Phases that did not occur (no EFI, no
  initrd) are omitted rather than reported as zero.
- **`critical_chain`**: optional `unit`, analyzing the chain to that
  unit instead of the default target. Timespans are returned verbatim
  (`1min 30.5s`); the server does not reinterpret systemd's formatting.
- **`boot_blame`**: optional `limit` (1..1000, default 25). The list is
  ordered slowest first, so the limit answers the question the tool is
  asked; the reply carries `returned` and `total`, so a truncated
  answer says it is one.

On a host that has not finished booting, `boot_times` and
`critical_chain` return a "not yet finished" error, mirroring
systemd-analyze. `boot_blame` may answer anyway with the units that
have started so far; that too mirrors systemd-analyze.

## `units:write`

- **`plan_change`**: required `action` (`start`, `stop`, `restart`,
  `reload`, `enable`, `disable`, `mask`, `unmask`, `log-level`,
  `log-target`) and `unit`. The log-control actions require `value`
  (levels `emerg`..`debug`; targets `console`, `kmsg`, `journal`,
  `journal-or-kmsg`, `auto`, `null`); other actions reject it. Reads
  state and records a plan; executes nothing.
- **`apply_plan`**: required `plan`, an id from `plan_change`.

Lifecycle actions (start, stop, restart, reload) operate on the unit's
active state; enablement actions (enable, disable, mask, unmask) on its
unit-file state; log-control actions on the service's LogControl1
value. Each plan records, and each apply re-checks, the state dimension
its action changes.

Rollback is reported in both the plan and the apply result as an
object: `{"action": ...}` with a `value` where the inverse needs one.
start/stop and enable/disable invert each other, mask inverts to
unmask, log-control actions invert to themselves with the previous
value, and restart and reload report null. The predicted state for
unmask is null: the outcome depends on the unit's install
configuration.

The apply result reports the filesystem changes systemd printed
(symlink creations and removals for enablement actions) as `changes`.
systemctl has no dry run for enablement, so these appear in the apply
result, not in the plan.

[DESIGN.md](DESIGN.md#write-path) covers what the precondition check
does and does not guarantee.
