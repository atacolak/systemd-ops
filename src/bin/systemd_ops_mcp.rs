//! Optional MCP frontend. Same engine as `systemd-ops`.
//!
//! Nothing is granted by default. Writes exist only through plan/apply.

use std::io::{stdin, stdout, BufReader};
use std::process::ExitCode;

use systemd_ops::config::OpsConfig;
use systemd_ops::mcp;
use systemd_ops::systemd::{self, Grants, Manager, Surface};

const USAGE: &str = "\
systemd-ops-mcp: MCP server exposing a capability-scoped view of systemd

Usage:
  systemd-ops-mcp --grant <scope>[,<scope>...] [options]
  systemd-ops-mcp --help | --version

Options:
  --manager user|system     systemd manager (default: user via config, else system)
  --surface full|compact    advertised tool set (default: full)
  --write-prefix <glob[,glob...]>  restrict writes to matching unit names
  --config <path>           config file (default: $XDG_CONFIG_HOME/systemd-ops/config.toml)

Scopes:
  units:read      units, timers, sockets, unit files, properties,
                  dependencies, security analysis, operations
  journal:read    read journal entries per unit
  boot:read       boot phase timings, critical chain, blame
  units:write     plan and apply changes: lifecycle
                  (start/stop/restart/reload/reset-failed), enablement
                  (enable/disable; full surface also has mask/unmask
                  and log-control), and operation create/update/retire

The server speaks MCP over stdio. Tools outside the granted scopes are
neither advertised nor callable. Writes exist only behind units:write
and only through plan/apply: a change is planned first (read-only,
returns current state, predicted state, and rollback), then applied by
plan_token; stale and expired plans are refused.";

fn take_flag_value(
    arg: &str,
    flag: &str,
    args: &mut impl Iterator<Item = String>,
) -> Option<String> {
    if let Some(rest) = arg.strip_prefix(&format!("{flag}=")) {
        return Some(rest.to_string());
    }
    if arg == flag {
        return args.next();
    }
    None
}

fn main() -> ExitCode {
    let mut grants: Option<Grants> = None;
    let mut manager = None;
    let mut surface = Surface::Full;
    let mut write_prefix: Option<String> = None;
    let mut config_path: Option<String> = None;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" | "-V" => {
                println!("systemd-ops-mcp {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
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
                    Ok(g) => grants.get_or_insert_with(Grants::default).extend(g),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            _ if arg == "--manager" || arg.starts_with("--manager=") => {
                let Some(spec) = take_flag_value(&arg, "--manager", &mut args) else {
                    eprintln!("error: --manager needs an argument\n\n{USAGE}");
                    return ExitCode::FAILURE;
                };
                match Manager::parse(&spec) {
                    Some(m) => manager = Some(m),
                    None => {
                        eprintln!("error: unknown manager '{spec}' (known: user, system)");
                        return ExitCode::FAILURE;
                    }
                }
            }
            _ if arg == "--surface" || arg.starts_with("--surface=") => {
                let Some(spec) = take_flag_value(&arg, "--surface", &mut args) else {
                    eprintln!("error: --surface needs an argument\n\n{USAGE}");
                    return ExitCode::FAILURE;
                };
                match Surface::parse(&spec) {
                    Some(s) => surface = s,
                    None => {
                        eprintln!("error: unknown surface '{spec}' (known: full, compact)");
                        return ExitCode::FAILURE;
                    }
                }
            }
            _ if arg == "--write-prefix" || arg.starts_with("--write-prefix=") => {
                let Some(spec) = take_flag_value(&arg, "--write-prefix", &mut args) else {
                    eprintln!("error: --write-prefix needs an argument\n\n{USAGE}");
                    return ExitCode::FAILURE;
                };
                match systemd::parse_write_prefix(&spec) {
                    Ok(_) => write_prefix = Some(spec),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            _ if arg == "--config" || arg.starts_with("--config=") => {
                let Some(spec) = take_flag_value(&arg, "--config", &mut args) else {
                    eprintln!("error: --config needs an argument\n\n{USAGE}");
                    return ExitCode::FAILURE;
                };
                config_path = Some(spec);
            }
            other => {
                eprintln!("error: unknown argument '{other}'\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    let Some(grants) = grants.filter(|g| !g.is_empty()) else {
        eprintln!("error: no scopes granted; pass --grant (see --help)");
        return ExitCode::FAILURE;
    };
    let cfg = match OpsConfig::load_with_default(
        manager,
        write_prefix,
        config_path.as_deref().map(std::path::Path::new),
        Manager::System,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    cfg.apply();
    systemd::set_surface(surface);

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
