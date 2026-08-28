# systemd-ops

Inspect, control, and author systemd operations. Direct CLI by default;
optional MCP frontend. Writes exist only through plan/apply.

Written in Rust. Core dependencies are serde, serde_json, and toml. The
optional TUI adds ratatui. No libsystemd linkage, no D-Bus library, no
async runtime.

**Rules, enforced in code:**

1. **Writes need an explicit prefix.** No `--write-prefix` (and no
   config/env prefix) means reads still work and writes are refused.
   There is no implied operator namespace.
2. **All systemd mutations go through plan/apply.** Lifecycle,
   enablement, and definition authoring are planned first, then applied
   with a sealed `plan_token`. Apply is refused if the token is stale,
   expired, tampered, the wrong class (control vs author), or bound to
   the other manager. Operator commentary and iteration history under
   `.systemd-ops/<stem>/state/operator.json` are advisory soft state:
   they are written directly by `systemd-ops operator ...` and never
   touch unit files or affect objective health.
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

## Responsibility scopes

A project should place its manifest at `.systemd-ops/scope.toml` under
the scope root. The legacy `.systemd-ops.toml` manifest remains readable.
If both exist at the same root, scope resolution fails instead of choosing
one.

```toml
[scope]
id = "speech"
owned = ["managed-speech-*"]

critical = ["managed-speech-asr", "managed-speech-tts"]

[[watch]]
operation = "managed-proxy-health"

[automation]
agent_root = "/srv/automation-agents"
```

```
systemd-ops scope validate
systemd-ops scope show
systemd-ops --json scope show
systemd-ops --scope-root ~/worlds/speech tui
```

For scope, operator, and TUI commands, scope resolution uses
`--scope-root` first, then `SYSTEMD_OPS_SCOPE_ROOT`, then upward discovery
from `--cwd` or the process working directory. Author and control
provenance continue to use `--cwd`. The scope root identifies the
responsibility scope. Each owned operation has an operation home at
`.systemd-ops/<stem>/` under that root. The execution cwd is the directory
recorded in and used by the systemd unit. These are separate concepts.
Operation homes hold advisory project state; they do not replace the
`.service` and `.timer` files or systemd's runtime state as operational
truth.

Agent-backed operations may add `.systemd-ops/<stem>/automation.toml` with
`version`, `agent`, optional same-scope `parent`, and explicit `brain_paths`.
The brain revision hashes canonical metadata, the exact resolved agent file,
and only those listed paths. Systemd remains canonical for execution, cwd,
schedule, restart, and enablement. `automation plan-create`, `plan-update`,
`plan-retire`, and `plan-complete` compose the existing sealed author path;
`automation complete` is the trusted immediate completion seam. Completed
operations keep their definition, operation home, history, fingerprint, TUI
row, and relations while future timer activation is stopped and disabled.

The OMP adapter exposes `automation_agent_author` and `automation_author` as
hidden explicit-only builder tools. Ordinary runtime agents should receive only
`automation_context`, `automation_report`, and `automation_activity`.

`systemd-ops --json scope show` and `tui` consume the same derived
ScopeView. Agents are the first-class author/control/operator interface;
the TUI is a read-only operator cockpit. The AUTOMATIONS panel omits a
redundant OWNED heading for owned-only scopes and shows relationship headings
when watching entries exist. Cockpit detail renders the description, NOW,
AGENT BRIEF, RECENT ITERATIONS, NOTABLE ACTIVITY, then objective RUNTIME. `d`
toggles wiring detail. `l` independently attaches a lazy raw-journal drawer
below the selected detail. Opening the TUI or moving selection while that drawer
is closed does not fetch journald. `j`/`k` or the arrow keys select an
operation. PageUp/PageDown scroll detail by one visible page; Home/End jump to
the top/bottom. The mouse wheel targets the list, detail, or logs under the
pointer. `/` filters, `r` refreshes, and `q` or Esc exits.
Details: [docs/SCOPES.md](docs/SCOPES.md).

### Operator state and iterations

```
systemd-ops --json --scope-root ~/worlds/personal operator set \
  --unit managed-personal-youtube-poll \
  --about "Polls YouTube for new uploads and queues summaries." \
  --headline "waiting for next poll" \
  --body "Last poll succeeded; next timer is armed."
systemd-ops --json --scope-root ~/worlds/personal operator append \
  --unit managed-personal-youtube-poll \
  --text "manual reconsolidation after schedule tweak"
systemd-ops --json --scope-root ~/worlds/personal operator show \
  --unit managed-personal-youtube-poll
systemd-ops --json --scope-root ~/worlds/personal operator iteration-start \
  --unit managed-personal-youtube-poll
systemd-ops --json --scope-root ~/worlds/personal operator iteration-finish \
  --unit managed-personal-youtube-poll --iteration ITERATION_ID --exit-code 0
```

