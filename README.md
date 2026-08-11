# systemd-mcpd

A [Model Context Protocol](https://modelcontextprotocol.io) server for systemd.
Read-only. Capability-scoped. Two dependencies. 543 KB.

PID 1 as a tool call — but only the tools you explicitly handed over.

## Why

Agents are starting to operate Linux machines. Today they do it by running
shell commands and parsing human-oriented output, which means every agent
session is a root shell with vibes. This server is an experiment in the
alternative: a **typed, scoped, auditable** interface between a model and
the init system.

Three positions, enforced in code:

1. **Nothing is granted by default.** The server refuses to start without
   `--grant`. Authority handed to a model should be a deliberate act that
   appears in your process table, not a default someone forgot to narrow.
2. **Scopes gate both visibility and execution.** Ungranted tools are not
   advertised in `tools/list` *and* are refused in `tools/call`. Hiding a
   tool is UX; refusing the call is security.
3. **Read-only is structural, not configurable.** There are no write scopes
   in v0.1. Not because writes are out of scope forever, but because a safe
   write path needs plan/apply/rollback semantics — and shipping mutation
   without them would be exactly the thing this project exists to argue
   against.

## Usage

```console
$ systemd-mcpd --grant units:read,journal:read,boot:read
```

Claude Desktop / any MCP client:

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

Every scope is independent — grant less if you want less.

Then ask your model: *"which units failed since boot, and why?"* — it will
find `failed_units`, pull `unit_logs` for each, and answer from structured
data instead of scraping `systemctl status` prose. Or *"what made boot
slow?"* — that lands on `boot_times` and `critical_chain`.

Note that `journal:read` sees exactly what the invoking user can read:
run as a member of the `systemd-journal` group to read the full journal
(the shipped unit file already arranges this).

## Tools

| Tool              | Scope          | Does                                        |
|-------------------|----------------|---------------------------------------------|
| `list_units`      | `units:read`   | All loaded units, optional state filter     |
| `failed_units`    | `units:read`   | Units currently failed                      |
| `unit_properties` | `units:read`   | Full property set of one unit               |
| `unit_logs`       | `journal:read` | Recent journal entries for one unit         |
| `boot_times`      | `boot:read`    | Boot duration split by phase, in µs         |
| `critical_chain`  | `boot:read`    | Units that gated reaching a target at boot  |

## Design notes

- **Backend:** the stable JSON interfaces of `systemctl --output=json` and
  `journalctl -o json`. No libsystemd linkage, no D-Bus library: every call
  the server makes is a plain, auditable process invocation, and the binary
  survives systemd version skew the way the CLIs do.
- **Native varlink where systemd serves it.** On systemd ≥ 257 unit listing
  talks straight to PID 1's varlink socket (`io.systemd.Manager`) — spoken
  by ~90 lines of stdlib `UnixStream`, no varlink crate. The probe is the
  `connect()` itself: any surprise — no socket, old systemd, unfamiliar
  reply shape — falls back to `systemctl` silently, with the same JSON
  shape either way, so the caller never learns which backend answered.
- **No async runtime.** MCP's stdio transport is line-delimited JSON-RPC on
  a pipe. A blocking read loop is the honest shape of that; tokio would be
  most of the binary for none of the benefit.
- **Input validation before argv.** Unit names are checked against the unit
  name grammar before touching an argument list — not because `Command` is
  shell-injectable (it isn't), but because a model deserves a precise error
  over a confusing one, and defense in depth is free here.
- **Journal entries are pruned** to timestamp/priority/message/pid. Handing
  a model forty metadata fields per line is how context windows die. What
  survives is typed: RFC 3339 timestamps, integer priority and pid.
- **One prose parser, fenced.** `critical_chain` is the single tool with no
  machine-readable source anywhere in systemd — not D-Bus, not
  `--output=json`. Its output is parsed by a pure function with tests over
  captured output, and that function gets deleted the day systemd grows a
  structured equivalent. `boot_times` needs no such apology: it reads the
  same manager timestamps `systemd-analyze time` reads, via `systemctl
  show`'s stable `Key=Value` output.

## Building

```console
$ cargo build --release
```

Dependencies: `serde`, `serde_json`. That's the list.

## Deployment

`systemd-mcpd.service` ships with the full hardening buffet
(`DynamicUser`, `ProtectSystem=strict`, syscall filtering, empty capability
bounding set). systemd confining the thing that talks to systemd is not
irony, it's layering.

## Roadmap

- more varlink as systemd grows it: unit listing is native today; journal
  reads and boot analysis still fork the CLIs because systemd serves them
  no other way yet
- the hard one: a write path with plan/apply semantics, structured diffs,
  and generation rollback. If that sentence sounds like a config management
  manifesto, yes. That's the point.

## License

MIT — see [LICENSE](LICENSE).
