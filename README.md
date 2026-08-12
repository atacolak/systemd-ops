# systemd-mcpd

[![CI](https://github.com/rhaist/systemd-mcpd/actions/workflows/ci.yml/badge.svg)](https://github.com/rhaist/systemd-mcpd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![tested on systemd 255 to 261](https://img.shields.io/badge/tested%20on%20systemd-255%20to%20261-informational)](docs/TESTING.md)
[![MCP 2026-07-28](https://img.shields.io/badge/MCP-2026--07--28-blueviolet)](docs/DESIGN.md#protocol-revisions)
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
   `tools/call`.
3. **All writes go through plan/apply.** No tool mutates directly: a
   change is planned first, read-only, then applied by id, and the
   apply is refused if the state moved in between.

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

The agent calls `list_units` with `{"pattern": "ssh*", "state":
"active"}` and gets:

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

Every reply is JSON, and the list tools take a `pattern` glob, because
a host has several hundred units and an agent asking about one of them
should not have to read about the rest.

To run the server by hand:

```
systemd-mcpd --grant <scope>[,<scope>...]
systemd-mcpd --help | --version
```

It speaks MCP (line-delimited JSON-RPC 2.0) on stdin/stdout, reads
requests until EOF, and exits 0. Startup errors (no grant, an unknown
scope, an unknown flag) go to stderr with exit status 1.

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

Arguments and reply shapes: [docs/TOOLS.md](docs/TOOLS.md).

## Changing something

`plan_change` with `{"action": "stop", "unit": "ssh.service"}` reads
state and returns:

```json
{
  "plan": 1,
  "unit": "ssh.service",
  "action": "stop",
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

Plans are single-use and live in the server's memory, so a client that
restarts the server between the two steps has to plan again. The server
runs with your privileges: an unprivileged user can plan anything but
apply only what polkit permits.

## Errors

Anything a corrected call would fix is a tool error (`isError: true`)
carrying the message, so the model can see it and retry: bad arguments,
a systemctl exit status, an unreachable manager. Protocol errors
(-32602) are reserved for what no argument fixes, an unknown tool or
one outside the granted scopes.

A query that matches nothing is not a failure. `unit_logs` returns an
empty entry list when a filter selects no entries.

## Permissions

The server runs with the invoking user's privileges and no others.

- `units:read`, `boot:read`: PID 1's varlink socket is
  world-connectable and systemctl works unprivileged. No setup needed.
- `journal:read`: the journal is read with the invoking user's access.
  Reading the full system journal requires membership in the
  `systemd-journal` group; the shipped unit file arranges this.

## Deployment

Normally an MCP client spawns the binary itself and owns both ends of
the pipe, and no unit file is involved.

For the case where you want a supervised instance on the machine
instead, `systemd-mcpd.socket` and `systemd-mcpd@.service` provide one
per connection. The framing is one JSON-RPC message per line over a
stream, which works the same over a Unix socket as over a pipe, so the
socket unit sets `Accept=yes` and each connection gets its own hardened
instance under `DynamicUser` with an empty capability bounding set.

```
systemctl enable --now systemd-mcpd.socket
socat - UNIX-CONNECT:/run/systemd-mcpd.sock
```

The socket is mode 0600. Anything that can open it gets the scopes the
service grants, so widen that deliberately or not at all. A plain
long-running service is the wrong shape for this program: with no
client on the other end of stdin it reads EOF and exits before doing
anything.

## Installing

```
make
sudo make prefix=/usr/local install
```

This installs the binary, the man page, the sample unit file, the docs
and the license, and honors `DESTDIR` and the usual directory
variables. Packagers: [docs/PACKAGING.md](docs/PACKAGING.md) has the
toolchain floor, the offline build, and what is deliberately absent.

## Documentation

- [docs/TOOLS.md](docs/TOOLS.md): every tool's arguments and reply shape
- [docs/DESIGN.md](docs/DESIGN.md): backends, protocol revisions, write
  path guarantees, roadmap
- [docs/TESTING.md](docs/TESTING.md): the three test tiers and the
  conformance suite
- [docs/PACKAGING.md](docs/PACKAGING.md): what a distribution packager
  needs
- [CHANGELOG.md](CHANGELOG.md): what changed per version
- [AGENTS.md](AGENTS.md): working notes for coding agents

## License

MIT, see [LICENSE](LICENSE).
