//! The systemd backend.
//!
//! systemd's command-line tools expose stable, documented JSON output
//! (`systemctl --output=json`, `journalctl --output=json`). We use those as
//! the machine interface rather than linking libsystemd: it keeps the binary
//! dependency-free, version-tolerant, and every call auditable as a plain
//! process invocation.
//!
//! Every invocation here is read-only, with one exception: `apply_verb`,
//! the single mutating call in the program, reachable only through the
//! plan/apply path in `crate::write`. Argument lists are built from
//! vetted values only.

use std::fmt;
use std::process::Command;

use serde_json::{json, Value};

/// Capability scopes. A tool is only advertised and callable if its scope
/// was granted on the command line: authority handed to an agent is
/// explicit, minimal, and checked on every call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    UnitsRead,
    JournalRead,
    BootRead,
    UnitsWrite,
}

impl Scope {
    /// Every scope, in display order. `parse` and error text derive from
    /// this; a new scope is one variant, one `name` arm, one entry here.
    const ALL: &'static [Scope] = &[
        Scope::UnitsRead,
        Scope::JournalRead,
        Scope::BootRead,
        Scope::UnitsWrite,
    ];

    /// The wire name. The one place a scope spells itself; the compiler
    /// forces a new variant to add its arm, and everything else derives.
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

/// The set of granted scopes.
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

    /// Adds another set, without duplicating. Repeated `--grant` flags
    /// union rather than the last one winning.
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

/// A failure while talking to systemd, reported as a tool-level error so
/// the client can retry or report it; the server keeps running.
#[derive(Debug)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Validates a unit name before it reaches an argument list.
///
/// systemd unit names use a known, small alphabet; anything else is
/// refused early. `Command` never passes through a shell, so this is not
/// an injection defense. It exists to return a precise error instead of
/// whatever systemctl prints for a malformed name.
///
/// A leading `-` is allowed: `-.mount` and `-.slice`, the root mount
/// and the root slice, exist on every host, and refusing them called
/// units systemd itself creates malformed. Nothing downstream can read
/// one as an option, because every name reaches argv after `--` or
/// inside a single `--flag=value` element.
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

/// Matches a name against a shell-style glob supporting `*` and `?`,
/// the wildcards systemctl's own unit patterns use. Bracket expressions
/// are not supported; a `[` matches itself.
///
/// Linear scan with one backtrack point: on a mismatch the match
/// restarts one character further into the name, with the last `*`
/// absorbing the difference.
fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.as_bytes(), name.as_bytes());
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<usize> = None;
    let mut resume = 0;
    while ni < n.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            resume = ni;
            pi += 1;
        } else if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if let Some(s) = star {
            resume += 1;
            ni = resume;
            pi = s + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// The name-glob predicate shared by the list tools, matching against
/// the unit name each one holds in `key`. `None` matches every row. The
/// pattern is validated once, here, rather than at four call sites; it
/// never reaches an argument list, so only its length is checked.
fn name_filter<'a>(
    key: &'a str,
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
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|name| glob_match(p, name)),
    })
}

/// Spawns a command and returns its output whatever the exit status.
/// Every process this server spawns goes through here: one place to
/// audit, one place that pins the environment (no pager, no color, since we
/// are parsing this output, not reading it).
fn spawn(program: &str, args: &[&str]) -> Result<std::process::Output, BackendError> {
    Command::new(program)
        .args(args)
        // LC_ALL=C because two of these outputs are parsed as prose.
        // systemd formats timespans itself rather than through the
        // locale, so no translation was observed reaching the parsers,
        // but pinning the locale removes the question rather than
        // answering it per release.
        .env("LC_ALL", "C")
        .env("SYSTEMD_PAGER", "cat")
        .env("SYSTEMD_COLORS", "false")
        .output()
        .map_err(|e| BackendError(format!("failed to run {program}: {e}")))
}

fn exit_error(program: &str, output: &std::process::Output) -> BackendError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    BackendError(format!(
        "{program} exited with {}: {}",
        output.status,
        stderr.trim()
    ))
}

/// Runs a command and returns its output, or a `BackendError` carrying
/// the exit status and stderr.
fn run_output(program: &str, args: &[&str]) -> Result<std::process::Output, BackendError> {
    let output = spawn(program, args)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(exit_error(program, &output))
    }
}

