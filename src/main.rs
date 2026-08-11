//! systemd-mcpd — a Model Context Protocol server for systemd.
//!
//! Read-only. Capability-scoped. Nothing is granted by default.
//!
//!     systemd-mcpd --grant units:read,journal:read
//!
//! The design position, in one sentence: authority handed to a language
//! model should be explicit at startup, enforced at every call, and
//! impossible to widen from inside the conversation.

mod mcp;
mod systemd;

use std::io::{stdin, stdout, BufReader};
use std::process::ExitCode;

use systemd::Grants;

const USAGE: &str = "\
systemd-mcpd: MCP server exposing a read-only, capability-scoped view of systemd

Usage:
  systemd-mcpd --grant <scope>[,<scope>...]
  systemd-mcpd --help | --version

Scopes:
  units:read      list units, failed units, unit properties
  journal:read    read journal entries per unit
  boot:read       boot phase timings and the boot-time critical chain

The server speaks MCP over stdio. Tools outside the granted scopes are
neither advertised nor callable. There are no write scopes; there is
nothing to misconfigure into mutability.";

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
            "--grant" => {
                let Some(spec) = args.next() else {
                    eprintln!("error: --grant needs an argument\n\n{USAGE}");
                    return ExitCode::FAILURE;
                };
                match Grants::from_args(&spec) {
                    Ok(g) => grants = Some(g),
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

    // No grants, no server. Requiring the operator to say what they are
    // handing over is the feature; an empty default would silently become
    // the permission model of every lazy deployment.
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
