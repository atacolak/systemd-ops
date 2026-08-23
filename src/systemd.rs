//! systemd CLI and varlink backend.
//!
//! Reads go through `systemctl` / `journalctl` / `systemd-analyze`
//! (`--output=json` or `Key=Value` show). The system manager also
//! tries PID 1 varlink for `list_units` and `boot_times`, then falls
//! back to the CLI. The user manager is CLI-only (`--user`).
//!
//! The only mutating invocation is `apply_verb`, reached only from
//! `crate::write` after a sealed plan passes its precondition.

use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::PathBuf;
use std::process::{Command, Output};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    System,
    User,
}

impl Manager {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "system" => Some(Manager::System),
            "user" => Some(Manager::User),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Manager::System => "system",
            Manager::User => "user",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Full,
    Compact,
}

impl Surface {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "full" => Some(Surface::Full),
            "compact" => Some(Surface::Compact),
            _ => None,
        }
    }
}

thread_local! {
    static MANAGER: Cell<Manager> = const { Cell::new(Manager::System) };
    static SURFACE: Cell<Surface> = const { Cell::new(Surface::Full) };
    static WRITE_PREFIX: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

pub fn set_manager(manager: Manager) {
    MANAGER.with(|c| c.set(manager));
}

pub fn manager() -> Manager {
    MANAGER.with(Cell::get)
}

pub fn set_surface(surface: Surface) {
    SURFACE.with(|c| c.set(surface));
}

pub fn surface() -> Surface {
    SURFACE.with(Cell::get)
}

pub fn parse_write_prefix(spec: &str) -> Result<Vec<String>, String> {
    let mut globs = Vec::new();
    for part in spec.split(',') {
        let g = part.trim();
        if g.is_empty() || g.len() > 128 {
            return Err("--write-prefix glob must be 1..128 characters".into());
        }
        globs.push(g.to_string());
    }
    if globs.is_empty() {
        return Err("--write-prefix needs at least one glob".into());
    }
    Ok(globs)
}

pub fn set_write_prefix(prefix: Option<String>) {
    let parsed = prefix.map(|s| parse_write_prefix(&s).unwrap_or_else(|e| panic!("{e}")));
    WRITE_PREFIX.with(|c| *c.borrow_mut() = parsed);
}

pub fn write_prefix() -> Option<Vec<String>> {
    WRITE_PREFIX.with(|c| c.borrow().clone())
}

pub fn write_unit_allowed(name: &str) -> bool {
    write_prefix()
        .map(|globs| globs.iter().any(|g| glob_match(g, name)))
        .unwrap_or(false)
}

pub fn require_write_unit(name: &str) -> Result<(), BackendError> {
    if write_unit_allowed(name) {
        return Ok(());
    }
    match write_prefix() {
        None => Err(BackendError(format!(
            "writes are disabled until a write-prefix is configured; refused '{name}'"
        ))),
        Some(globs) => Err(BackendError(format!(
            "writes are restricted to units matching '{}'; refused '{name}'",
            globs.join(",")
        ))),
    }
}

const COMPACT_TOOLS: &[&str] = &[
    "list_units",
    "failed_units",
    "get_unit",
    "list_operations",
    "get_operation",
    "list_timers",
    "list_unit_files",
    "unit_dependencies",
    "unit_logs",
    "plan_change",
    "plan_create_operation",
    "plan_update_operation",
    "plan_retire_operation",
    "apply_plan",
];

pub fn compact_tool(name: &str) -> bool {
    COMPACT_TOOLS.contains(&name)
}

pub fn tool_visible(name: &str) -> bool {
    surface() == Surface::Full || compact_tool(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    UnitsRead,
    JournalRead,
    BootRead,
    UnitsWrite,
}

impl Scope {
    const ALL: &'static [Scope] = &[
        Scope::UnitsRead,
        Scope::JournalRead,
        Scope::BootRead,
        Scope::UnitsWrite,
    ];

    fn name(self) -> &'static str {
        match self {
            Scope::UnitsRead => "units:read",
            Scope::JournalRead => "journal:read",
            Scope::BootRead => "boot:read",
            Scope::UnitsWrite => "units:write",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Scope::ALL.iter().copied().find(|scope| scope.name() == s)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Default)]
pub struct Grants(Vec<Scope>);

impl Grants {
    pub fn from_args(spec: &str) -> Result<Self, String> {
        let mut scopes = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let scope = Scope::parse(part).ok_or_else(|| {
                let known: Vec<&str> = Scope::ALL.iter().map(|s| s.name()).collect();
                format!("unknown scope '{part}' (known: {})", known.join(", "))
            })?;
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        Ok(Grants(scopes))
    }

    pub fn extend(&mut self, other: Grants) {
        for scope in other.0 {
            if !self.0.contains(&scope) {
                self.0.push(scope);
            }
        }
    }

    pub fn allows(&self, scope: Scope) -> bool {
        self.0.contains(&scope)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

pub(crate) fn validate_unit_name(name: &str) -> Result<(), BackendError> {
    let ok = !name.is_empty()
        && name.len() <= 256
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@\\:".contains(c));
    if ok {
        Ok(())
    } else {
        Err(BackendError(format!("'{name}' is not a valid unit name")))
    }
}

pub fn glob_match(pattern: &str, name: &str) -> bool {
    let (pat, text) = (pattern.as_bytes(), name.as_bytes());
    let mut i = 0;
    let mut j = 0;
    let mut star = None;
    let mut restart = 0;
    while j < text.len() {
        if i < pat.len() && pat[i] == b'*' {
            star = Some(i);
            restart = j;
            i += 1;
        } else if i < pat.len() && (pat[i] == b'?' || pat[i] == text[j]) {
            i += 1;
            j += 1;
        } else if let Some(at) = star {
            restart += 1;
            j = restart;
            i = at + 1;
        } else {
            return false;
        }
    }
    pat[i..].iter().all(|&c| c == b'*')
}

fn glob_on<'a>(
    field: &'a str,
    pattern: Option<&'a str>,
) -> Result<impl Fn(&Value) -> bool + 'a, BackendError> {
    if let Some(p) = pattern {
        if p.is_empty() || p.len() > 256 {
            return Err(BackendError(
                "pattern must be 1..256 characters, e.g. 'nginx*' or '*.timer'".into(),
            ));
        }
    }
    Ok(move |row: &Value| match pattern {
        None => true,
        Some(p) => row
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|name| glob_match(p, name)),
    })
}

