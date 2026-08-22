//! Direct CLI for systemd-ops. Agents use `--json`. Humans get text.

use std::process::ExitCode;

use serde_json::{json, Value};
use systemd_ops::config::OpsConfig;
use systemd_ops::json;
use systemd_ops::operations;
use systemd_ops::scope;
use systemd_ops::systemd::{self, BackendError, LogFilter, Manager, Surface};
use systemd_ops::token::{self, PlanClass};
use systemd_ops::tui;
use systemd_ops::write::{self, Action};

const USAGE: &str = "\
systemd-ops: inspect, control, and author systemd operations

Usage:
  systemd-ops [global options] <command> [args]
  systemd-ops --help | --version

Global options:
  --json                       machine-readable envelope on stdout
  --manager user|system        systemd manager (config default: user)
  --write-prefix <glob[,..]>   restrict writes; no prefix means writes refused
  --cwd <path>                 provenance for authoring plans; also scope discovery
  --config <path>              config file (default: $XDG_CONFIG_HOME/systemd-ops/config.toml)

Commands:
  inspect list-units [--state STATE] [--pattern GLOB]
  inspect failed-units
  inspect get-unit --unit NAME
  inspect list-operations [--pattern GLOB]
  inspect get-operation --unit STEM
  inspect list-timers [--pattern GLOB]
  inspect list-unit-files [--pattern GLOB] [--state STATE]
  inspect unit-dependencies --unit NAME
  inspect unit-logs --unit NAME [--lines N] [--since T] [--until T]
                    [--priority N] [--grep RE] [--boot N]

  control plan --action start|stop|restart|reload|enable|disable|reset-failed --unit NAME
  control apply --plan-token TOKEN

  author plan-create --spec JSON | (flat OperationSpec fields)
  author plan-update --spec JSON | (flat OperationSpec fields)
  author plan-retire --unit STEM
  author apply --plan-token TOKEN

  scope show
  scope validate
  tui
";
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
fn fail(json_mode: bool, code: &str, message: &str) -> ExitCode {
    let env = json::err(code, message, Value::Null);
    if json_mode {
        println!("{}", env);
    } else {
        eprintln!("error: {message}");
        println!("{}", env);
    }
    ExitCode::FAILURE
}

fn succeed(json_mode: bool, data: Value) -> ExitCode {
    if json_mode {
        println!("{}", json::ok(data));
    } else {
        print_human(&data);
    }
    ExitCode::SUCCESS
}

fn from_result(json_mode: bool, result: Result<Value, BackendError>) -> ExitCode {
    match result {
        Ok(data) => succeed(json_mode, data),
        Err(e) => {
            let env = json::from_backend(&e);
            if json_mode {
                println!("{}", env);
            } else {
                eprintln!("error: {e}");
                println!("{}", env);
            }
            ExitCode::FAILURE
        }
    }
}

fn print_human(data: &Value) {
    if data.get("owned").is_some() && data.get("watching").is_some() && data.get("id").is_some() {
        print_scope_human(data);
        return;
    }
    match data {
        Value::Array(rows) => {
            println!("{} rows", rows.len());
            for row in rows {
                if let Some(unit) = row.get("unit").and_then(Value::as_str) {
                    let extra = row
                        .get("title")
                        .and_then(Value::as_str)
                        .or_else(|| row.get("health").and_then(Value::as_str))
                        .or_else(|| row.get("description").and_then(Value::as_str))
                        .or_else(|| row.get("active").and_then(Value::as_str))
                        .unwrap_or("");
                    if extra.is_empty() {
                        println!("{unit}");
                    } else {
                        println!("{unit}  {extra}");
                    }
                } else {
                    println!("{row}");
                }
            }
        }
        Value::Object(map) => {
            if let Some(token) = map.get("plan_token").and_then(Value::as_str) {
                println!("plan_token {token}");
            }
            if let Some(unit) = map.get("unit").and_then(Value::as_str) {
                println!("unit {unit}");
            }
            if let Some(action) = map.get("action").and_then(Value::as_str) {
                println!("action {action}");
            }
            if let Some(next) = map.get("next") {
                println!("next {next}");
            }
            if let Some(last) = map.get("last") {
                println!("last {last}");
            }
            if let Some(result) = map.get("last_result") {
                println!("last_result {result}");
            }
            if let Some(exec) = map.get("exec") {
                println!("exec {exec}");
            }
            if let Some(schedule) = map.get("schedule") {
                if !schedule.is_null() {
                    println!("schedule {schedule}");
                }
            }
            if map.get("applied").and_then(Value::as_bool) == Some(true) {
                println!("applied true");
            }
            if map.len() > 8 {
                println!("{data}");
            }
        }
        other => println!("{other}"),
    }
}

