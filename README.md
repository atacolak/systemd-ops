# systemd-mcpd

A [Model Context Protocol](https://modelcontextprotocol.io) server that
exposes systemd to language-model agents. Written in Rust; the
dependencies are serde and serde_json.

Three rules, enforced in code:

1. Nothing is granted by default. The server refuses to start without
   `--grant`. The authority handed to an agent is stated once, on the
   command line, where it is visible in the process table.
2. Scopes gate both visibility and execution. Tools outside the granted
   scopes are not advertised in `tools/list` and are refused in
   `tools/call`; an unadvertised tool must also fail when called
   directly.
3. All writes go through plan/apply. No tool mutates directly: a change
   is first planned (a read-only step returning current state, predicted
   state, rollback action, and a plan id), then applied by id. Apply
   re-checks the state the plan was made against and refuses stale
   plans.

## Invocation

```
systemd-mcpd --grant <scope>[,<scope>...]
systemd-mcpd --help | --version
```

The server speaks MCP (line-delimited JSON-RPC 2.0) on stdin/stdout,
reads requests until EOF, and exits 0. Startup errors (no grant, an
unknown scope, an unknown flag) go to stderr with exit status 1. An
unknown scope is an error, not a warning.

| Scope          | Grants                                                   |
|----------------|----------------------------------------------------------|
| `units:read`   | unit state, properties, timers, sockets, unit files, dependency edges, security analysis |
| `journal:read` | journal entries, per unit                                |
| `boot:read`    | boot phase timings, critical chain, blame                |
| `units:write`  | unit lifecycle, enablement, and log-control changes, only through plan/apply |

Scopes are independent. Grant `units:write` only where the agent is
expected to operate the machine rather than inspect it.

MCP client configuration (any MCP client; Claude Desktop shown):

```json
{
  "mcpServers": {
    "systemd": {
      "command": "/usr/local/bin/systemd-mcpd",
      "args": ["--grant", "units:read,journal:read,boot:read"]
    }
  }
}
```

## Tools

| Tool                | Scope          | Returns                                     |
|---------------------|----------------|---------------------------------------------|
| `list_units`        | `units:read`   | Loaded units with load/active/sub state     |
| `failed_units`      | `units:read`   | Units currently in the failed state         |
| `unit_properties`   | `units:read`   | Full property set of one unit               |
| `list_timers`       | `units:read`   | Timer units: schedules and activated units  |
| `list_sockets`      | `units:read`   | Socket units: listeners and activated units |
| `list_unit_files`   | `units:read`   | Installed unit files and enablement state   |
| `unit_dependencies` | `units:read`   | One unit's dependency edges, by relation    |
| `unit_security`     | `units:read`   | systemd-analyze exposure analysis of one unit |
| `unit_log_control`  | `units:read`   | One service's runtime log level and target  |
| `unit_logs`         | `journal:read` | Journal entries for one unit, filtered      |
| `list_boots`        | `journal:read` | Boots recorded in the journal               |
| `boot_times`        | `boot:read`    | Boot duration per phase, in microseconds    |
| `critical_chain`    | `boot:read`    | Units that gated reaching a target at boot  |
| `boot_blame`        | `boot:read`    | Units by own startup time, slowest first    |
| `plan_change`       | `units:write`  | Plan a unit state change; executes nothing  |
| `apply_plan`        | `units:write`  | Execute a plan; refuses stale plans         |

Arguments and behavior:

- `list_units`: optional `state`, one of `active`, `inactive`, `failed`,
  `activating`, `deactivating`. Matches the unit's active state. The
  filter is applied by the server, identically over both backends.
- `unit_properties`, `unit_dependencies`, `unit_security`,
  `unit_log_control`: required `unit`. `unit_log_control` reads through
  systemd's LogControl1 interface over D-Bus; the service must declare
  `BusName=` and implement the interface (systemd-logind and
  systemd-resolved do, journald serves its log control over varlink
  instead and is not reachable this way). Unit names are validated
  against the unit-name character set before they reach an argument
  list; malformed names are refused with an error naming the input.
- `list_unit_files`: optional `state`, an enablement state (`enabled`,
  `static`, `masked`, `generated`, ...). Compared by equality; unit-file
  states are version-dependent, so an unknown value matches nothing
  rather than erroring.
- `unit_logs`: required `unit`; optional `lines` (1..1000, default 50),
  `priority` (0..7; entries at that syslog priority or more severe),
  `since`/`until` (journalctl time syntax: "2026-08-12 06:00:00",
  "-5min", "yesterday"), `boot` (offset: 0 current, -1 previous; see
  `list_boots`), and `grep` (regular expression over the message).
  Entries carry `timestamp` (RFC 3339 UTC), `priority` and `pid` as
  integers, and `message` unchanged, since it can legitimately be a
  byte array for non-UTF-8 payloads.
- `list_boots`: no arguments.
- `critical_chain`: optional `unit`, analyzing the chain to that unit
  instead of the default target. Timespans are returned verbatim
  ("1min 30.5s"); the server does not reinterpret systemd's formatting.
- `boot_times`: no arguments. Phases that did not occur (no EFI, no
  initrd) are omitted rather than reported as zero.
- `failed_units`, `list_timers`, `list_sockets`, `boot_blame`: no
  arguments.
- `plan_change`: required `action` (`start`, `stop`, `restart`,
  `reload`, `enable`, `disable`, `mask`, `unmask`, `log-level`,
  `log-target`) and `unit`. The log-control actions require `value`
  (levels `emerg`..`debug`; targets `console`, `kmsg`, `journal`,
  `journal-or-kmsg`, `auto`, `null`); other actions reject it. Reads
  state and records a plan; executes nothing.
- `apply_plan`: required `plan`, an id from `plan_change`.

On a host that has not finished booting, `boot_times` and
`critical_chain` return a "not yet finished" error, mirroring
systemd-analyze. `boot_blame` may answer anyway with the units that
have started so far; that too mirrors systemd-analyze.

## Write path

The write path covers unit lifecycle, enablement, and log-control
changes.

- Lifecycle actions (start, stop, restart, reload) operate on the
  unit's active state; enablement actions (enable, disable, mask,
  unmask) operate on its unit-file state; log-control actions
  (log-level, log-target) operate on the service's LogControl1 value.
  Each plan records, and each apply re-checks, the state dimension its
  action changes.