struct Proc {
    program: &'static str,
    args: Vec<String>,
    journal_empty_ok: bool,
}

impl Proc {
    fn new(program: &'static str) -> Self {
        Self {
            program,
            args: Vec::new(),
            journal_empty_ok: false,
        }
    }

    fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    fn empty_journal_ok(mut self) -> Self {
        self.journal_empty_ok = true;
        self
    }

    fn output(self) -> Result<Output, BackendError> {
        let mut cmd = Command::new(self.program);
        if manager() == Manager::User
            && matches!(self.program, "systemctl" | "journalctl" | "systemd-analyze")
        {
            cmd.arg("--user");
        }
        let output = cmd
            .args(&self.args)
            .env("LC_ALL", "C")
            .env("SYSTEMD_PAGER", "cat")
            .env("SYSTEMD_COLORS", "false")
            .output()
            .map_err(|e| BackendError(format!("failed to run {}: {e}", self.program)))?;
        let empty_search = self.journal_empty_ok
            && output.status.code() == Some(1)
            && output.stdout.is_empty()
            && output.stderr.is_empty();
        if output.status.success() || empty_search {
            Ok(output)
        } else {
            Err(BackendError(format!(
                "{} exited with {}: {}",
                self.program,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn stdout(self) -> Result<Vec<u8>, BackendError> {
        self.output().map(|o| o.stdout)
    }

    fn json(self) -> Result<Value, BackendError> {
        let program = self.program;
        let stdout = self.stdout()?;
        serde_json::from_slice(&stdout)
            .map_err(|e| BackendError(format!("{program} produced invalid JSON: {e}")))
    }

    fn json_array(self) -> Result<Vec<Value>, BackendError> {
        let program = self.program;
        match self.json()? {
            Value::Array(rows) => Ok(rows),
            _ => Err(BackendError(format!(
                "{program} did not produce a JSON array"
            ))),
        }
    }

    fn key_values(self) -> Result<Map<String, Value>, BackendError> {
        let stdout = self.stdout()?;
        Ok(String::from_utf8_lossy(&stdout)
            .lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                Some((key.to_string(), Value::String(value.to_string())))
            })
            .collect())
    }
}

fn systemctl() -> Proc {
    Proc::new("systemctl")
}

fn journalctl() -> Proc {
    Proc::new("journalctl")
}

fn analyze() -> Proc {
    Proc::new("systemd-analyze")
}

fn show(name: &str, properties: &[&str]) -> Result<Map<String, Value>, BackendError> {
    let mut p = systemctl().arg("show").arg("--no-pager");
    if !properties.is_empty() {
        p = p.arg(format!("--property={}", properties.join(",")));
    }
    p.arg("--").arg(name).key_values()
}

fn list_json(verb: &str) -> Result<Vec<Value>, BackendError> {
    systemctl()
        .arg(verb)
        .arg("--all")
        .arg("--output=json")
        .arg("--no-pager")
        .json_array()
}

fn load_state(name: &str) -> Result<Option<String>, BackendError> {
    Ok(show(name, &["LoadState"])?
        .get("LoadState")
        .and_then(Value::as_str)
        .map(str::to_string))
}

pub fn ensure_unit_known(name: &str) -> Result<(), BackendError> {
    match load_state(name)?.as_deref() {
        Some("not-found") => Err(BackendError(format!("no such unit: {name}"))),
        _ => Ok(()),
    }
}

pub(crate) fn unit_state(name: &str) -> Result<(String, String), BackendError> {
    let props = show(name, &["ActiveState", "SubState"])?;
    let get = |key: &str| {
        props
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string()
    };
    Ok((get("ActiveState"), get("SubState")))
}

pub fn list_boots() -> Result<Value, BackendError> {
    journalctl()
        .arg("--list-boots")
        .arg("--output=json")
        .arg("--no-pager")
        .json()
}

pub(crate) fn service_log_get(verb: &str, unit: &str) -> Result<String, BackendError> {
    let stdout = systemctl()
        .arg(verb)
        .arg("--no-pager")
        .arg("--")
        .arg(unit)
        .stdout()?;
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}

pub fn unit_log_control(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    Ok(json!({
        "unit": name,
        "log_level": service_log_get("service-log-level", name)?,
        "log_target": service_log_get("service-log-target", name)?,
    }))
}

pub(crate) fn unit_file_state(name: &str) -> Result<String, BackendError> {
    Ok(show(name, &["UnitFileState"])?
        .get("UnitFileState")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

fn collect_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

pub fn apply_verb(
    verb: &str,
    unit: &str,
    value: Option<&str>,
) -> Result<Vec<String>, BackendError> {
    let mut p = systemctl().arg(verb).arg("--no-pager").arg("--").arg(unit);
    if let Some(value) = value {
        p = p.arg(value);
    }
    let output = p.output()?;
    let mut changes = collect_lines(&output.stdout);
    changes.extend(collect_lines(&output.stderr));
    Ok(changes)
}

pub(crate) fn unit_file_dir() -> PathBuf {
    match manager() {
        Manager::User => {
            if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
                PathBuf::from(xdg).join("systemd/user")
            } else if let Some(home) = std::env::var_os("HOME") {
                PathBuf::from(home).join(".config/systemd/user")
            } else {
                PathBuf::from("/tmp/systemd-user")
            }
        }
        Manager::System => PathBuf::from("/etc/systemd/system"),
    }
}

pub(crate) fn daemon_reload() -> Result<Vec<String>, BackendError> {
    let output = systemctl()
        .arg("daemon-reload")
        .arg("--no-pager")
        .output()?;
    let mut changes = collect_lines(&output.stdout);
    changes.extend(collect_lines(&output.stderr));
    Ok(changes)
}

pub(crate) fn try_disable(unit: &str) -> Result<Vec<String>, BackendError> {
    apply_verb("disable", unit, None).or_else(|_| Ok(Vec::new()))
}

pub const STATES: &[&str] = &["active", "inactive", "failed", "activating", "deactivating"];

pub fn list_units(state: Option<&str>, pattern: Option<&str>) -> Result<Value, BackendError> {
    if let Some(state) = state {
        if !STATES.contains(&state) {
            return Err(BackendError(format!(
                "unknown state filter '{state}' (known: {})",
                STATES.join(", ")
            )));
        }
    }
    let matches_name = glob_on("unit", pattern)?;
    let units = if manager() == Manager::User {
        list_json("list-units")?
    } else {
        crate::varlink::list_units().or_else(|_| list_json("list-units"))?
    };
    Ok(Value::Array(
        units
            .into_iter()
            .filter(|u| state.is_none_or(|s| u["active"] == s) && matches_name(u))
            .map(|u| unit_row(&u))
            .collect(),
    ))
}

fn unit_row(unit: &Value) -> Value {
    let field = |key: &str| unit.get(key).and_then(Value::as_str).unwrap_or("");
    json!({
        "unit": field("unit"),
        "load": field("load"),
        "active": field("active"),
        "sub": field("sub"),
        "description": field("description"),
    })
}

pub fn failed_units() -> Result<Value, BackendError> {
    list_units(Some("failed"), None)
}

pub fn list_timers(pattern: Option<&str>) -> Result<Value, BackendError> {
    let matches_name = glob_on("unit", pattern)?;
    let timers = list_json("list-timers")?;
    Ok(Value::Array(
        timers
            .iter()
            .filter(|row| matches_name(row))
            .map(timer_row)
            .collect(),
    ))
}

fn timer_row(timer: &Value) -> Value {
    let time = |key: &str| match timer.get(key).and_then(Value::as_u64) {
        Some(usec) if usec > 0 && usec != u64::MAX => json!(usec_to_rfc3339(usec)),
        _ => Value::Null,
    };
    json!({
        "unit": timer.get("unit").and_then(Value::as_str).unwrap_or(""),
        "activates": timer.get("activates").cloned().unwrap_or(Value::Null),
        "next": time("next"),
        "last": time("last"),
    })
}

pub fn list_sockets(pattern: Option<&str>) -> Result<Value, BackendError> {
    let matches_name = glob_on("unit", pattern)?;
    Ok(Value::Array(
        list_json("list-sockets")?
            .into_iter()
            .filter(|row| matches_name(row))
            .collect(),
    ))
}

pub fn list_unit_files(state: Option<&str>, pattern: Option<&str>) -> Result<Value, BackendError> {
    let matches_name = glob_on("unit_file", pattern)?;
    let files = systemctl()
        .arg("list-unit-files")
        .arg("--output=json")
        .arg("--no-pager")
        .json_array()?;
    Ok(Value::Array(
        files
            .into_iter()
            .filter(|f| state.is_none_or(|s| f["state"] == s) && matches_name(f))
            .collect(),
    ))
}

const DEPENDENCY_PROPS: &[&str] = &[
    "Requires",
    "Requisite",
    "Wants",
    "BindsTo",
    "PartOf",
    "Upholds",
    "Conflicts",
    "Before",
    "After",
    "WantedBy",
    "RequiredBy",
    "BoundBy",
    "UpheldBy",
    "TriggeredBy",
    "Triggers",
];

pub fn unit_dependencies(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    ensure_unit_known(name)?;
    let props = show(name, DEPENDENCY_PROPS)?;
    let deps: Map<String, Value> = DEPENDENCY_PROPS
        .iter()
        .map(|&key| {
            let units: Vec<&str> = props
                .get(key)
                .and_then(Value::as_str)
                .map(|v| v.split_whitespace().collect())
                .unwrap_or_default();
            (key.to_string(), json!(units))
        })
        .collect();
    Ok(json!({ "unit": name, "dependencies": deps }))
}

pub fn unit_security(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let analysis = analyze()
        .arg("security")
        .arg("--json=short")
        .arg("--no-pager")
        .arg("--")
        .arg(name)
        .json()?;
    Ok(json!({ "unit": name, "analysis": analysis }))
}

pub fn unit_properties(name: &str, select: &[String]) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let props = show(name, &[])?;
    if props.get("LoadState").and_then(Value::as_str) == Some("not-found") {
        return Err(BackendError(format!("no such unit: {name}")));
    }
    if select.is_empty() {
        return Ok(json!({ "unit": name, "properties": props }));
    }
    let selected: Map<String, Value> = select
        .iter()
        .filter_map(|key| Some((key.clone(), props.get(key)?.clone())))
        .collect();
    if selected.is_empty() {
        return Err(BackendError(format!(
            "{name} has none of the requested properties: {}",
            select.join(", ")
        )));
    }
    Ok(json!({ "unit": name, "properties": selected }))
}

const GET_UNIT_PROPS: &[&str] = &[
    "Description",
    "LoadState",
    "ActiveState",
    "SubState",
    "UnitFileState",
    "FragmentPath",
    "MainPID",
    "NRestarts",
    "MemoryCurrent",
    "CPUUsageNSec",
    "Result",
    "ExecMainStatus",
    "ExecMainCode",
    "ActiveEnterTimestamp",
    "InactiveEnterTimestamp",
    "ExecMainStartTimestamp",
    "StateChangeTimestamp",
];

fn parse_u64_prop(props: &Map<String, Value>, key: &str) -> Value {
    match props.get(key).and_then(Value::as_str) {
        Some(s) if s.is_empty() || s == "[not set]" || s == "n/a" => Value::Null,
        Some(s) => s
            .parse::<u64>()
            .map(Value::from)
            .unwrap_or(Value::String(s.to_string())),
        None => Value::Null,
    }
}

fn parse_string_prop(props: &Map<String, Value>, key: &str) -> Value {
    match props.get(key).and_then(Value::as_str) {
        Some(s) if s.is_empty() || s == "[not set]" || s == "n/a" => Value::Null,
        Some(s) => Value::String(s.to_string()),
        None => Value::Null,
    }
}

pub fn get_unit(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let props = show(name, GET_UNIT_PROPS)?;
    if props.get("LoadState").and_then(Value::as_str) == Some("not-found") {
        return Err(BackendError(format!("no such unit: {name}")));
    }
    Ok(json!({
        "unit": name,
        "description": parse_string_prop(&props, "Description"),
        "load": parse_string_prop(&props, "LoadState"),
        "active": parse_string_prop(&props, "ActiveState"),
        "sub": parse_string_prop(&props, "SubState"),
        "enabled": parse_string_prop(&props, "UnitFileState"),
        "unit_file": parse_string_prop(&props, "FragmentPath"),
        "pid": parse_u64_prop(&props, "MainPID"),
        "restarts": parse_u64_prop(&props, "NRestarts"),
        "memory_bytes": parse_u64_prop(&props, "MemoryCurrent"),
        "cpu_nsec": parse_u64_prop(&props, "CPUUsageNSec"),
        "result": parse_string_prop(&props, "Result"),
        "exit_status": parse_u64_prop(&props, "ExecMainStatus"),
        "exit_code": parse_u64_prop(&props, "ExecMainCode"),
        "active_enter": parse_string_prop(&props, "ActiveEnterTimestamp"),
        "inactive_enter": parse_string_prop(&props, "InactiveEnterTimestamp"),
        "exec_start": parse_string_prop(&props, "ExecMainStartTimestamp"),
        "state_change": parse_string_prop(&props, "StateChangeTimestamp"),
    }))
}

#[derive(Default)]
pub struct LogFilter<'a> {
    pub lines: u64,
    pub priority: Option<u64>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub boot: Option<i64>,
    pub grep: Option<&'a str>,
}

fn validate_time_spec(spec: &str) -> Result<(), BackendError> {
    let ok = !spec.is_empty()
        && spec.len() <= 64
        && spec
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " :+-.@/".contains(c));
    if ok {
        Ok(())
    } else {
        Err(BackendError(format!(
            "'{spec}' is not a valid time specification"
        )))
    }
}

pub fn unit_logs(name: &str, filter: &LogFilter) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let n = filter.lines.clamp(1, 1000).to_string();
    let mut p = journalctl()
        .arg("--output=json")
        .arg("--output-fields=MESSAGE,PRIORITY,_PID")
        .arg("--no-pager")
        .arg("-n")
        .arg(n)
        .arg(format!("--unit={name}"));
    if let Some(prio) = filter.priority {
        if prio > 7 {
            return Err(BackendError(format!(
                "priority {prio} out of range (0=emerg .. 7=debug)"
            )));
        }
        p = p.arg(format!("--priority={prio}"));
    }
    if let Some(since) = filter.since {
        validate_time_spec(since)?;
        p = p.arg(format!("--since={since}"));
    }
    if let Some(until) = filter.until {
        validate_time_spec(until)?;
        p = p.arg(format!("--until={until}"));
    }
    if let Some(boot) = filter.boot {
        if !(-1000..=1000).contains(&boot) {
            return Err(BackendError(format!("boot offset {boot} out of range")));
        }
        p = p.arg(format!("--boot={boot}"));
    }
    if let Some(grep) = filter.grep {
        if grep.is_empty() || grep.len() > 256 {
            return Err(BackendError(
                "grep pattern must be 1..256 characters".into(),
            ));
        }
        p = p.arg(format!("--grep={grep}"));
    }
    let stdout = p.empty_journal_ok().stdout()?;
    let entries: Vec<Value> = String::from_utf8_lossy(&stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|entry| {
            let str_field = |key: &str| entry.get(key).and_then(Value::as_str);
            json!({
                "timestamp": str_field("__REALTIME_TIMESTAMP")
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(usec_to_rfc3339),
                "priority": str_field("PRIORITY").and_then(|s| s.parse::<u8>().ok()),
                "message": entry.get("MESSAGE").cloned().unwrap_or(Value::Null),
                "pid": str_field("_PID").and_then(|s| s.parse::<u64>().ok()),
            })
        })
        .collect();
    let mut reply = json!({ "unit": name, "entries": entries });
    if reply["entries"].as_array().is_some_and(Vec::is_empty) && ensure_unit_known(name).is_err() {
        reply["note"] = json!(
            "no entries, and no unit by this name is currently loaded: \
             check the name, or the boot offset if it is from an earlier boot"
        );
    }
    Ok(reply)
}