fn print_scope_human(data: &Value) {
    let id = data.get("id").and_then(Value::as_str).unwrap_or("?");
    let health = data.get("health").and_then(Value::as_str).unwrap_or("?");
    let root = data.get("root").and_then(Value::as_str).unwrap_or("");
    let owned = data.get("owned").and_then(Value::as_array);
    let watching = data.get("watching").and_then(Value::as_array);
    let attention = data.get("attention").and_then(Value::as_array);
    let n_owned = owned.map(Vec::len).unwrap_or(0);
    let n_watch = watching.map(Vec::len).unwrap_or(0);
    let n_att = attention.map(Vec::len).unwrap_or(0);
    println!(
        "{}                                      {}",
        id.to_uppercase(),
        health.to_uppercase()
    );
    if !root.is_empty() {
        println!("{root}");
    }
    println!("{n_owned} owned · {n_watch} watching · {n_att} attention");
    println!();
    println!("OWNED");
    if let Some(rows) = owned {
        if rows.is_empty() {
            println!("  (none)");
        }
        for row in rows {
            print_scope_row(row);
        }
    }
    println!();
    println!("WATCHING");
    if let Some(rows) = watching {
        if rows.is_empty() {
            println!("  (none)");
        }
        for row in rows {
            print_scope_row(row);
        }
    }
    if n_att > 0 {
        println!();
        println!("ATTENTION");
        if let Some(rows) = attention {
            for row in rows {
                let op = row.get("operation").and_then(Value::as_str).unwrap_or("?");
                let rel = row
                    .get("relationship")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let reason = row.get("reason").and_then(Value::as_str).unwrap_or("?");
                println!("  {op}  {rel}  {reason}");
            }
        }
    }
}

fn print_scope_row(row: &Value) {
    let mark = match row.get("health").and_then(Value::as_str) {
        Some("healthy") => "●",
        Some("failed") => "✖",
        _ => "?",
    };
    let title = row
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| row.get("unit").and_then(Value::as_str))
        .unwrap_or("?");
    let health = row.get("health").and_then(Value::as_str).unwrap_or("?");
    let crit = if row.get("critical").and_then(Value::as_bool) == Some(true) {
        "  critical"
    } else {
        ""
    };
    let next = row.get("next");
    let next_s = match next {
        Some(Value::Null) | None => String::new(),
        Some(v) => format!("  next {v}"),
    };
    println!("  {mark} {title}  {health}{crit}{next_s}");
}

fn run_scope(cmd: &str, args: &mut [String], cwd: Option<&str>) -> Result<Value, BackendError> {
    remaining_flags(args).map_err(BackendError)?;
    match cmd {
        "show" => Ok(scope::show(cwd)?.to_json()),
        "validate" => scope::validate(cwd),
        other => Err(BackendError(format!("unknown scope command '{other}'"))),
    }
}

fn json_arg(args: &mut Vec<String>, flag: &str) -> Result<Option<Value>, String> {
    let Some(pos) = args
        .iter()
        .position(|a| a == flag || a.starts_with(&format!("{flag}=")))
    else {
        return Ok(None);
    };
    let eq = format!("{flag}=");
    let raw = if let Some(rest) = args[pos].strip_prefix(&eq) {
        let v = rest.to_string();
        args.remove(pos);
        v
    } else {
        args.remove(pos);
        if pos >= args.len() {
            return Err(format!("{flag} needs an argument"));
        }
        args.remove(pos)
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("{flag}: {e}"))
}

