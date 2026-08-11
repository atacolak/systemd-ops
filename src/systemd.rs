//! The systemd backend.
//!
//! systemd's command-line tools expose stable, documented JSON output
//! (`systemctl --output=json`, `journalctl --output=json`). We use those as
//! the machine interface rather than linking libsystemd: it keeps the binary
//! dependency-free, version-tolerant, and every call auditable as a plain
//! process invocation.
//!
//! Everything in this module is read-only by construction: no verb we invoke
//! can mutate state, and the argument lists are built from vetted values only.

use std::fmt;
use std::process::Command;

use serde_json::{json, Value};

/// Capability scopes. A tool is only advertised and callable if its scope
/// was granted on the command line. This is the entire point of the program:
/// authority handed to an agent should be explicit, minimal, and enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    UnitsRead,
    JournalRead,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "units:read" => Some(Scope::UnitsRead),
            "journal:read" => Some(Scope::JournalRead),
            _ => None,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::UnitsRead => write!(f, "units:read"),
            Scope::JournalRead => write!(f, "journal:read"),
        }
    }
}

/// The set of granted scopes.
#[derive(Debug, Default)]
pub struct Grants(Vec<Scope>);

impl Grants {
    pub fn from_args(spec: &str) -> Result<Self, String> {
        let mut scopes = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let scope = Scope::parse(part)
                .ok_or_else(|| format!("unknown scope '{part}' (known: units:read, journal:read)"))?;
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        Ok(Grants(scopes))
    }

    pub fn allows(&self, scope: Scope) -> bool {
        self.0.contains(&scope)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A failure while talking to systemd, reported to the model as a normal
/// tool error so it can react (retry, ask, give up) instead of crashing us.
#[derive(Debug)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Validates a unit name before it goes anywhere near an argument list.
///
/// systemd unit names are a known, small alphabet. Anything else is refused —
/// not because `Command` is exploitable (it never passes through a shell),
/// but because refusing malformed input early gives the model a precise error
/// instead of a confusing one from systemctl.
fn validate_unit_name(name: &str) -> Result<(), BackendError> {
    let ok = !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@\\:".contains(c));
    if ok {
        Ok(())
    } else {
        Err(BackendError(format!("'{name}' is not a valid unit name")))
    }
}

/// Runs a command and parses its stdout as JSON.
fn run_json(program: &str, args: &[&str]) -> Result<Value, BackendError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| BackendError(format!("failed to run {program}: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BackendError(format!(
            "{program} exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|e| BackendError(format!("{program} produced invalid JSON: {e}")))
}

/// `list_units`: all currently loaded units, optionally filtered by state.
pub fn list_units(state: Option<&str>) -> Result<Value, BackendError> {
    let mut args = vec!["list-units", "--all", "--output=json", "--no-pager"];
    let state_arg;
    if let Some(state) = state {
        // Vetted filter values; systemctl would reject others anyway, but an
        // allowlist keeps the contract obvious.
        const STATES: &[&str] = &["active", "inactive", "failed", "activating", "deactivating"];
        if !STATES.contains(&state) {
            return Err(BackendError(format!(
                "unknown state filter '{state}' (known: {})",
                STATES.join(", ")
            )));
        }
        state_arg = format!("--state={state}");
        args.push(&state_arg);
    }
    run_json("systemctl", &args)
}

/// `failed_units`: shorthand for the question every session starts with.
pub fn failed_units() -> Result<Value, BackendError> {
    run_json(
        "systemctl",
        &["list-units", "--state=failed", "--output=json", "--no-pager"],
    )
}

/// `unit_properties`: the full property set of one unit.
///
/// `systemctl show` emits `Key=Value` lines rather than JSON; we convert them
/// into an object so the model gets structure, not a wall of text.
pub fn unit_properties(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let output = Command::new("systemctl")
        .args(["show", "--no-pager", "--", name])
        .output()
        .map_err(|e| BackendError(format!("failed to run systemctl: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BackendError(format!(
            "systemctl show failed: {}",
            stderr.trim()
        )));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let props: serde_json::Map<String, Value> = text
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), Value::String(value.to_string())))
        })
        .collect();

    if props.is_empty() {
        return Err(BackendError(format!("no such unit: {name}")));
    }
    Ok(json!({ "unit": name, "properties": props }))
}

/// `unit_logs`: the last `lines` journal entries for one unit.
///
/// journalctl emits one JSON object per line; we keep the fields a human
/// would read and drop the dozens of internal ones, because handing a model
/// 40 metadata fields per line is how context windows die.
pub fn unit_logs(name: &str, lines: u64) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let lines = lines.clamp(1, 1000);
    let n = lines.to_string();
    // `--unit=` binds flag and value in one argv element. The previous
    // `-u -- <name>` made getopt consume `--` as -u's argument and pass the
    // real unit name as a raw journal match, failing every call with
    // "Failed to add match". The name is validated above and can never
    // start with '-'.
    let unit_arg = format!("--unit={name}");
    let output = Command::new("journalctl")
        .args(["--output=json", "--no-pager", "-n", &n, &unit_arg])
        .output()
        .map_err(|e| BackendError(format!("failed to run journalctl: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BackendError(format!(
            "journalctl failed: {}",
            stderr.trim()
        )));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let entries: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|entry| {
            json!({
                "timestamp": entry.get("__REALTIME_TIMESTAMP").cloned().unwrap_or(Value::Null),
                "priority": entry.get("PRIORITY").cloned().unwrap_or(Value::Null),
                "message": entry.get("MESSAGE").cloned().unwrap_or(Value::Null),
                "pid": entry.get("_PID").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();

    Ok(json!({ "unit": name, "entries": entries }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_names() {
        assert!(validate_unit_name("ssh.service").is_ok());
        assert!(validate_unit_name("user@1000.service").is_ok());
        assert!(validate_unit_name("dev-disk-by\\x2duuid.device").is_ok());
        assert!(validate_unit_name("").is_err());
        assert!(validate_unit_name("-rf").is_err());
        assert!(validate_unit_name("a b").is_err());
        assert!(validate_unit_name("x;reboot").is_err());
        assert!(validate_unit_name(&"x".repeat(300)).is_err());
    }

    #[test]
    fn grants() {
        let g = Grants::from_args("units:read, journal:read").unwrap();
        assert!(g.allows(Scope::UnitsRead));
        assert!(g.allows(Scope::JournalRead));
        assert!(Grants::from_args("units:write").is_err());
        assert!(Grants::from_args("").unwrap().is_empty());
    }
}