pub fn usec_to_rfc3339(usec: u64) -> String {
    let secs = (usec / 1_000_000) as i64;
    let micros = usec % 1_000_000;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        sod / 3_600,
        (sod % 3_600) / 60,
        sod % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        yoe + era * 400 + i64::from(month <= 2),
        month as u32,
        day as u32,
    )
}

pub fn boot_times() -> Result<Value, BackendError> {
    let stamps = if manager() == Manager::User {
        cli_boot_timestamps()
    } else {
        crate::varlink::boot_timestamps().or_else(|_| cli_boot_timestamps())
    }?;
    let [firmware, loader, initrd, userspace, finish] = stamps;
    compute_boot_times(firmware, loader, initrd, userspace, finish, in_container())
}

fn in_container() -> bool {
    Proc::new("systemd-detect-virt")
        .arg("--container")
        .arg("--quiet")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn cli_boot_timestamps() -> Result<[u64; 5], BackendError> {
    const PROPS: [&str; 5] = [
        "FirmwareTimestampMonotonic",
        "LoaderTimestampMonotonic",
        "InitRDTimestampMonotonic",
        "UserspaceTimestampMonotonic",
        "FinishTimestampMonotonic",
    ];
    let props = systemctl()
        .arg("show")
        .arg("--no-pager")
        .arg(format!("--property={}", PROPS.join(",")))
        .key_values()?;
    Ok(PROPS.map(|key| {
        props
            .get(key)
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    }))
}

fn compute_boot_times(
    firmware: u64,
    loader: u64,
    initrd: u64,
    userspace: u64,
    finish: u64,
    container: bool,
) -> Result<Value, BackendError> {
    if finish == 0 {
        return Err(BackendError(
            "bootup is not yet finished; try again once startup completes".into(),
        ));
    }
    let mut phases = Map::new();
    let mut insert = |key: &str, usec: u64| {
        phases.insert(key.to_string(), json!(usec));
    };
    if container {
        insert("userspace_usec", finish.saturating_sub(userspace));
        insert("total_usec", finish.saturating_sub(userspace));
        insert("container", 1);
        return Ok(Value::Object(phases));
    }
    if firmware > loader {
        insert("firmware_usec", firmware - loader);
    }
    if loader > 0 {
        insert("loader_usec", loader);
    }
    if initrd > 0 {
        insert("kernel_usec", initrd);
        insert("initrd_usec", userspace.saturating_sub(initrd));
    } else {
        insert("kernel_usec", userspace);
    }
    insert("userspace_usec", finish.saturating_sub(userspace));
    insert("total_usec", firmware.max(loader) + finish);
    Ok(Value::Object(phases))
}

pub fn critical_chain(unit: Option<&str>) -> Result<Value, BackendError> {
    let mut p = analyze().arg("critical-chain").arg("--no-pager");
    if let Some(unit) = unit {
        validate_unit_name(unit)?;
        p = p.arg("--").arg(unit);
    }
    let stdout = p.stdout()?;
    let text = String::from_utf8_lossy(&stdout);
    let chain = parse_critical_chain(&text);
    if chain.is_empty() {
        return Err(BackendError(format!(
            "could not parse systemd-analyze critical-chain output: {}",
            text.trim()
        )));
    }
    Ok(json!({ "chain": chain }))
}

fn strip_tree_prefix(line: &str) -> &str {
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start_matches(' ');
        let stripped = ["└─", "├─", "│", "`-", "|-", "|"]
            .iter()
            .find_map(|connector| trimmed.strip_prefix(connector));
        match stripped {
            Some(next) => rest = next,
            None => return trimmed,
        }
    }
}

