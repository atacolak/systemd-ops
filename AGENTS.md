# AGENTS.md

Working notes for coding agents. Conventions and gotchas that are not
evident from the source.

## What this is

An MCP server exposing systemd to language-model clients. Line-delimited
JSON-RPC 2.0 on stdin/stdout, no async runtime, no libsystemd linkage.
Dependencies are serde and serde_json; adding a third needs a reason
that survives review.

## Layout

| File             | Holds                                                    |
|------------------|----------------------------------------------------------|
| `src/main.rs`    | argument parsing, `--grant`, usage text                  |
| `src/mcp.rs`     | protocol, tool registry, scope gating                    |
| `src/systemd.rs` | backend: process invocation, parsers, scope definitions  |
| `src/varlink.rs` | varlink client for PID 1's socket                        |
| `src/write.rs`   | plan/apply state machine                                 |

## Invariants

- **Scopes gate twice.** A tool absent from `tools/list` must also be
  refused by `tools/call`. Both paths read the same `Scope`.
- **One mutating call.** `systemd::apply_verb` is the only invocation
  that changes system state, reachable only from `write::apply`. Do not
  add a second; route new mutations through plan/apply.
- **Backends are indistinguishable.** varlink and CLI results are
  normalized to the same shape, and filters are applied after either
  backend rather than inside one. A caller must not be able to tell
  which answered.
- **Argv, never a shell.** Values reaching an argument list are
  validated first, and flags carrying values use the `--flag=value`
  form so option parsing cannot misread them.
- **One registry entry per tool**, in `src/mcp.rs`: name, scope,
  description, schema, handler. There is no second dispatch site.

## Building and testing

```
cargo test                        # protocol and parsers, no systemd needed
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Live suites drive the built binary against a running systemd. `MCPD` is
how to run the server, `HOST` how to reach the target systemd (empty
for this machine):

```
cargo build --release
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo -E bash tests/integration.sh
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo -E bash tests/varlink-proof.sh
```

Root is required: the suites create transient units, write a unit file
under `/etc/systemd/system`, and read the system journal.
`varlink-proof.sh` also needs systemd >= 258 for the socket.

## Things that cost time to learn

- **Empty filter results.** journalctl exits 1 when a filter matches
  nothing, which is an answer rather than a failure.
  `run_journal_query` distinguishes the two: no matches produces exit 1
  with both streams empty, a real failure writes to stderr.
- **Masking `/etc` fragments.** `systemctl mask` fails for units whose
  fragment lives in `/etc/systemd/system`, because the mask symlink
  wants that exact path. Use a different verb to manufacture
  enablement drift in tests.
- **LogControl1 needs `BusName=`.** `systemctl service-log-level` talks
  D-Bus, so it works for logind and resolved but not for journald,
  which serves log control over varlink instead.
- **Unfinished boots.** Hosts that never finish booting (CI runner VMs)
  make `boot_times` and `critical_chain` return a "not yet finished"
  error, while `boot_blame` may still answer. Tests assert whichever
  behavior the host exhibits.
- **Method naming.** `io.systemd.Unit.List` lists units, not
  `Manager.ListUnits`. Verify varlink interfaces against the systemd
  source (`src/shared/varlink-io.systemd.*.c`) before coding to them.
- **Proving the varlink path.** Never do it by moving
  `/usr/bin/systemctl`. Run the server with an empty `PATH` instead; an
  interrupted run must not leave a host without systemctl.

## Prose

Documentation, comments, and commit messages state facts. The general
standard is the `avoid-ai-writing` skill; install it rather than
copying its rules here. The repository additionally holds to:

- no em dashes, including in tool descriptions sent to clients
- limits documented with the same weight as features: what is
  unguarded, what is version-dependent, what was not implemented
- numbers and names in place of adjectives
- code described by behavior, not character: an error is returned,
  not "said"

Commit messages state what changed, why, and what was observed. Do not
add `Co-Authored-By` or other attribution trailers.
