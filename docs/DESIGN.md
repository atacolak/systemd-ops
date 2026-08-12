# Design

How the server talks to systemd, how it talks to MCP clients, and what
the write path does and does not guarantee. For arguments and reply
shapes see [TOOLS.md](TOOLS.md); for how any of this is verified see
[TESTING.md](TESTING.md).

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

## Write path

Nothing mutates without a plan, and the program contains exactly one
mutating process invocation, at the end of the apply path. No other
code path reaches it.

- Nothing executes at plan time. `plan_change` performs reads only.
- `apply_plan` compares the current state against the state recorded at
  plan time. On mismatch the plan is discarded with an error directing
  the client to re-plan. This is a precondition check, not a lock: the
  window between the check and the systemctl invocation is unguarded,
  as it is for any other systemctl caller.
- Plans are single-use, exist only in server memory for the lifetime of
  the process, and are capped at 32 with oldest-first eviction.
- Privileges are the invoking user's. An unprivileged user can plan
  anything but apply only what polkit permits; the refusal is
  systemctl's, passed through as a tool error.

## Error channels

Failures travel on two channels, split by whether the model can fix
them:

- **Tool errors** (`isError: true`, carrying the message) are anything
  a corrected call would resolve: invalid arguments (missing `unit`,
  out-of-range `priority`, unknown `state`), and backend failures (a
  systemctl exit status, an unreachable manager, bootup not finished).
  The session continues and the client passes the message to the model.
- **Protocol errors** (JSON-RPC, code -32602) are for what no argument
  fixes: an unknown tool, or one outside the granted scopes.

Argument validation lands on the tool channel deliberately. Clients are
not required to show protocol errors to the model, so reporting a
misspelled action as one hides the correction from the only party that
can make it. This is also what the specification asks for; see
[SEP-1303](https://github.com/modelcontextprotocol/modelcontextprotocol/issues/1303).

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
learns nothing from a refusal. A request that carries client
capabilities but no version is refused instead: capabilities are a
modern field, so the request has already declared its era.

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
