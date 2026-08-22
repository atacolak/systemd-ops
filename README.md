# systemd-ops

*A derivative of [systemd-mcpd](https://github.com/rhaist/systemd-mcpd).*

Inspect, control, and author systemd operations. Direct CLI by default;
optional MCP frontend. Writes exist only through plan/apply.

Written in Rust, with serde and serde_json as its only dependencies: no
libsystemd linkage, no D-Bus library, no async runtime.

**Rules, enforced in code:**

1. **Writes need an explicit prefix.** No `--write-prefix` (and no
   config/env prefix) means reads still work and writes are refused.
   There is no implied operator namespace.
2. **All writes go through plan/apply.** Nothing mutates directly. A
   change is planned first, then applied with a sealed `plan_token`.
   Apply is refused if the token is stale, expired, tampered, the
   wrong class (control vs author), or bound to the other manager.
3. **The MCP frontend grants nothing by default.** `systemd-ops-mcp`
   refuses to start without `--grant`. Tools outside the granted scopes
   are not advertised and are refused.

## Quick start

```
cargo build --release
./target/release/systemd-ops --json inspect failed-units
./target/release/systemd-ops --json --write-prefix 'managed-*' \
  control plan --action restart --unit managed-mail-check.service
```

`--json` prints one envelope on stdout:

```json
{"schema_version":1,"ok":true,"data":...}
```

or

```json
{"schema_version":1,"ok":false,"error":{"code":"...","message":"...","details":null}}
```

The CLI default manager is the user instance (`systemctl --user`).
Pass `--manager system` for PID 1. `systemd-ops-mcp` uses the same
default. Live PID-1 suites pass `--manager system` explicitly.

## MCP frontend

Optional. Direct CLI is the default agent surface. The MCP binary is
`systemd-ops-mcp`; it is the same engine behind a JSON-RPC stdio
transport.

It speaks MCP (line-delimited JSON-RPC 2.0) on stdin/stdout. Without
`--manager`, and without config/env, it defaults to the user manager.
Writes still need a prefix.

```json
{
  "mcpServers": {
    "systemd": {
      "command": "/usr/local/bin/systemd-ops-mcp",
      "args": ["--grant", "units:read,journal:read,boot:read"]
    }
  }
}
```

That grant is read-only.

## Scopes (MCP)

| Scope          | Grants                                                   |
|----------------|----------------------------------------------------------|
| `units:read`   | unit state, properties, timers, sockets, unit files, dependency edges, security analysis, operations |
| `journal:read` | journal entries, per unit                                |
| `boot:read`    | boot phase timings, critical chain, blame                |
| `units:write`  | lifecycle, enablement, log-control, and operation authoring, only through plan/apply |

## Changing something

`control plan --action stop --unit managed-x.service` reads state and
returns a `plan_token`. The unit is still running. `control apply
--plan-token TOKEN` is what stops it, and only if `active` is still
what the plan recorded.

Authoring writes `# managed: systemd-ops 1` into the unit file.
Unmarked units are project-managed: lifecycle is allowed under the
write-prefix, definition rewrite is not.

The `omp/` tree is an Oh My Pi adapter that shells `systemd-ops --json`.
It lives in git; it is not part of the crates.io package.

## Installing

```
make
sudo make prefix=/usr/local install
```

This installs `systemd-ops`, `systemd-ops-mcp`, the man page, the
optional socket pair, the docs and the license. Packagers:
[docs/PACKAGING.md](docs/PACKAGING.md).

Optional supervised MCP instance:

```
systemctl enable --now systemd-ops-mcp.socket
socat - UNIX-CONNECT:/run/systemd-ops-mcp.sock
```

The socket is mode 0600. The package should not enable it.

## Documentation

- [docs/TOOLS.md](docs/TOOLS.md): MCP tool arguments and reply shapes
- [docs/DESIGN.md](docs/DESIGN.md): backends, protocol revisions, write path
- [docs/TESTING.md](docs/TESTING.md): the three test tiers
- [docs/PACKAGING.md](docs/PACKAGING.md): distribution packaging
- [SECURITY.md](SECURITY.md): what is in scope
- [CHANGELOG.md](CHANGELOG.md): what changed per version
- [AGENTS.md](AGENTS.md): working notes for coding agents

## License

MIT, see [LICENSE](LICENSE). Ancestry: [NOTICE](NOTICE).