fn take_opt(args: &mut Vec<String>, flag: &str) -> Result<Option<String>, String> {
    let Some(pos) = args
        .iter()
        .position(|a| a == flag || a.starts_with(&format!("{flag}=")))
    else {
        return Ok(None);
    };
    if let Some(rest) = args[pos].strip_prefix(&format!("{flag}=")) {
        let v = rest.to_string();
        args.remove(pos);
        return Ok(Some(v));
    }
    args.remove(pos);
    if pos >= args.len() {
        return Err(format!("{flag} needs an argument"));
    }
    Ok(Some(args.remove(pos)))
}

fn require_opt(args: &mut Vec<String>, flag: &str) -> Result<String, String> {
    take_opt(args, flag)?.ok_or_else(|| format!("missing {flag}"))
}

fn remaining_flags(args: &[String]) -> Result<(), String> {
    if let Some(a) = args.iter().find(|a| a.starts_with('-')) {
        return Err(format!("unknown argument '{a}'"));
    }
    if !args.is_empty() {
        return Err(format!("unexpected argument '{}'", args[0]));
    }
    Ok(())
}

fn spec_from_flat(args: &mut Vec<String>) -> Result<Value, String> {
    if let Some(spec) = json_arg(args, "--spec")? {
        return Ok(spec);
    }
    let mut spec = serde_json::Map::new();
    let keys = [
        "--unit",
        "--kind",
        "--title",
        "--purpose",
        "--description",
        "--cwd",
        "--restart",
    ];
    for k in keys {
        if let Some(v) = take_opt(args, k)? {
            spec.insert(k.trim_start_matches('-').replace('-', "_"), json!(v));
        }
    }
    if let Some(v) = json_arg(args, "--exec")? {
        spec.insert("exec".into(), v);
    }
    if let Some(v) = json_arg(args, "--tags")? {
        spec.insert("tags".into(), v);
    }
    if let Some(v) = json_arg(args, "--env")? {
        spec.insert("env".into(), v);
    }
    if let Some(v) = json_arg(args, "--path")? {
        spec.insert("path".into(), v);
    }
    if let Some(v) = json_arg(args, "--environment-files")? {
        spec.insert("environment_files".into(), v);
    }
    if let Some(v) = json_arg(args, "--after")? {
        spec.insert("after".into(), v);
    }
    if let Some(v) = json_arg(args, "--schedule")? {
        spec.insert("schedule".into(), v);
    }
    if let Some(v) = take_opt(args, "--nice")? {
        spec.insert(
            "nice".into(),
            json!(v.parse::<i64>().map_err(|_| "nice must be an integer")?),
        );
    }
    if let Some(v) = take_opt(args, "--enabled")? {
        spec.insert("enabled".into(), json!(v == "true" || v == "1"));
    }
    if let Some(v) = take_opt(args, "--start-now")? {
        spec.insert("start_now".into(), json!(v == "true" || v == "1"));
    }
    if let Some(v) = take_opt(args, "--wants-network-online")? {
        spec.insert(
            "wants_network_online".into(),
            json!(v == "true" || v == "1"),
        );
    }
    if spec.get("unit").is_none() {
        return Err("missing --unit or --spec".into());
    }
    Ok(Value::Object(spec))
}

fn with_cwd(mut args: Value, cwd: Option<&str>) -> Value {
    if let Some(cwd) = cwd {
        if args.get("context").is_none() {
            args["context"] = json!({ "cwd": cwd });
        }
    }
    args
}

fn process_cwd(explicit: Option<String>) -> Result<String, BackendError> {
    if let Some(c) = explicit {
        return Ok(c);
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| BackendError(format!("cannot resolve cwd: {e}")))
}