fn parse_critical_chain(text: &str) -> Vec<Value> {
    let mut chain = Vec::new();
    for line in text.lines() {
        let rest = strip_tree_prefix(line);
        let depth = (line.chars().count() - rest.chars().count()) / 2;
        let mut tokens = rest.split_whitespace();
        let Some(name) = tokens.next() else { continue };
        if !name.contains('.') || validate_unit_name(name).is_err() {
            continue;
        }
        let mut activated: Option<String> = None;
        let mut duration: Option<String> = None;
        for token in tokens {
            if let Some(t) = token.strip_prefix('@') {
                activated = Some(t.to_string());
            } else if let Some(t) = token.strip_prefix('+') {
                duration = Some(t.to_string());
            } else if let Some(span) = duration.as_mut().or(activated.as_mut()) {
                span.push(' ');
                span.push_str(token);
            }
        }
        chain.push(json!({
            "unit": name,
            "depth": depth,
            "activated": activated,
            "duration": duration,
        }));
    }
    chain
}

pub fn boot_blame(limit: usize) -> Result<Value, BackendError> {
    let stdout = analyze().arg("blame").arg("--no-pager").stdout()?;
    let text = String::from_utf8_lossy(&stdout);
    let mut blame = parse_blame(&text);
    if blame.is_empty() {
        return Err(BackendError(format!(
            "could not parse systemd-analyze blame output: {}",
            text.trim()
        )));
    }
    let total = blame.len();
    blame.truncate(limit.clamp(1, 1000));
    Ok(json!({ "blame": blame, "returned": blame.len(), "total": total }))
}

