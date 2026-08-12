# Testing

Three tiers, in cost order, plus the conformance suite.

| Tier                     | Needs                  | Runs                            |
|--------------------------|------------------------|---------------------------------|
| `cargo test`             | nothing                | protocol layer and parsers      |
| GitHub CI                | a push                 | fast gates, two live systemds   |
| `tests/release-check.sh` | KVM, by hand           | three systemd versions in QEMU  |

## Fast gates

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
MCPD=target/release/systemd-mcpd bash tests/manpage.sh
```

`make check` runs all of them.

The man page check is three things: `groff -mandoc -ww -z`, which is
what Debian runs as `manpage-has-errors-from-man`; `lexgrog`, which
decides whether `apropos` can index the page; and a comparison of the
scopes and long options in the page against the ones the built binary
reports in `--help`, in both directions. The last is the point. Bad
roff is loud, but a page that documents a flag the binary dropped is
silent, and the person who finds out is a user or a packager. It needs
groff and lexgrog, and skips itself under `make check` if they are
absent.

## Live suites

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
create transient units, write a unit file, and read the system journal:

```
cargo build --release
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo bash tests/integration.sh
MCPD=$PWD/target/release/systemd-mcpd HOST= sudo bash tests/varlink-proof.sh
```

`varlink-proof.sh` also needs a systemd new enough to serve the socket,
and renames it for the duration of the CLI half; `/run` is a tmpfs, so
an interrupted run costs the socket until reboot and nothing on disk.

## Release check

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

## Conformance

The official suite,
[modelcontextprotocol/conformance](https://github.com/modelcontextprotocol/conformance),
tests servers over HTTP only, and this one is stdio only.
`tests/http-shim.py` bridges the two for testing and ships with
nothing: it writes the request body to the server's stdin unchanged and
returns the reply line unchanged, so the suite judges the bytes
systemd-mcpd produces rather than a bridge's re-serialization. Note the
version: `latest` on npm carries no 2026-07-28 server scenarios yet.

```
python3 tests/http-shim.py --port 3000 -- \
  ./target/release/systemd-mcpd --grant units:read,journal:read,boot:read,units:write &
npx @modelcontextprotocol/conformance@0.2.0-alpha.11 server \
  --url http://127.0.0.1:3000/mcp --scenario tools-list --spec-version 2026-07-28
```

What applies, and passes: `tools-list` (3 of 3, in both eras),
`caching` (the `tools/list` hints, a non-negative TTL, a valid cache
scope), 9 checks of `server-stateless` including discovery, the server
identity in each result's `_meta`, capabilities matching the handlers,
and the unsupported-version error, plus `ping` and `server-initialize`
in the handshake era.

The rest fail for reasons that are not the server. HTTP status codes
(400, 404, 405) belong to the shim, since this server has no HTTP
transport to return them from. The resources, prompts, completion,
subscriptions and logging scenarios test primitives it does not
implement, and several tool scenarios call fixture tools by name
(`test_simple_text`, `json_schema_2020_12_tool`) that only the
specification's reference server has.

One failure is real and deliberate: `request-meta-invalid-missing-meta`
requires a modern server to reject a request carrying no `_meta` at
all. A dual-era server cannot, because that is exactly what a legacy
client sends. The suite has no dual-era mode; the specification's
compatibility matrix describes this case and allows it. Supporting
clients that exist today is worth one scenario.
