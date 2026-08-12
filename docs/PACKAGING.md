# Packaging

What a distribution packager needs, in one place. If something here is
wrong or missing for your distribution, that is a bug worth filing.

## Facts

| | |
|---|---|
| Language | Rust 2021 |
| Minimum toolchain | 1.82, declared as `rust-version` and verified in CI both directions |
| Dependencies | serde, serde_json. No libsystemd, no D-Bus library, no async runtime |
| Build-time requirements | cargo, rustc. Nothing else |
| Runtime requirements | systemd. `systemctl`, `journalctl` and `systemd-analyze` on `PATH` |
| Architecture | any that rustc targets. No architecture-specific code |
| Network at build time | none, given a vendored or pre-fetched registry |
| Configuration files | none |
| Daemons started | none. The binary is a stdio child of an MCP client |
| License | MIT, one file, `LICENSE` |

## Building and installing

```
make
make DESTDIR=$RPM_BUILD_ROOT prefix=/usr install
```

The `Makefile` is a thin wrapper over cargo that exists so packaging
does not have to hand-roll install lines. It honors `DESTDIR`, `prefix`,
`bindir`, `mandir`, `man1dir`, `docdir`, `licensedir` and `unitdir`, and
takes `CARGO` and `CARGOFLAGS` for offline or vendored builds:

```
make CARGOFLAGS="--release --locked --offline"
```

`make install` places:

| Path | Contents |
|---|---|
| `$(bindir)/systemd-mcpd` | the binary |
| `$(man1dir)/systemd-mcpd.1` | the man page, with the version substituted into its `.TH` line |
| `$(unitdir)/systemd-mcpd.service` | sample hardened unit |
| `$(docdir)/` | README and the docs directory |
| `$(licensedir)/LICENSE` | the license |

`unitdir` defaults to whatever `pkg-config --variable=systemdsystemunitdir systemd`
reports, and to `$(prefix)/lib/systemd/system` when pkg-config or the
systemd development files are absent.

The unit file in the repository has `ExecStart=/usr/local/bin/systemd-mcpd`,
which is right for a manual install. `make install` rewrites it to the
`bindir` being installed into, so there is nothing to patch.

## Vendoring

`Cargo.lock` is committed and `--locked` is used throughout, so a build
resolves to exactly the versions upstream tested. For an offline build:

```
cargo vendor vendor/
```

Note that `Cargo.lock` is format version 4, which needs cargo 1.78 or
later to parse. That is below the declared MSRV, so any toolchain that
can build this can read the lockfile.

## The unit file is optional

The usual deployment is an MCP client spawning the binary as a child
process over stdio, which needs no unit file, no service, and no
enablement. `systemd-mcpd.service` is a sample for the case where an
operator wants the server supervised, and it should not be enabled by
the package. It grants read scopes only.

## Testing during a package build

`make check` runs what upstream CI gates on and needs no systemd:

```
make check
```

That includes the man page check: `groff -mandoc -ww -z` (Debian's
`manpage-has-errors-from-man`), `lexgrog` for the whatis entry, and a
comparison of the documented scopes and flags against the built
binary. It skips itself if groff or lexgrog is missing, so it will not
break a minimal build chroot.

The live suites under `tests/` need root, a running systemd, and the
ability to create transient units and write to `/etc/systemd/system`.
They are unsuitable for a build chroot and are excluded from the
published crate. See [TESTING.md](TESTING.md) if you want to run them
against an installed package.

## Things that are deliberately absent

- **No shipped `debian/` or `.spec` directory.** Distribution packaging
  belongs to the distribution, and an upstream copy goes stale without
  anyone noticing.
- **No bundled dependencies.** Two crates, both packaged everywhere.
- **No setuid, no capabilities, no `/etc` file, no state directory.**
  The binary runs with the privileges of whoever spawns it and stores
  nothing.
- **No shell completions.** There are three flags.

## Reporting

Patches that make packaging easier are welcome, particularly ones that
correct assumptions about paths. The one thing to avoid is patching the
tool descriptions or schemas: those are the contract a language model
sees, and they are covered by tests that assert exact wire shapes.