fn run(program: &str, args: &[&str]) -> Result<Vec<u8>, BackendError> {
    run_output(program, args).map(|output| output.stdout)
}

/// Runs a journal query, treating "no entries matched" as an empty
/// result rather than a failure.
///
/// journalctl exits 1 when a filter (`--grep`, a time window) selects
/// nothing, which is an ordinary answer to a search. That case is
/// distinguishable: exit 1 with both streams empty, where a real
/// failure writes a message to stderr.
fn run_journal_query(program: &str, args: &[&str]) -> Result<Vec<u8>, BackendError> {
    let output = spawn(program, args)?;
    let no_matches =
        output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty();
    if output.status.success() || no_matches {
        Ok(output.stdout)
    } else {
        Err(exit_error(program, &output))
    }
}

/// Runs a command and parses its stdout as JSON.
fn run_json(program: &str, args: &[&str]) -> Result<Value, BackendError> {
    let stdout = run(program, args)?;
    serde_json::from_slice(&stdout)
        .map_err(|e| BackendError(format!("{program} produced invalid JSON: {e}")))
}

/// Runs `systemctl show`-style commands and collects the `Key=Value` lines
/// into an object, keyed for lookup (systemctl doesn't repeat keys).
fn run_key_values(
    program: &str,
    args: &[&str],
) -> Result<serde_json::Map<String, Value>, BackendError> {
    let stdout = run(program, args)?;
    let text = String::from_utf8_lossy(&stdout);
    Ok(text
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), Value::String(value.to_string())))
        })
        .collect())
}

/// Errors unless the manager knows the unit.
///
/// `systemctl show` does not fail for a name nobody has heard of: it
/// synthesizes the unit and prints a full property set with
/// `LoadState=not-found` and exit 0. Every caller that wanted "no such
/// unit" has to ask for that field and look at it. Masked units report
/// `masked` rather than `not-found`, so unmasking one still works.
pub(crate) fn ensure_unit_known(name: &str) -> Result<(), BackendError> {
    let props = run_key_values(
        "systemctl",
        &["show", "--no-pager", "--property=LoadState", "--", name],
    )?;
    match props.get("LoadState").and_then(Value::as_str) {
        Some("not-found") => Err(BackendError(format!("no such unit: {name}"))),
        _ => Ok(()),
    }
}

/// The active and sub state of one unit, for the plan/apply path.
pub(crate) fn unit_state(name: &str) -> Result<(String, String), BackendError> {
    let props = run_key_values(
        "systemctl",
        &[
            "show",
            "--no-pager",
            "--property=ActiveState,SubState",
            "--",
            name,
        ],
    )?;
    let get = |key: &str| {
        props
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string()
    };
    Ok((get("ActiveState"), get("SubState")))
}

/// `list_boots`: the boots recorded in the journal, oldest first;
/// `journalctl --list-boots --output=json` passed through.
pub fn list_boots() -> Result<Value, BackendError> {
    run_json(
        "journalctl",
        &["--list-boots", "--output=json", "--no-pager"],
    )
}

/// The current LogControl1 value for one verb ("service-log-level" or
/// "service-log-target"). Fails for services that do not implement the
/// interface; the error is systemctl's.
pub(crate) fn service_log_get(verb: &str, unit: &str) -> Result<String, BackendError> {
    let stdout = run("systemctl", &[verb, "--no-pager", "--", unit])?;
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}

/// `unit_log_control`: one service's runtime log level and target, read
/// through systemd's LogControl1 interface.
pub fn unit_log_control(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let level = service_log_get("service-log-level", name)?;
    let target = service_log_get("service-log-target", name)?;
    Ok(json!({ "unit": name, "log_level": level, "log_target": target }))
}

