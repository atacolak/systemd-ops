# Security

## Reporting

Report vulnerabilities through GitHub's private vulnerability reporting
on this repository (Security, then Report a vulnerability). That keeps
the report private until there is something to release.

Please do not open a public issue for anything that lets a caller reach
authority it was not granted.

Expect an acknowledgement within a week. This is a small project with
no security team behind it, and that is worth knowing before you rely
on it.

## What is in scope

The claims this project makes, and therefore the ones worth breaking:

- **Scopes bound authority.** A tool outside the granted scopes is
  neither advertised nor callable, in either protocol era.
- **Nothing mutates without an applied plan.** There is one mutating
  process invocation and it is reachable only through `apply_plan`.
  A token is bound to the manager it was issued against.
- **No shell, ever.** Arguments are argv elements. A unit name reaches
  a command line only after `--` or inside a single `--flag=value`,
  and only after validation.
- **The server acquires no privilege.** It runs as whoever spawned it
  and is not setuid. Optional config (`--config`,
  `$XDG_CONFIG_HOME/systemd-ops/config.toml`, `SYSTEMD_OPS_*`) can
  set manager, write-prefix, and plan TTL. It cannot widen MCP grants.

A way around any of those is a vulnerability. So is anything that makes
the socket unit reachable by a user the operator did not intend.

## What is not

- **A model with `units:write` can operate systemd.** That is the
  feature. Granting it to a model reading untrusted input is a
  deployment decision, and the plan/apply step exists so a human can
  see the change before it happens.
- **Content returned by the tools is untrusted.** Journal messages come
  from whatever wrote them, and any local process can write to the
  journal. The server labels this in its instructions and returns the
  text as inert JSON; it cannot sanitize it without destroying the
  content it exists to report. See docs/DESIGN.md.
- **Denial of service against yourself over stdio.** The client owns
  the process it spawned and can kill it.
- **Anything requiring root.** An attacker who is already root does not
  need this server.

## Known limits

Documented rather than fixed, with reasons, in
[docs/DESIGN.md](docs/DESIGN.md#known-limits): child process runtime is
unguarded, request lines are unbounded, and the plan precondition is a
check rather than a lock.
