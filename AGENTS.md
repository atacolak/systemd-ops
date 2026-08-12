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

Actions are pinned by major tag and kept current by
`.github/dependabot.yml`, not pinned by commit SHA. A SHA pin is
stronger only while somebody bumps it; unattended it is a frozen old
version. Do not convert them without also deciding who does the
bumping.

Packaging surface, kept in step with the code: `Makefile` (install
targets), `systemd-mcpd.1` (man page), `systemd-mcpd.service` (sample
unit), `CHANGELOG.md`, and `docs/PACKAGING.md`. A new flag or scope
means updating the man page and the changelog too. `rust-version` in
`Cargo.toml` is a promise to packagers and CI checks it in both
directions, so raising it is a deliberate act, not a side effect.

## Invariants

- **Scopes gate twice.** A tool absent from `tools/list` must also be
  refused by `tools/call`. Both paths read the same `Scope`.
- **One mutating call:** `systemd::apply_verb` is the only invocation
  that changes system state, reachable only from `write::apply`. Do not
  add a second; route new mutations through plan/apply.
- **Backends are indistinguishable.** varlink and CLI results are
  normalized to the same shape, and filters are applied after either
  backend rather than inside one. A caller must not be able to tell
  which answered.
- **Argv, never a shell:** values reaching an argument list are
  validated first, and flags carrying values use the `--flag=value`
  form so option parsing cannot misread them.
- **One registry entry per tool**, in `src/mcp.rs`: name, scope,
  description, schema, handler. There is no second dispatch site.
- **One dispatch for both protocol eras:** `dispatch` matches the
  method once, for both eras, which differ only in the envelope around
  a result. Everything that decides the era, and whether a request is
  well formed, happens in `serve` before it. Two dispatches would let a
  method exist in one era and not the other.
- **Two error channels, split by who can fix it:** bad arguments and
  backend failures are tool errors (`isError`), because a corrected
  call resolves them and clients are not required to show protocol
  errors to the model. Protocol errors are for an unknown tool or an
  ungranted scope.

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
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo bash tests/integration.sh
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo bash tests/varlink-proof.sh
```

A sudoers rule that grants these two scripts without a password matches
the command by absolute path and carries the variables through
`env_keep`, so under one the invocation is
`sudo /usr/bin/bash /path/to/tests/integration.sh` with no `-E`: a
relative path does not match the rule, and `-E` is refused outright.

Root is required: the suites create transient units, write a unit file
under `/etc/systemd/system`, and read the system journal.
`varlink-proof.sh` also needs systemd >= 258 for the socket.

The tiers, what CI covers, the QEMU release check and the conformance
suite are documented in [docs/TESTING.md](docs/TESTING.md); what
follows is only what is easy to get wrong.

Two notes on the guests. Writing tests that run in one means no shell
syntax may cross ssh: ssh rebuilds the remote command through a shell,
and a redirect in the command string is performed by the local shell
instead. Pass content over stdin and keep operators, globs and quotes
out of remote commands. And the firmware and loader boot phases stay
uncovered: they come from EFI variables only systemd-boot sets, and a
cloud image cannot be converted to it, because its boot loader entries
live on a /boot that sd-boot ignores unless typed XBOOTLDR. Covering
them needs an image built for it (mkosi).

## Things that cost time to learn

- **Empty filter results:** journalctl exits 1 when a filter matches
  nothing, which is an answer rather than a failure.
  `run_journal_query` distinguishes the two: no matches produces exit 1
  with both streams empty, a real failure writes to stderr.
- **Masking `/etc` fragments:** `systemctl mask` fails for units whose
  fragment lives in `/etc/systemd/system`, because the mask symlink
  wants that exact path. Use a different verb to manufacture
  enablement drift in tests.
- **LogControl1 needs `BusName=`.** `systemctl service-log-level` talks
  D-Bus, so it works for logind and resolved but not for journald,
  which serves log control over varlink instead.
- **Unfinished boots:** hosts that never finish booting (CI runner VMs)
  make `boot_times` and `critical_chain` return a "not yet finished"
  error, while `boot_blame` may still answer. Tests assert whichever
  behavior the host exhibits.
- **Method naming:** `io.systemd.Unit.List` lists units, not
  `Manager.ListUnits`. Verify varlink interfaces against the systemd
  source (`src/shared/varlink-io.systemd.*.c`) before coding to them.
- **Proving the varlink path:** never do it by moving
  `/usr/bin/systemctl`. Run the server with an empty `PATH` instead; an
  interrupted run must not leave a host without systemctl.
- **Protocol revisions:** read the specification page before coding to
  a revision; the wire changed more than the version string in
  2026-07-28. The pages that matter for a tools-only stdio server are
  `basic/index#meta`, `basic/versioning`, `server/discover`,
  `server/tools`, and `server/utilities/caching`.
- **The conformance suite is HTTP-only.** Server scenarios take a
  `--url`, so reaching a stdio server needs `tests/http-shim.py`. On
  npm, `latest` (0.1.16) has no 2026-07-28 server scenarios; the
  0.2.0 prereleases do. Most failures against this server are the
  shim's HTTP status codes or primitives it does not implement, so
  read the check names rather than the totals.

## Prose

Documentation, comments, and commit messages state facts. The general
standard is the `avoid-ai-writing` skill; install it rather than
copying its rules here. The repository also holds to:

- no em dashes, including in tool descriptions sent to clients
- limits documented with the same weight as features: what is
  unguarded, what is version-dependent, what was not implemented
- numbers and names in place of adjectives
- code described by behavior, not character: an error is returned,
  not "said"

Commit messages state what changed, why, and what was observed. Do not
add `Co-Authored-By` or other attribution trailers.