/// The enablement state of one unit's file, for the plan/apply path.
/// Empty for units without a unit file (for example transient units).
pub(crate) fn unit_file_state(name: &str) -> Result<String, BackendError> {
    let props = run_key_values(
        "systemctl",
        &["show", "--no-pager", "--property=UnitFileState", "--", name],
    )?;
    Ok(props
        .get("UnitFileState")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

/// Executes a unit state-change, enablement, or log-control verb. The
/// single mutating invocation in the program; the only caller is
/// `crate::write::apply`, which reaches here exclusively through an
/// applied plan. `value` carries the positional argument log-control
/// verbs take. Returns the non-empty output lines; systemctl reports
/// enablement symlink creations and removals on stderr.
pub(crate) fn apply_verb(
    verb: &str,
    unit: &str,
    value: Option<&str>,
) -> Result<Vec<String>, BackendError> {
    let mut args = vec![verb, "--no-pager", "--", unit];
    if let Some(value) = value {
        args.push(value);
    }
    let output = run_output("systemctl", &args)?;
    let lines = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let mut changes = lines(&output.stdout);
    changes.extend(lines(&output.stderr));
    Ok(changes)
}

/// Unit states accepted by the `list_units` filter. Also the schema enum
/// advertised in `tools/list`. One list, so the contract cannot drift from
/// the check.
pub const STATES: &[&str] = &["active", "inactive", "failed", "activating", "deactivating"];

/// `list_units`: all currently loaded units, optionally filtered by
/// state and by a glob on the unit name.
///
/// Prefers PID 1's native varlink socket (systemd ≥ 258) and falls back to
/// systemctl on any failure. Same JSON shape either way, so the caller
/// cannot tell which backend answered. Both filters are applied here,
/// once, after either backend, so their semantics cannot differ between
/// backends. Filtering matters: an ordinary host has several hundred
/// loaded units, and the unfiltered reply is the largest this server
/// produces.
pub fn list_units(state: Option<&str>, pattern: Option<&str>) -> Result<Value, BackendError> {
    if let Some(state) = state {
        if !STATES.contains(&state) {
            return Err(BackendError(format!(
                "unknown state filter '{state}' (known: {})",
                STATES.join(", ")
            )));
        }
    }
    let matches_name = name_filter("unit", pattern)?;

    let units = match crate::varlink::list_units() {
        Ok(units) => units,
        Err(_) => match run_json(
            "systemctl",
            &["list-units", "--all", "--output=json", "--no-pager"],
        )? {
            Value::Array(units) => units,
            _ => {
                return Err(BackendError(
                    "systemctl list-units did not produce a JSON array".into(),
                ))
            }
        },
    };

    Ok(Value::Array(
        units
            .into_iter()
            .filter(|u| state.is_none_or(|s| u["active"] == s) && matches_name(u))
            .map(|u| unit_row(&u))
            .collect(),
    ))
}

/// The row shape `list_units` emits, defined once for both backends.
///
/// systemctl adds a `job` key to units with a queued job, which the
/// varlink reply never carries; passing its JSON through unchanged
/// would let the row shape depend on which backend answered and on
/// whether a job happened to be in flight.
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

/// `failed_units`: shorthand for the question every session starts with.
pub fn failed_units() -> Result<Value, BackendError> {
    list_units(Some("failed"), None)
}

/// Runs a `systemctl list-*` subcommand and returns the rows whose
/// unit name matches the glob.
fn list_filtered(verb: &str, pattern: Option<&str>) -> Result<Vec<Value>, BackendError> {
    let matches_name = name_filter("unit", pattern)?;
    let rows = run_json("systemctl", &[verb, "--all", "--output=json", "--no-pager"])?;
    let Value::Array(rows) = rows else {
        return Err(BackendError(format!(
            "systemctl {verb} did not produce a JSON array"
        )));
    };
    Ok(rows.into_iter().filter(&matches_name).collect())
}

/// `list_timers`: timer units, what each activates, and when it next and
/// last elapsed.
///
/// The row is projected rather than passed through. systemctl's JSON
/// carries the timestamps as raw microseconds, and its `left` and
/// `passed` fields are filled in only on some paths (`left` repeats the
/// absolute `next` value, `passed` is zero for timers that have run).
/// What holds is `next` and `last`, converted to timestamps a reader can
/// act on; the two derived fields are dropped rather than reported
/// wrong.
pub fn list_timers(pattern: Option<&str>) -> Result<Value, BackendError> {
    let timers = list_filtered("list-timers", pattern)?;
    Ok(Value::Array(timers.iter().map(timer_row).collect()))
}

fn timer_row(timer: &Value) -> Value {
    // 0 means "never", and USEC_INFINITY means "not scheduled"; both
    // report as null rather than as a date in 1970 or in the year
    // 586524.
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

/// `list_sockets`: socket units, what they listen on and what they
/// activate; `systemctl list-sockets --output=json` passed through.
pub fn list_sockets(pattern: Option<&str>) -> Result<Value, BackendError> {
    Ok(Value::Array(list_filtered("list-sockets", pattern)?))
}

/// `list_unit_files`: installed unit files and their enablement state,
/// the on-disk view, where `list_units` shows the loaded one.
///
/// The optional state filter is a plain equality match applied here, not
/// an allowlist: unit-file states are version-dependent (enabled, static,
/// masked, generated, transient, ...) and the value never reaches argv,
/// so there is nothing to vet it against; an unknown state just matches
/// nothing. The name glob matches the unit file name.
pub fn list_unit_files(state: Option<&str>, pattern: Option<&str>) -> Result<Value, BackendError> {
    let matches_name = name_filter("unit_file", pattern)?;
    let files = run_json(
        "systemctl",
        &["list-unit-files", "--output=json", "--no-pager"],
    )?;
    let Value::Array(files) = files else {
        return Err(BackendError(
            "systemctl list-unit-files did not produce a JSON array".into(),
        ));
    };
    Ok(Value::Array(
        files
            .into_iter()
            .filter(|f| state.is_none_or(|s| f["state"] == s) && matches_name(f))
            .collect(),
    ))
}

/// The dependency properties `unit_dependencies` reports, forward and
/// reverse. One list: the argv and the reply are built from the same rows.
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

/// `unit_dependencies`: one unit's dependency edges by relation.
///
/// `systemctl list-dependencies` draws a tree with no reliable JSON form;
/// the same facts live in unit properties as space-separated lists (unit
/// names cannot contain spaces), which is the structured source. Every
/// relation is present in the reply, empty ones as empty arrays.
pub fn unit_dependencies(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    // Without this, a typo reads back as the authoritative claim that
    // the unit has no dependencies at all.
    ensure_unit_known(name)?;
    let prop_arg = format!("--property={}", DEPENDENCY_PROPS.join(","));
    let props = run_key_values("systemctl", &["show", "--no-pager", &prop_arg, "--", name])?;
    let deps: serde_json::Map<String, Value> = DEPENDENCY_PROPS
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

/// `unit_security`: systemd-analyze's sandboxing exposure analysis of one
/// unit, the same hardening scoring this project's own service file is
/// judged by. `--json=short` is a stable machine format; passed through.
pub fn unit_security(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let analysis = run_json(
        "systemd-analyze",
        &["security", "--json=short", "--no-pager", "--", name],
    )?;
    Ok(json!({ "unit": name, "analysis": analysis }))
}

/// `unit_properties`: the property set of one unit, all of it or the
/// named subset.
///
/// `systemctl show` emits `Key=Value` lines rather than JSON; we convert them
/// into an object so the model gets structure, not a wall of text. The
/// full set runs to some 200 properties and 10 KB for one service, which
/// is why a caller that wants three of them can say so.
///
/// The subset is selected here rather than with `systemctl
/// --property=`: systemctl prints nothing for a unit that does not exist
/// and nothing for a property that does not exist, so letting it select
/// would collapse those two cases into one empty reply.
pub fn unit_properties(name: &str, select: &[String]) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let props = run_key_values("systemctl", &["show", "--no-pager", "--", name])?;

    // A name nobody knows comes back as a synthesized property set
    // rather than as an error or an empty one, so the field is what
    // decides, not the count.
    if props.get("LoadState").and_then(Value::as_str) == Some("not-found") {
        return Err(BackendError(format!("no such unit: {name}")));
    }
    if select.is_empty() {
        return Ok(json!({ "unit": name, "properties": props }));
    }

    let selected: serde_json::Map<String, Value> = select
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

/// Filters accepted by `unit_logs` beyond the unit itself.
#[derive(Default)]
pub struct LogFilter<'a> {
    pub lines: u64,
    pub priority: Option<u64>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub boot: Option<i64>,
    pub grep: Option<&'a str>,
}

/// journalctl parses its own timestamp syntax ("2026-08-12 06:00:00",
/// "-5min", "yesterday") and rejects invalid input with a usable error;
/// this check only keeps the value shaped like a timestamp before it
/// becomes an argv element.
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
    // `--unit=`-style pairing keeps each flag and its value in one argv
    // element, leaving no option-parsing ambiguity. The name is validated
    // above and can never start with '-'.
    let unit_arg = format!("--unit={name}");
    let mut args = vec![
        "--output=json".to_string(),
        // Ask for the four fields the reply keeps. journald sends
        // dozens per entry (_CMDLINE, _EXE, _SELINUX_CONTEXT and the
        // rest), and measured on a real journal, 64% of what came back
        // was parsed and thrown away. The cursor and the timestamps
        // arrive regardless; they are not optional.
        "--output-fields=MESSAGE,PRIORITY,_PID".to_string(),
        "--no-pager".to_string(),
        "-n".to_string(),
        n,
        unit_arg,
    ];
    if let Some(p) = filter.priority {
        if p > 7 {
            return Err(BackendError(format!(
                "priority {p} out of range (0=emerg .. 7=debug)"
            )));
        }
        args.push(format!("--priority={p}"));
    }
    if let Some(since) = filter.since {
        validate_time_spec(since)?;
        args.push(format!("--since={since}"));
    }
    if let Some(until) = filter.until {
        validate_time_spec(until)?;
        args.push(format!("--until={until}"));
    }
    if let Some(boot) = filter.boot {
        if !(-1000..=1000).contains(&boot) {
            return Err(BackendError(format!("boot offset {boot} out of range")));
        }
        args.push(format!("--boot={boot}"));
    }
    if let Some(grep) = filter.grep {
        if grep.is_empty() || grep.len() > 256 {
            return Err(BackendError(
                "grep pattern must be 1..256 characters".into(),
            ));
        }
        args.push(format!("--grep={grep}"));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let stdout = run_journal_query("journalctl", &arg_refs)?;

    let text = String::from_utf8_lossy(&stdout);
    let entries: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|entry| {
            // journald hands us strings for everything; give the model real
            // types where the field has one. `MESSAGE` stays unchanged, since it
            // can legitimately be a byte array for non-UTF-8 payloads.
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

    // journalctl answers for any name, so a typo reads as "this unit is
    // quiet", which is the wrong conclusion during an incident. It
    // cannot be an error, though: the journal outlives the unit, and
    // reading the logs of a transient unit that has already been
    // collected, or of one removed since, or of a previous boot, is
    // the point of the tool. So say it, only when there is nothing to
    // show and the manager has never heard of the name.
    let mut reply = json!({ "unit": name, "entries": entries });
    if reply["entries"].as_array().is_some_and(Vec::is_empty) && ensure_unit_known(name).is_err() {
        reply["note"] = json!(
            "no entries, and no unit by this name is currently loaded: \
             check the name, or the boot offset if it is from an earlier boot"
        );
    }
    Ok(reply)
}

/// Formats a realtime microsecond timestamp as RFC 3339 UTC.
///
/// Implemented locally: one output format in one direction does not
/// justify a chrono dependency. The days-to-civil conversion is the
/// standard Gregorian-cycle algorithm (146097 days per 400 years).
fn usec_to_rfc3339(usec: u64) -> String {
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

/// `boot_times`: how long the last boot took, split by phase.
///
/// The same manager timestamps `systemd-analyze time` reads, preferably
/// over varlink (`io.systemd.Manager.Describe`, systemd >= 258), else via
/// the stable `Key=Value` output of `systemctl show`. Structured data at
/// the source either way, never systemd-analyze's prose summary.
pub fn boot_times() -> Result<Value, BackendError> {
    let [firmware, loader, initrd, userspace, finish] =
        crate::varlink::boot_timestamps().or_else(|_| cli_boot_timestamps())?;
    compute_boot_times(firmware, loader, initrd, userspace, finish, in_container())
}

/// Whether this manager runs a container, which systemd-analyze checks
/// before reporting any pre-userspace phase.
///
/// It matters more than it looks. In a container the userspace
/// timestamp is the host's monotonic clock at container start, not a
/// duration this system spent booting, so treating it as the kernel
/// phase reports a boot of hours for a startup of seconds.
fn in_container() -> bool {
    spawn("systemd-detect-virt", &["--container", "--quiet"])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn cli_boot_timestamps() -> Result<[u64; 5], BackendError> {
    // Spelled once: the same array asks systemctl for the properties and
    // reads them back, so a typo can't silently read as "phase absent".
    const PROPS: [&str; 5] = [
        "FirmwareTimestampMonotonic",
        "LoaderTimestampMonotonic",
        "InitRDTimestampMonotonic",
        "UserspaceTimestampMonotonic",
        "FinishTimestampMonotonic",
    ];
    let prop_arg = format!("--property={}", PROPS.join(","));
    let props = run_key_values("systemctl", &["show", "--no-pager", &prop_arg])?;
    Ok(PROPS.map(|key| {
        props
            .get(key)
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    }))
}

/// The phase arithmetic, mirroring systemd-analyze: `firmware` and `loader`
/// are durations spent before the kernel started; `initrd`, `userspace` and
/// `finish` are monotonic timestamps counted from kernel start. Zero means
/// "not applicable on this boot" (no EFI, no initrd), except for `finish`,
/// where zero means startup has not completed yet.
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

    let mut phases = serde_json::Map::new();
    let mut insert = |key: &str, usec: u64| {
        phases.insert(key.to_string(), json!(usec));
    };

    // A container did not boot: no firmware, no loader, no kernel, and
    // the timestamps for them belong to the host. Report the one phase
    // that happened here, which is what systemd-analyze does.
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

/// `critical_chain`: the chain of units that gated reaching the default
/// target (or a given unit) during boot.
///
/// This is the one backend with no machine-readable interface: neither
/// D-Bus nor `--output=json` expose the dependency-chain analysis, only
/// `systemd-analyze critical-chain` prose. It is parsed by a pure
/// function with tests over captured output, to be removed when systemd
/// provides a structured equivalent.
pub fn critical_chain(unit: Option<&str>) -> Result<Value, BackendError> {
    let mut args = vec!["critical-chain", "--no-pager"];
    if let Some(unit) = unit {
        validate_unit_name(unit)?;
        args.push("--");
        args.push(unit);
    }
    let stdout = run("systemd-analyze", &args)?;
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

/// Strips the tree drawing from the front of a critical-chain line,
/// leaving the unit name.
///
/// systemd-analyze draws the tree with box-drawing characters under a
/// UTF-8 locale and with ``` `- ``` and `|-` under LC_ALL=C, which
/// `spawn` pins. Both forms have to be handled, and the connectors are
/// matched whole rather than as a character set: a unit name can begin
/// with `-`, so stripping `-` as a set member would eat the first
/// character of `-.mount`.
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

/// Parses critical-chain output into `{unit, depth, activated, duration}`
/// entries. `activated` is the `@` time (when the unit became active,
/// relative to boot start), `duration` the `+` time (how long it took to
/// start); both stay verbatim strings ("1min 30.5s"). The tree structure
/// is parsed; the timespans are not reinterpreted.
fn parse_critical_chain(text: &str) -> Vec<Value> {
    let mut chain = Vec::new();
    for line in text.lines() {
        let rest = strip_tree_prefix(line);
        let depth = (line.chars().count() - rest.chars().count()) / 2;
        let mut tokens = rest.split_whitespace();
        let Some(name) = tokens.next() else { continue };
        // The header prose ("The time when unit became active...") has no
        // unit-name shape; a dot is what separates ssh.service from a word.
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
                // Continuation of a multi-token timespan ("@1min 30.5s").
                // `+` only ever follows `@` on a line, so "duration if set,
                // else activated" is exactly the field written last.
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

/// `boot_blame`: units ordered by how long their own startup took,
/// `systemd-analyze blame`.
///
/// Like `critical_chain`, this has no machine-readable source anywhere in
/// systemd, so its prose is parsed by a pure, tested function, to be
/// removed when systemd provides a structured equivalent. Timespans stay
/// verbatim strings; the formatting is not reinterpreted.
///
/// The list is already ordered slowest first, so `limit` answers the
/// question this tool is asked without carrying the couple of hundred
/// units nobody is waiting on. The reply reports the total, so a
/// truncated answer says so.
pub fn boot_blame(limit: usize) -> Result<Value, BackendError> {
    let stdout = run("systemd-analyze", &["blame", "--no-pager"])?;
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

/// Each blame line is a timespan and a unit name. The timespan can be
/// several tokens ("1min 30.5s"); the unit name is always the last token
/// and cannot contain spaces, so parse from the right. A timespan starts
/// with a digit and a unit name carries a type suffix. Anything else is
/// prose and parses to nothing rather than into fake entries.
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
        // The root mount and root slice start with '-' and are real.
        // Every call site places a name after `--` or inside
        // `--flag=value`, so nothing reads one as an option.
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
        // A trailing star matches the empty rest, a leading one the
        // empty start.
        assert!(glob_match("ssh.service*", "ssh.service"));
        assert!(glob_match("*ssh.service", "ssh.service"));
        // Backtracking: the first candidate for '*.service' inside the
        // name is not the one that matches.
        assert!(glob_match("*.service", "a.service.d.service"));
        assert!(!glob_match("nginx*", "not-nginx.service"));
        assert!(!glob_match("*.timer", "logrotate.timer.d"));
        assert!(!glob_match("user@????.service", "user@10.service"));
        assert!(!glob_match("ssh.service", "ssh.service.d"));
        assert!(!glob_match("", "ssh.service"));
        // Bracket expressions are not implemented: '[' is literal.
        assert!(glob_match("a[bc.service", "a[bc.service"));
        assert!(!glob_match("a[bc].service", "ab.service"));
    }

    /// The obviously correct matcher: try every split, exponential in
    /// the worst case, unusable in production and ideal as an oracle.
    /// `glob_match` is the fast version with one backtrack point, which
    /// is where a subtle wrong answer would live.
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
        // Exhaustive over short strings rather than sampled: every
        // pattern of up to 4 characters from "ab*?" against every name
        // of up to 4 from "ab". That is where backtracking bugs live,
        // and it runs in milliseconds. Deterministic, so a failure
        // reproduces.
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
        let f = name_filter("unit", Some("*.timer")).unwrap();
        assert_eq!(rows.iter().filter(|r| f(r)).count(), 1);
        // No pattern keeps every row, including one missing the key.
        let f = name_filter("unit", None).unwrap();
        assert!(f(&json!({})));
        assert!(name_filter("unit", Some("")).is_err());
        assert!(name_filter("unit", Some(&"*".repeat(300))).is_err());
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
        // Never run reports null, not the epoch...
        assert_eq!(row["last"], json!(null));
        // ...and the fields systemctl fills in inconsistently are gone.
        assert!(row.get("left").is_none() && row.get("passed").is_none());
        // Not scheduled (USEC_INFINITY) is null too.
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
        // Granting the write scope grants no read scope.
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
        // Leap-year day, with microseconds.
        assert_eq!(
            usec_to_rfc3339(1_582_934_400_123_456),
            "2020-02-29T00:00:00.123456Z"
        );
    }

    #[test]
    fn boot_time_phases() {
        // EFI boot with initrd: every phase present, arithmetic per
        // systemd-analyze (firmware/loader are pre-kernel durations,
        // the rest are timestamps since kernel start).
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

        // BIOS boot, no initrd: absent phases are absent, not zero.
        let v = compute_boot_times(0, 0, 0, 4_000_000, 10_000_000, false).unwrap();
        assert!(v.get("firmware_usec").is_none());
        assert!(v.get("loader_usec").is_none());
        assert!(v.get("initrd_usec").is_none());
        assert_eq!(v["kernel_usec"], json!(4_000_000));
        assert_eq!(v["total_usec"], json!(10_000_000));

        // Still booting: an error the model can act on, not garbage math.
        assert!(compute_boot_times(0, 0, 0, 4_000_000, 0, false).is_err());

        // In a container the userspace timestamp is the host's uptime,
        // so reporting it as a kernel phase claimed a 7.5 hour boot for
        // a 34 second startup. Only the phase that happened is shown.
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
        // Header prose skipped, all five chain lines kept.
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
        assert_eq!(chain[3]["depth"], json!(3));
        assert_eq!(
            chain[4]["unit"],
            json!("NetworkManager-wait-online.service")
        );
        assert_eq!(chain[4]["duration"], json!("45.8s"));
    }

    #[test]
    fn critical_chain_parses_the_ascii_tree() {
        // LC_ALL=C, which `spawn` pins, makes systemd-analyze draw the
        // tree with `- and |- instead of box characters. Parsing only
        // the box form silently reduced every chain to its root.
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
        // A unit name may itself begin with '-', so the connectors are
        // stripped whole rather than as a set of characters.
        assert_eq!(chain[4]["unit"], json!("-.mount"));
    }

    #[test]
    fn critical_chain_rejects_non_units() {
        // Lines whose first token doesn't have unit-name shape are dropped,
        // not mangled into fake entries.
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
        // Prose (no trailing unit-name shape) parses to nothing.
        assert!(parse_blame("Bootup is not yet finished. Please try again later.\n").is_empty());
    }
}