- Log-control actions carry a `value` and predict it as the outcome.
  Their rollback is the same action with the previously observed value,
  which the plan records, so these are the one action class with an
  exact undo.
- Nothing executes at plan time. `plan_change` performs reads only.
- `apply_plan` compares the current state against the state recorded at
  plan time. On mismatch the plan is discarded with an error directing
  the client to re-plan. This is a precondition check, not a lock: the
  window between the check and the systemctl invocation is unguarded,
  as it is for any other systemctl caller.
- The apply result reports the filesystem changes systemd printed
  (symlink creations and removals for enablement actions) as `changes`.
  systemctl has no dry run for enablement, so these appear in the apply
  result, not in the plan.
- Plans are single-use, exist only in server memory for the lifetime of
  the session, and are capped at 32 with oldest-first eviction.
- Rollback is reported in both the plan and the apply result as an
  object: `{"action": ...}` with a `value` where the inverse needs one.
  start/stop and enable/disable invert each other, mask inverts to
  unmask, log-control actions invert to themselves with the previous
  value, and restart and reload report null. The predicted state for
  unmask is null: the outcome depends on the unit's install
  configuration.
- Privileges are the invoking user's. An unprivileged user can plan
  anything but apply only what polkit permits; the refusal is
  systemctl's, passed through as a tool error.
- The program contains one mutating process invocation, at the end of
  the apply path; no other code path reaches it.

## Errors

Failures travel on two channels:

- Invalid arguments (missing `unit`, out-of-range `priority`, unknown
  `state`) are JSON-RPC protocol errors, code -32602.
- Backend failures (a systemctl exit status, an unreachable manager,
  bootup not finished) are tool results with `isError: true` carrying
  the backend's message. The session continues and the client receives
  the message.

A query that matches nothing is not a failure. journalctl exits 1 when
`--grep` or a time window selects no entries; `unit_logs` returns an
empty entry list for that case and reserves `isError` for a journalctl
failure, which is distinguishable by its message on stderr.

## Permissions

The server runs with the invoking user's privileges and no others.

- `units:read`, `boot:read`: PID 1's varlink socket is
  world-connectable and systemctl works unprivileged. No setup needed.
- `journal:read`: the journal is read with the invoking user's access.
  Reading the full system journal requires membership in the
  `systemd-journal` group; the shipped unit file arranges this.

## Backends