Only stems matching the resolved scope's `owned` globs may be written.
The canonical state file is
`.systemd-ops/<stem>/state/operator.json`. Existing
`.systemd-ops/operator/<stem>.json` files remain readable when canonical
state is absent. The next successful operator write uses the canonical
path and may remove the legacy file. If both files exist, the canonical
file wins with a warning and their contents are not merged. Deleting
operator state has no effect on systemd operations.

Activity is a stream of advisory notes. `operator iteration-start --unit
STEM` opens an explicit advisory work session. `operator iteration-finish
--unit STEM --iteration ID --exit-code N` closes that exact session. The
surface shows the active iteration and the latest 20 finished iterations,
newest first. Runtime is objective state derived from systemd. Timer
activations, service checks, and other systemd executions are runtime
facts, not operator iterations. None of the advisory fields affect
operation or scope health.

OMP adapter calls prefer the session `ctx.cwd` over the adapter factory
cwd, pass that directory as both the CLI global `--cwd` and child process
cwd, and inherit `SYSTEMD_OPS_SCOPE_ROOT` from the parent environment. The
adapter adds no separate scope-root parameter.

### Bound autonomous operation tools

The OMP adapter exposes four capability classes. `systemd_inspect` is the
broad read surface for project builders, operators, and admins.
`systemd_control` changes lifecycle state and `systemd_author` changes unit
definitions; ordinary runtime maintainers should receive neither.
`systemd_operator` is the low-level manual advisory-state surface. Bound
runtime maintainers instead receive only `automation_context`,
`automation_report`, and `automation_activity`.

The narrow commands take no operation argument. They require inherited
`SYSTEMD_OPS_SCOPE_ROOT` and `SYSTEMD_OPS_OPERATION`, resolve that exact owned
stem, and reject missing, watched, or cross-operation use. `automation context`
returns canonical purpose and identity, objective runtime, the current human
report, active iteration, latest 20 finished iterations, and notable activity.
It is working context, not raw journald.

`automation report --headline TEXT --summary JSON_ARRAY` is the mandatory
human-facing reconsolidation for a normal autonomous pass. Headline is one
non-empty line of at most 80 characters. Summary is 1 to 5 non-empty,
single-paragraph strings of at most 280 characters each; the stored compatible
body joins them with blank lines. `automation activity --text TEXT` is optional
and accepts one non-empty line of at most 200 characters for a notable semantic
milestone. Activity alone does not reconsolidate an iteration. The wrapper must
finish the iteration successfully, observe `reconsolidated: true`, and only then
advance its external fingerprint. OMP failure, report omission, or finish
failure leaves the fingerprint unchanged so the operation retries.

Repository proofs for these contracts are `tests/automation-cli.sh` and
`tests/wrapper-contract.sh`. Both use temporary scope roots and mutate no
systemd state.

OMP currently loads custom tools at the session surface. It does not provide a
persistent per-agent capability registry in systemd-ops. Dogfood maintainers
therefore narrow visibility explicitly with OMP `--tools`; delegated workers do
not inherit the autonomous tools unless their launcher grants them.

`get_operation` returns `editable_spec` for systemd-ops-managed stems so
an agent can change one field and plan-update without dropping the rest.
`start_now` is apply intent, not durable configuration, and is omitted
from that reconstruction.

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

- [docs/SCOPES.md](docs/SCOPES.md): responsibility scopes, TUI, health
- [docs/TOOLS.md](docs/TOOLS.md): MCP tool arguments and reply shapes
- [docs/DESIGN.md](docs/DESIGN.md): backends, protocol revisions, write path
- [docs/TESTING.md](docs/TESTING.md): the three test tiers
- [docs/PACKAGING.md](docs/PACKAGING.md): distribution packaging
- [SECURITY.md](SECURITY.md): what is in scope
- [CHANGELOG.md](CHANGELOG.md): what changed per version
- [AGENTS.md](AGENTS.md): working notes for coding agents

## License

MIT, see [LICENSE](LICENSE).

Portions of this codebase originate from
[systemd-mcpd](https://github.com/rhaist/systemd-mcpd). See [NOTICE](NOTICE).