fn parse_blame(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let (time, unit) = line.trim().rsplit_once(' ')?;
            let time = time.trim();
            let plausible = time.chars().next().is_some_and(|c| c.is_ascii_digit())
                && unit.contains('.')
                && !unit.ends_with('.')
                && validate_unit_name(unit).is_ok();
            plausible.then(|| json!({ "unit": unit, "time": time }))
        })
        .collect()
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
        assert!(validate_unit_name("-.mount").is_ok());
        assert!(validate_unit_name("-.slice").is_ok());
        assert!(validate_unit_name("a b").is_err());
        assert!(validate_unit_name("x;reboot").is_err());
        assert!(validate_unit_name(&"x".repeat(300)).is_err());
    }

    #[test]
    fn globs() {
        assert!(glob_match("nginx*", "nginx.service"));
        assert!(glob_match("*.timer", "logrotate.timer"));
        assert!(glob_match("systemd-*.service", "systemd-journald.service"));
        assert!(glob_match("user@????.service", "user@1000.service"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("ssh.service", "ssh.service"));
        assert!(glob_match("ssh.service*", "ssh.service"));
        assert!(glob_match("*ssh.service", "ssh.service"));
        assert!(glob_match("*.service", "a.service.d.service"));
        assert!(!glob_match("nginx*", "not-nginx.service"));
        assert!(!glob_match("*.timer", "logrotate.timer.d"));
        assert!(!glob_match("user@????.service", "user@10.service"));
        assert!(!glob_match("ssh.service", "ssh.service.d"));
        assert!(!glob_match("", "ssh.service"));
        assert!(glob_match("a[bc.service", "a[bc.service"));
        assert!(!glob_match("a[bc].service", "ab.service"));
    }

    fn glob_match_naive(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, _) => n.is_empty(),
            (Some(b'*'), _) => {
                glob_match_naive(&p[1..], n) || (!n.is_empty() && glob_match_naive(p, &n[1..]))
            }
            (Some(b'?'), Some(_)) => glob_match_naive(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => glob_match_naive(&p[1..], &n[1..]),
            _ => false,
        }
    }

    #[test]
    fn glob_agrees_with_the_naive_oracle() {
        let pattern_alphabet = *b"ab*?";
        let name_alphabet = *b"ab";
        let words = |alphabet: &[u8], max: u32| {
            let mut out = vec![Vec::new()];
            let mut frontier = vec![Vec::new()];
            for _ in 0..max {
                let mut next = Vec::new();
                for word in &frontier {
                    for &c in alphabet {
                        let mut w: Vec<u8> = word.clone();
                        w.push(c);
                        next.push(w);
                    }
                }
                out.extend(next.iter().cloned());
                frontier = next;
            }
            out
        };
        let patterns = words(&pattern_alphabet, 4);
        let names = words(&name_alphabet, 4);
        let mut checked = 0;
        for p in &patterns {
            let p = std::str::from_utf8(p).unwrap();
            for n in &names {
                let n = std::str::from_utf8(n).unwrap();
                assert_eq!(
                    glob_match(p, n),
                    glob_match_naive(p.as_bytes(), n.as_bytes()),
                    "disagree: pattern {p:?} name {n:?}"
                );
                checked += 1;
            }
        }
        assert!(checked > 10_000, "only {checked} pairs compared");
    }

    #[test]
    fn name_filtering() {
        let rows = [
            json!({ "unit": "ssh.service" }),
            json!({ "unit": "a.timer" }),
        ];
        let f = glob_on("unit", Some("*.timer")).unwrap();
        assert_eq!(rows.iter().filter(|r| f(r)).count(), 1);
        let f = glob_on("unit", None).unwrap();
        assert!(f(&json!({})));
        assert!(glob_on("unit", Some("")).is_err());
        assert!(glob_on("unit", Some(&"*".repeat(300))).is_err());
    }

    #[test]
    fn timer_rows() {
        let row = timer_row(&json!({
            "unit": "logrotate.timer",
            "activates": "logrotate.service",
            "next": 1_582_934_400_123_456_u64,
            "last": 0,
            "left": 1_582_934_400_123_456_u64,
            "passed": 0,
        }));
        assert_eq!(row["unit"], json!("logrotate.timer"));
        assert_eq!(row["next"], json!("2020-02-29T00:00:00.123456Z"));
        assert_eq!(row["last"], json!(null));
        assert!(row.get("left").is_none() && row.get("passed").is_none());
        let row = timer_row(&json!({ "unit": "x.timer", "next": u64::MAX }));
        assert_eq!(row["next"], json!(null));
        assert_eq!(row["activates"], json!(null));
    }

    #[test]
    fn grants() {
        let g = Grants::from_args("units:read, journal:read,boot:read").unwrap();
        assert!(g.allows(Scope::UnitsRead));
        assert!(g.allows(Scope::JournalRead));
        assert!(g.allows(Scope::BootRead));
        let w = Grants::from_args("units:write").unwrap();
        assert!(w.allows(Scope::UnitsWrite));
        assert!(!w.allows(Scope::UnitsRead));
        assert!(Grants::from_args("units:delete").is_err());
        assert!(Grants::from_args("").unwrap().is_empty());
    }

    #[test]
    fn rfc3339_timestamps() {
        assert_eq!(usec_to_rfc3339(0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(
            usec_to_rfc3339(1_000_000_000_000_000),
            "2001-09-09T01:46:40.000000Z"
        );
        assert_eq!(
            usec_to_rfc3339(1_582_934_400_123_456),
            "2020-02-29T00:00:00.123456Z"
        );
    }

    #[test]
    fn boot_time_phases() {
        let v = compute_boot_times(
            5_000_000, 2_000_000, 3_000_000, 4_000_000, 10_000_000, false,
        )
        .unwrap();
        assert_eq!(v["firmware_usec"], json!(3_000_000));
        assert_eq!(v["loader_usec"], json!(2_000_000));
        assert_eq!(v["kernel_usec"], json!(3_000_000));
        assert_eq!(v["initrd_usec"], json!(1_000_000));
        assert_eq!(v["userspace_usec"], json!(6_000_000));
        assert_eq!(v["total_usec"], json!(15_000_000));

        let v = compute_boot_times(0, 0, 0, 4_000_000, 10_000_000, false).unwrap();
        assert!(v.get("firmware_usec").is_none());
        assert!(v.get("loader_usec").is_none());
        assert!(v.get("initrd_usec").is_none());
        assert_eq!(v["kernel_usec"], json!(4_000_000));
        assert_eq!(v["total_usec"], json!(10_000_000));

        assert!(compute_boot_times(0, 0, 0, 4_000_000, 0, false).is_err());

        let v = compute_boot_times(0, 0, 0, 27_007_369_578, 27_041_763_522, true).unwrap();
        assert_eq!(v["userspace_usec"], json!(34_393_944));
        assert_eq!(v["total_usec"], json!(34_393_944));
        assert!(v.get("kernel_usec").is_none());
    }

    #[test]
    fn critical_chain_parsing() {
        let text = "\
The time when unit became active or started is printed after the \"@\" character.
The time the unit took to start is printed after the \"+\" character.

graphical.target @1min 30.5s
└─multi-user.target @1min 30.5s
  └─nginx.service @58.2s +2.1s
    └─network-online.target @58.1s
      └─NetworkManager-wait-online.service @12.3s +45.8s
";
        let chain = parse_critical_chain(text);
        assert_eq!(chain.len(), 5);
        assert_eq!(chain[0]["unit"], json!("graphical.target"));
        assert_eq!(chain[0]["depth"], json!(0));
        assert_eq!(chain[0]["activated"], json!("1min 30.5s"));
        assert_eq!(chain[0]["duration"], json!(null));
        assert_eq!(chain[1]["depth"], json!(1));
        assert_eq!(chain[2]["unit"], json!("nginx.service"));
        assert_eq!(chain[2]["depth"], json!(2));
        assert_eq!(chain[2]["duration"], json!("2.1s"));
        assert_eq!(chain[3]["unit"], json!("network-online.target"));
        assert_eq!(
            chain[4]["unit"],
            json!("NetworkManager-wait-online.service")
        );
        assert_eq!(chain[4]["duration"], json!("45.8s"));
    }

    #[test]
    fn critical_chain_parses_the_ascii_tree() {
        let text = "\
graphical.target @1min 30.5s
`-multi-user.target @1min 30.5s
  `-nginx.service @58.2s +2.1s
    |-network-online.target @58.1s
    `--.mount @1.2s
";
        let chain = parse_critical_chain(text);
        assert_eq!(chain.len(), 5, "got: {chain:?}");
        assert_eq!(chain[1]["unit"], json!("multi-user.target"));
        assert_eq!(chain[2]["unit"], json!("nginx.service"));
        assert_eq!(chain[2]["duration"], json!("2.1s"));
        assert_eq!(chain[3]["unit"], json!("network-online.target"));
        assert_eq!(chain[4]["unit"], json!("-.mount"));
    }

    #[test]
    fn critical_chain_rejects_non_units() {
        let chain = parse_critical_chain("Bootup is not yet finished.\nno-dot-here @3s\n");
        assert!(chain.is_empty());
    }

    #[test]
    fn blame_parsing() {
        let text = "\
1min 30.5s NetworkManager-wait-online.service
    6.544s snapd.service
     122ms user@1000.service
";
        let blame = parse_blame(text);
        assert_eq!(blame.len(), 3);
        assert_eq!(blame[0]["time"], json!("1min 30.5s"));
        assert_eq!(
            blame[0]["unit"],
            json!("NetworkManager-wait-online.service")
        );
        assert_eq!(blame[2]["time"], json!("122ms"));
        assert_eq!(blame[2]["unit"], json!("user@1000.service"));
        assert!(parse_blame("Bootup is not yet finished. Please try again later.\n").is_empty());
    }

    #[test]
    fn write_prefix_matches_managed_glob() {
        set_write_prefix(Some("managed-*".into()));
        assert!(write_unit_allowed("managed-test-demo.service"));
        assert!(write_unit_allowed("managed-mail-check.timer"));
        assert!(!write_unit_allowed("bluetooth.service"));
        assert!(!write_unit_allowed("syncthing.service"));
        set_write_prefix(None);
        assert!(!write_unit_allowed("bluetooth.service"));
    }

    #[test]
    fn write_prefix_managed_only() {
        set_write_prefix(Some("managed-*".into()));
        assert!(write_unit_allowed("managed-mail-check.service"));
        assert!(!write_unit_allowed("other-x.service"));
        assert!(!write_unit_allowed("bluetooth.service"));
        set_write_prefix(None);
    }

    #[test]
    fn write_prefix_dual_families() {
        set_write_prefix(Some("managed-*,tmp-*".into()));
        assert!(write_unit_allowed("managed-mail-check.service"));
        assert!(write_unit_allowed("tmp-x.service"));
        assert!(!write_unit_allowed("bluetooth.service"));
        set_write_prefix(None);
    }

    #[test]
    fn write_prefix_parse_rejects_empty_glob() {
        assert!(parse_write_prefix("").is_err());
        assert!(parse_write_prefix("managed-*,").is_err());
        assert_eq!(
            parse_write_prefix(" managed-* , tmp-* ").unwrap(),
            vec!["managed-*".to_string(), "tmp-*".to_string()]
        );
    }

    #[test]
    fn manager_and_surface_parse() {
        assert_eq!(Manager::parse("user"), Some(Manager::User));
        assert_eq!(Manager::parse("system"), Some(Manager::System));
        assert_eq!(Manager::parse("pid1"), None);
        assert_eq!(Surface::parse("compact"), Some(Surface::Compact));
        assert_eq!(Surface::parse("full"), Some(Surface::Full));
        assert!(compact_tool("get_unit"));
        assert!(!compact_tool("unit_security"));
        assert!(!compact_tool("boot_times"));
    }
}