fn run_inspect(cmd: &str, args: &mut Vec<String>) -> Result<Value, BackendError> {
    match cmd {
        "list-units" => {
            let state = take_opt(args, "--state").map_err(BackendError)?;
            let pattern = take_opt(args, "--pattern").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            systemd::list_units(state.as_deref(), pattern.as_deref())
        }
        "failed-units" => {
            remaining_flags(args).map_err(BackendError)?;
            systemd::failed_units()
        }
        "get-unit" => {
            let unit = require_opt(args, "--unit").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            systemd::get_unit(&unit)
        }
        "list-operations" => {
            let pattern = take_opt(args, "--pattern").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            operations::list_operations(pattern.as_deref())
        }
        "get-operation" => {
            let unit = require_opt(args, "--unit").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            operations::get_operation(&unit)
        }
        "list-timers" => {
            let pattern = take_opt(args, "--pattern").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            systemd::list_timers(pattern.as_deref())
        }
        "list-unit-files" => {
            let pattern = take_opt(args, "--pattern").map_err(BackendError)?;
            let state = take_opt(args, "--state").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            systemd::list_unit_files(state.as_deref(), pattern.as_deref())
        }
        "unit-dependencies" => {
            let unit = require_opt(args, "--unit").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            systemd::unit_dependencies(&unit)
        }
        "unit-logs" => {
            let unit = require_opt(args, "--unit").map_err(BackendError)?;
            let lines = take_opt(args, "--lines")
                .map_err(BackendError)?
                .and_then(|s| s.parse().ok())
                .unwrap_or(50);
            let since = take_opt(args, "--since").map_err(BackendError)?;
            let until = take_opt(args, "--until").map_err(BackendError)?;
            let priority = take_opt(args, "--priority")
                .map_err(BackendError)?
                .and_then(|s| s.parse().ok());
            let grep = take_opt(args, "--grep").map_err(BackendError)?;
            let boot = take_opt(args, "--boot")
                .map_err(BackendError)?
                .and_then(|s| s.parse().ok());
            remaining_flags(args).map_err(BackendError)?;
            let filter = LogFilter {
                lines,
                priority,
                since: since.as_deref(),
                until: until.as_deref(),
                boot,
                grep: grep.as_deref(),
            };
            systemd::unit_logs(&unit, &filter)
        }
        other => Err(BackendError(format!("unknown inspect command '{other}'"))),
    }
}

fn run_control(
    cmd: &str,
    args: &mut Vec<String>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    match cmd {
        "plan" => {
            let action = require_opt(args, "--action").map_err(BackendError)?;
            let unit = require_opt(args, "--unit").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            let action = Action::parse(&action)
                .ok_or_else(|| BackendError(format!("unknown action '{action}'")))?;
            if !write::action_visible(action) {
                return Err(BackendError(
                    "this action is not available on the compact surface".into(),
                ));
            }
            write::plan(action, &unit, None)
        }
        "apply" => {
            let token = require_opt(args, "--plan-token")
                .or_else(|_| require_opt(args, "--token"))
                .map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            let cfg = systemd_ops::config::current_or_load()?;
            let plan = token::parse(&cfg, &token)?;
            token::require_class(&plan, PlanClass::Control)?;
            write::apply_with_context(&token, cwd)
        }
        other => Err(BackendError(format!("unknown control command '{other}'"))),
    }
}

fn run_author(cmd: &str, args: &mut Vec<String>, cwd: Option<&str>) -> Result<Value, BackendError> {
    match cmd {
        "plan-create" => {
            let spec = spec_from_flat(args).map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            operations::plan_create(&with_cwd(json!({ "spec": spec }), cwd))
        }
        "plan-update" => {
            let spec = spec_from_flat(args).map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            operations::plan_update(&with_cwd(json!({ "spec": spec }), cwd))
        }
        "plan-retire" => {
            let unit = require_opt(args, "--unit").map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            operations::plan_retire(&with_cwd(json!({ "unit": unit }), cwd))
        }
        "apply" => {
            let token = require_opt(args, "--plan-token")
                .or_else(|_| require_opt(args, "--token"))
                .map_err(BackendError)?;
            remaining_flags(args).map_err(BackendError)?;
            let cfg = systemd_ops::config::current_or_load()?;
            let plan = token::parse(&cfg, &token)?;
            token::require_class(&plan, PlanClass::Author)?;
            write::apply_with_context(&token, cwd)
        }
        other => Err(BackendError(format!("unknown author command '{other}'"))),
    }
}