The primary interfaces are the stable machine formats of the systemd
CLIs: `systemctl --output=json`, `systemctl show` (`Key=Value`), and
`journalctl --output=json`. There is no libsystemd linkage and no D-Bus
library; every backend call is a plain process invocation, and the
binary tolerates systemd version skew the way the CLIs do.

On systemd >= 258, unit listing and boot timestamps use PID 1's varlink
socket directly (`io.systemd.Unit.List` and `io.systemd.Manager.Describe`
on `/run/systemd/io.systemd.Manager`, verified against the v261.2
interface definitions). The client is written directly over stdlib
`UnixStream`; no varlink crate. The probe is the `connect()` itself:
any failure (no socket, older systemd, an error reply, an unfamiliar
reply shape) falls back to the CLI silently. Both backends are
normalized to the same output shape and the caller cannot tell which
one answered; the state filter is applied after either backend so the
filter semantics cannot diverge.

Journal reads stay on journalctl. The varlink journal API
(`io.systemd.JournalAccess`, systemd 260) is served by executing
journalctl, not by a bound socket, so calling it would spawn the same
process for a less capable interface.

Two tools have no machine-readable source anywhere in systemd, not
D-Bus, not varlink, not `--output=json`: `critical_chain` and
`boot_blame`. Their prose output is parsed by pure functions with tests
over captured output; those parsers are to be deleted when systemd
grows structured equivalents. `unit_dependencies` is not in that group:
it reads dependency edges from unit properties instead of scraping the
`list-dependencies` tree rendering.

There is no async runtime. The stdio transport is a pipe read in a
blocking loop; an executor would add binary size without adding
capability.

## Building

```
cargo build --release
```

## Testing

`cargo test` covers the protocol layer and the parsers. CI also
runs the binary against two live systemds on every push:

- the GitHub runner itself (systemd 255, PID 1, live journal): every
  tool end to end over the CLI backend, including a transient canary
  unit whose log line must round-trip through `unit_logs`, and
  `systemd-analyze verify` of the shipped unit file;
- a Fedora container booted with systemd >= 258 as PID 1: the varlink
  backend. Each backend is made the only one available: the socket is
  renamed to force the CLI, and the server is run with an empty `PATH`
  so it cannot execute systemctl. A differential check then asserts
  both backends emit identical row shapes.

Both suites take `MCPD` (how to run the server) and `HOST` (how to
reach the target systemd, empty for this machine). They need root to
create transient units, write a unit file, and read the system
journal:

```
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo -E bash tests/integration.sh
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo -E bash tests/varlink-proof.sh
```

`varlink-proof.sh` also needs a systemd new enough to serve the
socket, and renames it for the duration of the CLI half; `/run` is a
tmpfs, so an interrupted run costs the socket until reboot and nothing
on disk.

`tests/release-check.sh` is the heavy tier, run by hand before tagging.
It runs the fast gates and both suites on the host, then boots
disposable QEMU guests (Debian 13, Fedora 43, Arch) and runs them again
inside each, which is the only way to reach three properties: the
initrd boot phase, enablement surviving a reboot, and three systemd
versions straddling the release where the varlink socket appears (257,
258, 261). Firmware and loader timestamps stay zero even there, since
those come from EFI variables only systemd-boot sets. It needs
`qemu-system-x86`, `qemu-utils`, `ovmf`, `genisoimage`, `curl`, `jq`,
and `/dev/kvm`, and takes `--tag vX.Y.Z` to tag on success.

## Deployment

`systemd-mcpd.service` runs the server under `DynamicUser` with
`ProtectSystem=strict`, a system-service syscall filter, and an empty
capability bounding set. CI verifies the unit file. When an MCP client
spawns the server directly (the common case), the unit file is not
needed; the hardening applies when you wrap the server in a service.

## Roadmap

The remaining work tracks systemd's varlink subsystem. Unit listing and
boot timestamps are native today; journal reads and the analyze verbs
still fork the CLIs because systemd offers no socket-served equivalent.
As systemd extends its varlink interfaces, the corresponding CLI
invocations here are planned to be replaced with socket calls behind
the existing probe-and-fall-back mechanism, keeping the output shapes
unchanged.

Also under consideration: the manager-wide log level (`systemctl
log-level`, unit-less and therefore outside the current plan machinery)
and journald maintenance (rotate, flush; vacuum deletes data and needs
more than a precondition check).

## License

MIT, see [LICENSE](LICENSE).
