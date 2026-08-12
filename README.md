# systemd-mcpd

[![CI](https://github.com/rhaist/systemd-mcpd/actions/workflows/ci.yml/badge.svg)](https://github.com/rhaist/systemd-mcpd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![tested on systemd 255 to 261](https://img.shields.io/badge/tested%20on%20systemd-255%20to%20261-informational)](#testing)
[![MCP 2026-07-28](https://img.shields.io/badge/MCP-2026--07--28-blueviolet)](#protocol-revisions)
[![dependencies: serde, serde_json](https://img.shields.io/badge/dependencies-serde%2C%20serde__json-brightgreen)](Cargo.toml)

A [Model Context Protocol](https://modelcontextprotocol.io) server that
exposes systemd to language-model agents: what is running, what failed,
what the journal says, where boot time went, and, if you allow it, how
to change any of that.

Written in Rust, with serde and serde_json as its only dependencies: no
libsystemd linkage, no D-Bus library, no async runtime.

**Three rules, enforced in code:**

1. **Nothing is granted by default.** The server refuses to start
   without `--grant`. The authority handed to an agent is stated once,
   on the command line, where it is visible in the process table.
2. **Scopes gate both visibility and execution.** Tools outside the
   granted scopes are not advertised in `tools/list` and are refused in
   `tools/call`; an unadvertised tool must also fail when called
   directly.
3. **All writes go through plan/apply.** No tool mutates directly: a
   change is first planned (a read-only step returning current state,
   predicted state, rollback action, and a plan id), then applied by
   id. Apply re-checks the state the plan was made against and refuses
   stale plans.

**Contents:** [Quick start](#quick-start) &middot;
[Scopes](#scopes) &middot; [Tools](#tools) &middot;
[Write path](#write-path) &middot; [Errors](#errors) &middot;
[Protocol revisions](#protocol-revisions) &middot;
[Permissions](#permissions) &middot; [Backends](#backends) &middot;
[Testing](#testing) &middot; [Deployment](#deployment) &middot;
[Roadmap](#roadmap)

## Quick start

Build it, then point an MCP client at it:

```
cargo build --release
```

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

That grant is read-only. The configuration above is Claude Desktop's;
any MCP client works the same way.

### What it looks like

The agent calls `list_units` with `{"pattern": "ssh*", "state":
"active"}`:

```json
[
  {
    "unit": "ssh.service",
    "load": "loaded",
    "active": "active",
    "sub": "running",
    "description": "OpenBSD Secure Shell server"
  },
  {
    "unit": "sshd-unix-local.socket",
    "load": "loaded",
    "active": "active",
    "sub": "listening",
    "description": "OpenSSH Server Socket (systemd-ssh-generator, AF_UNIX Local)"
  }
]
```

Or `list_timers` with `{"pattern": "fstrim*"}`:

```json
[
  {
    "unit": "fstrim.timer",
    "activates": "fstrim.service",
    "next": "2026-08-16T23:04:56.413046Z",
    "last": "2026-08-12T06:40:08.922808Z"
  }
]
```

Every reply is JSON, and each of the four list tools takes a `pattern`,
because a host has several hundred units and an agent asking about one
of them should not have to read about the rest.

### Running it by hand

```
systemd-mcpd --grant <scope>[,<scope>...]
systemd-mcpd --help | --version
```

The server speaks MCP (line-delimited JSON-RPC 2.0) on stdin/stdout,
reads requests until EOF, and exits 0. Startup errors (no grant, an
unknown scope, an unknown flag) go to stderr with exit status 1. An
unknown scope is an error, not a warning.

## Scopes

| Scope          | Grants                                                   |
|----------------|----------------------------------------------------------|
| `units:read`   | unit state, properties, timers, sockets, unit files, dependency edges, security analysis |
| `journal:read` | journal entries, per unit                                |
| `boot:read`    | boot phase timings, critical chain, blame                |
| `units:write`  | unit lifecycle, enablement, and log-control changes, only through plan/apply |

Scopes are independent: granting `units:write` grants no read scope,
and neither implies the other. Grant `units:write` only where the agent
is expected to operate the machine rather than inspect it.

## Tools

| Tool                | Scope          | Returns                                     |
|---------------------|----------------|---------------------------------------------|
| `list_units`        | `units:read`   | Loaded units with load/active/sub state     |
| `failed_units`      | `units:read`   | Units currently in the failed state         |
| `unit_properties`   | `units:read`   | Properties of one unit, all or a named subset |
| `list_timers`       | `units:read`   | Timer units: next and last elapse, activated unit |
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

### Narrowing the answer

The four list tools (`list_units`, `list_timers`, `list_sockets`,
`list_unit_files`) take an optional `pattern`: a glob over the unit
name supporting `*` and `?`.

| Pattern                | Matches                                    |
|------------------------|--------------------------------------------|
| `nginx*`               | a service and its instances                |
| `systemd-*.service`    | one project's services                     |
| `*.timer`              | a unit type                                |
| `user@????.service`    | four-character instance names              |

Bracket expressions are not implemented; `[` matches itself. A pattern
that matches nothing returns an empty array rather than an error.
Filtering is worth reaching for: an ordinary host has several hundred
loaded units and as many unit files, and the unfiltered `list_units`
reply is the largest this server produces.

Unit names are validated against the unit-name character set before
they reach an argument list; malformed names are refused with an error
naming the input.

### `units:read`

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

### `journal:read`

- **`unit_logs`**: required `unit`; optional `lines` (1..1000, default
  50), `priority` (0..7; entries at that syslog priority or more
  severe), `since`/`until` (journalctl time syntax: `2026-08-12
  06:00:00`, `-5min`, `yesterday`), `boot` (offset: 0 current, -1
  previous; see `list_boots`), and `grep` (regular expression over the
  message). Entries carry `timestamp` (RFC 3339 UTC), `priority` and
  `pid` as integers, and `message` unchanged, since it can legitimately
  be a byte array for non-UTF-8 payloads.
- **`list_boots`**: no arguments.

### `boot:read`

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

### `units:write`

- **`plan_change`**: required `action` (`start`, `stop`, `restart`,
  `reload`, `enable`, `disable`, `mask`, `unmask`, `log-level`,
  `log-target`) and `unit`. The log-control actions require `value`
  (levels `emerg`..`debug`; targets `console`, `kmsg`, `journal`,
  `journal-or-kmsg`, `auto`, `null`); other actions reject it. Reads
  state and records a plan; executes nothing.
- **`apply_plan`**: required `plan`, an id from `plan_change`.

See [Write path](#write-path) for what a plan records and when an apply
is refused.

## Write path

The write path covers unit lifecycle, enablement, and log-control
changes. Nothing happens in one step. `plan_change` with
`{"action": "stop", "unit": "ssh.service"}` reads state and returns:

```json
{
  "plan": 1,
  "unit": "ssh.service",
  "action": "stop",
  "value": null,
  "current": { "active": "active", "sub": "running" },
  "predicted": { "active": "inactive" },
  "rollback": { "action": "start" },
  "note": "nothing has been executed; apply with apply_plan"
}
```

The unit is still running. `apply_plan` with `{"plan": 1}` is what
stops it, and only if `active` is still what the plan recorded. If
something else stopped or restarted the unit in between, the apply is
refused as stale and the agent has to look again.

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

Failures travel on two channels, split by whether the model can fix
them:

- **Tool errors** (`isError: true`, carrying the message) are anything
  a corrected call would resolve: invalid arguments (missing `unit`,
  out-of-range `priority`, unknown `state`), and backend failures (a
  systemctl exit status, an unreachable manager, bootup not finished).
  The session continues and the client passes the message to the
  model.
- **Protocol errors** (JSON-RPC, code -32602) are for what no argument
  fixes: an unknown tool, or one outside the granted scopes.

Argument validation lands on the tool channel deliberately. Clients are
not required to show protocol errors to the model, so reporting a
misspelled action as one hides the correction from the only party that
can make it. This is also what the specification asks for; see
[SEP-1303](https://github.com/modelcontextprotocol/modelcontextprotocol/issues/1303).

A query that matches nothing is not a failure. journalctl exits 1 when
`--grep` or a time window selects no entries; `unit_logs` returns an
empty entry list for that case and reserves `isError` for a journalctl
failure, which is distinguishable by its message on stderr.

## Protocol revisions

MCP revision 2026-07-28 removed the `initialize` handshake. A request
now carries its own protocol version and the client's capabilities in
`_meta`, every result carries a `resultType`, and `server/discover`
reports what a server speaks. Most deployed clients have not moved yet,
so this server speaks both eras:

| Era      | Revisions                                         | Opened by                     |
|----------|---------------------------------------------------|-------------------------------|
| Modern   | 2026-07-28                                        | `_meta` on every request      |
| Legacy   | 2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05    | the `initialize` handshake    |

The era is decided per request, by whether
`_meta["io.modelcontextprotocol/protocolVersion"]` is present, never by
what came earlier on the connection. The protocol is stateless and a
client may interleave unrelated requests on one process.

What a modern request gets that a legacy one does not:

- `resultType: "complete"` on every result, and the server identity in
  each result's `_meta`.
- `structuredContent`: the reply as JSON, beside the text block that
  every era receives. Older revisions have no such field, so it is
  emitted only where it is defined.
- `ttlMs` and `cacheScope` on `server/discover` and `tools/list`. The
  tool set is fixed at startup by `--grant` and cannot change while the
  process runs, which is why no `listChanged` capability is declared
  and why the freshness hint is an hour.
- A declared version this server does not speak is refused with
  `UnsupportedProtocolVersion` (-32022) listing what it does, and a
  request missing a required `_meta` field is refused with -32602.
  Legacy versions are not accepted in `_meta`: they are reachable
  through `initialize`, which is the only way to speak them.

`server/discover` is answered even when the request declares no version
at all, since a client probing to find out which era it is talking to
learns nothing from a refusal.

Not implemented, because this server exposes tools and nothing else:
resources, prompts, completion, pagination, subscriptions, tasks, MCP
Apps, and the client features (sampling, roots, elicitation). Roots,
sampling and logging are deprecated as of 2026-07-28 in any case. There
is no HTTP transport and therefore no authorization: stdio takes its
credentials from the process that spawned it.

One consequence of statelessness is worth stating plainly: plan ids
follow the pattern the specification recommends for state, an explicit
handle minted by the server and passed back as an ordinary argument,
but they live in the process's memory. A client that restarts the
server between `plan_change` and `apply_plan` gets "unknown plan" and
has to plan again.

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
one answered; the state and pattern filters are applied after either
backend so their semantics cannot diverge.

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

## Testing

Three tiers, in cost order.

| Tier                     | Needs                  | Runs                            |
|--------------------------|------------------------|---------------------------------|
| `cargo test`             | nothing                | protocol layer and parsers      |
| GitHub CI                | a push                 | fast gates, two live systemds   |
| `tests/release-check.sh` | KVM, by hand           | three systemd versions in QEMU  |

CI runs the binary against two live systemds on every push:

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
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo bash tests/integration.sh
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo bash tests/varlink-proof.sh
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