fn main() -> ExitCode {
    let mut json_mode = false;
    let mut manager = None;
    let mut write_prefix = None;
    let mut cwd = None;
    let mut config_path = None;
    let mut rest = Vec::new();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" | "-V" => {
                println!("systemd-ops {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--json" => json_mode = true,
            _ if arg == "--manager" || arg.starts_with("--manager=") => {
                let Some(spec) = take_flag_value(&arg, "--manager", &mut args) else {
                    return fail(json_mode, "invalid_argument", "--manager needs an argument");
                };
                match Manager::parse(&spec) {
                    Some(m) => manager = Some(m),
                    None => {
                        return fail(
                            json_mode,
                            "invalid_argument",
                            &format!("unknown manager '{spec}' (known: user, system)"),
                        )
                    }
                }
            }
            _ if arg == "--write-prefix" || arg.starts_with("--write-prefix=") => {
                let Some(spec) = take_flag_value(&arg, "--write-prefix", &mut args) else {
                    return fail(
                        json_mode,
                        "invalid_argument",
                        "--write-prefix needs an argument",
                    );
                };
                write_prefix = Some(spec);
            }
            _ if arg == "--cwd" || arg.starts_with("--cwd=") => {
                let Some(spec) = take_flag_value(&arg, "--cwd", &mut args) else {
                    return fail(json_mode, "invalid_argument", "--cwd needs an argument");
                };
                cwd = Some(spec);
            }
            _ if arg == "--config" || arg.starts_with("--config=") => {
                let Some(spec) = take_flag_value(&arg, "--config", &mut args) else {
                    return fail(json_mode, "invalid_argument", "--config needs an argument");
                };
                config_path = Some(spec);
            }
            _ if arg == "--surface" || arg.starts_with("--surface=") => {
                let Some(spec) = take_flag_value(&arg, "--surface", &mut args) else {
                    return fail(json_mode, "invalid_argument", "--surface needs an argument");
                };
                match Surface::parse(&spec) {
                    Some(s) => systemd::set_surface(s),
                    None => {
                        return fail(
                            json_mode,
                            "invalid_argument",
                            &format!("unknown surface '{spec}'"),
                        )
                    }
                }
            }
            other if other.starts_with('-') => {
                return fail(
                    json_mode,
                    "invalid_argument",
                    &format!("unknown argument '{other}'"),
                );
            }
            other => {
                rest.push(other.to_string());
                rest.extend(args);
                break;
            }
        }
    }

    let cfg = match OpsConfig::load(
        manager,
        write_prefix,
        config_path.as_deref().map(std::path::Path::new),
    ) {
        Ok(c) => c,
        Err(e) => return fail(json_mode, "error", &e.0),
    };
    cfg.apply();
    systemd::set_surface(Surface::Compact);

    if rest.is_empty() {
        return fail(
            json_mode,
            "invalid_argument",
            "missing command (see --help)",
        );
    }
    let group = rest.remove(0);
    let cwd = match process_cwd(cwd) {
        Ok(c) => c,
        Err(e) => return fail(json_mode, "error", &e.0),
    };
    let cwd_ref = Some(cwd.as_str());
    if group == "tui" {
        remaining_flags(&rest).ok();
        return match tui::run(cwd_ref) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(json_mode, "error", &e.0),
        };
    }
    if rest.is_empty() {
        return fail(
            json_mode,
            "invalid_argument",
            &format!("missing {group} subcommand (see --help)"),
        );
    }
    let cmd = rest.remove(0);
    match group.as_str() {
        "inspect" => from_result(json_mode, run_inspect(&cmd, &mut rest)),
        "control" => from_result(json_mode, run_control(&cmd, &mut rest, cwd_ref)),
        "author" => from_result(json_mode, run_author(&cmd, &mut rest, cwd_ref)),
        "scope" => from_result(json_mode, run_scope(&cmd, &mut rest, cwd_ref)),
        other => fail(
            json_mode,
            "invalid_argument",
            &format!("unknown command '{other}' (inspect, control, author, scope, tui)"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_cwd_explicit_overrides() {
        assert_eq!(
            process_cwd(Some("/project/personal".into())).unwrap(),
            "/project/personal"
        );
    }

    #[test]
    fn process_cwd_defaults_to_current_dir() {
        let got = process_cwd(None).unwrap();
        let here = std::env::current_dir().unwrap();
        assert_eq!(got, here.to_string_lossy());
    }
}
