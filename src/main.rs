//! systemd-mcpd: a Model Context Protocol server for systemd.
//!
//! Capability-scoped. Nothing is granted by default; reads are direct,
//! writes exist only behind units:write and only through plan/apply.
//!
//!     systemd-mcpd --grant units:read,journal:read,boot:read
//!
//! The design position, in one sentence: authority handed to a language
//! model should be explicit at startup, enforced at every call, and
//! impossible to widen from inside the conversation.

// There is none, and a program that operates systemd on a model's
// behalf should not be able to acquire any without the compiler
// objecting.
#![forbid(unsafe_code)]

mod mcp;
mod systemd;
mod varlink;
mod write;

use std::io::{stdin, stdout, BufReader};
use std::process::ExitCode;

use systemd::Grants;

const USAGE: &str = "\
systemd-mcpd: MCP server exposing a capability-scoped view of systemd

Usage:
  systemd-mcpd --grant <scope>[,<scope>...]
  systemd-mcpd --help | --version

Scopes:
  units:read      units, timers, sockets, unit files, properties,
                  dependencies, security analysis
  journal:read    read journal entries per unit
  boot:read       boot phase timings, critical chain, blame
  units:write     plan and apply changes: lifecycle
                  (start/stop/restart/reload), enablement
                  (enable/disable/mask/unmask), log level and log target

The server speaks MCP over stdio. Tools outside the granted scopes are
neither advertised nor callable. Writes exist only behind units:write
and only through plan/apply: a change is planned first (read-only,
returns current state, predicted state, and rollback), then applied by
plan id; stale plans are refused.";

fn main() -> ExitCode {
    let mut grants: Option<Grants> = None;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" | "-V" => {
                println!("systemd-mcpd {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            // Both spellings: a user who types `--grant=units:read` is
            // not wrong, and rejecting it as an unknown argument reads
            // like the scope was the problem.
            _ if arg == "--grant" || arg.starts_with("--grant=") => {
                let spec = match arg.strip_prefix("--grant=") {
                    Some(spec) => spec.to_string(),
                    None => match args.next() {
                        Some(spec) => spec,
                        None => {
                            eprintln!("error: --grant needs an argument\n\n{USAGE}");
                            return ExitCode::FAILURE;
                        }
                    },
                };
                match Grants::from_args(&spec) {
                    // Repeated flags add up rather than replacing. The
                    // command line is where the authority is stated and
                    // where an operator reads it back, so what the
                    // process table shows and what the server enforces
                    // have to be the same set.
                    Ok(g) => grants.get_or_insert_with(Grants::default).extend(g),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            other => {
                eprintln!("error: unknown argument '{other}'\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Without grants the server refuses to start. The operator must state
    // what is being handed over; an empty default would become the de
    // facto permission model of unconfigured deployments.
    let Some(grants) = grants.filter(|g| !g.is_empty()) else {
        eprintln!("error: no scopes granted; pass --grant (see --help)");
        return ExitCode::FAILURE;
    };

    let stdin = stdin();
    let stdout = stdout();
    match mcp::serve(BufReader::new(stdin.lock()), stdout.lock(), &grants) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
