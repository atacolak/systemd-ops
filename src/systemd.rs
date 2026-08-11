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
// The shared `Read` suffix is the point, not an accident: scopes are
// `noun:verb`, and a future write path will add `*Write` variants beside
// these. Renaming to appease the lint would erase the taxonomy.
#[allow(clippy::enum_variant_names)]
pub enum Scope {
    UnitsRead,
    JournalRead,
    BootRead,
}

impl Scope {
    /// Every scope, in display order. `parse` and error text derive from
    /// this; a new scope is one variant, one `name` arm, one entry here.
    const ALL: &'static [Scope] = &[Scope::UnitsRead, Scope::JournalRead, Scope::BootRead];

    /// The wire name. The one place a scope spells itself — the compiler
    /// forces a new variant to add its arm, and everything else derives.
    fn name(self) -> &'static str {
        match self {
            Scope::UnitsRead => "units:read",
            Scope::JournalRead => "journal:read",
            Scope::BootRead => "boot:read",
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

/// Runs a command and returns its stdout, or a `BackendError` carrying the
/// exit status and stderr. Every process this server spawns goes through
/// here: one place to audit, one place that pins the environment (no pager,
/// no color — we are parsing this output, not reading it).
fn run(program: &str, args: &[&str]) -> Result<Vec<u8>, BackendError> {
    let output = Command::new(program)
        .args(args)
        .env("SYSTEMD_PAGER", "cat")
        .env("SYSTEMD_COLORS", "false")
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
    Ok(output.stdout)
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

/// Unit states accepted by the `list_units` filter. Also the schema enum
/// advertised in `tools/list` — one list, so the contract can't drift from
/// the check.
pub const STATES: &[&str] = &["active", "inactive", "failed", "activating", "deactivating"];

/// `list_units`: all currently loaded units, optionally filtered by state.
///
/// Prefers PID 1's native varlink socket (systemd ≥ 258) and falls back to
/// systemctl on any surprise — same JSON shape either way, so the caller
/// never learns which backend answered. That's the contract, which is also
/// why the state filter is applied here, once, after either backend: filter
/// semantics defined per backend is how the backends drift apart.
pub fn list_units(state: Option<&str>) -> Result<Value, BackendError> {
    if let Some(state) = state {
        if !STATES.contains(&state) {
            return Err(BackendError(format!(
                "unknown state filter '{state}' (known: {})",
                STATES.join(", ")
            )));
        }
    }

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
            .filter(|u| state.is_none_or(|s| u["active"] == s))
            .collect(),
    ))
}

/// `failed_units`: shorthand for the question every session starts with.
pub fn failed_units() -> Result<Value, BackendError> {
    list_units(Some("failed"))
}

/// `list_timers`: timer units with their schedules, next elapse, and what
/// they activate — `systemctl list-timers --output=json` passed through.
pub fn list_timers() -> Result<Value, BackendError> {
    run_json(
        "systemctl",
        &["list-timers", "--all", "--output=json", "--no-pager"],
    )
}

/// `list_sockets`: socket units, what they listen on and what they
/// activate — `systemctl list-sockets --output=json` passed through.
pub fn list_sockets() -> Result<Value, BackendError> {
    run_json(
        "systemctl",
        &["list-sockets", "--all", "--output=json", "--no-pager"],
    )
}

/// `list_unit_files`: installed unit files and their enablement state —
/// the on-disk view, where `list_units` shows the loaded one.
///
/// The optional state filter is a plain equality match applied here, not
/// an allowlist: unit-file states are version-dependent (enabled, static,
/// masked, generated, transient, ...) and the value never reaches argv,
/// so there is nothing to vet it against — an unknown state just matches
/// nothing.
pub fn list_unit_files(state: Option<&str>) -> Result<Value, BackendError> {
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
            .filter(|f| state.is_none_or(|s| f["state"] == s))
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
/// unit — the same hardening scoring this project's own service file is
/// judged by. `--json=short` is a stable machine format; passed through.
pub fn unit_security(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let analysis = run_json(
        "systemd-analyze",
        &["security", "--json=short", "--no-pager", "--", name],
    )?;
    Ok(json!({ "unit": name, "analysis": analysis }))
}

/// `unit_properties`: the full property set of one unit.
///
/// `systemctl show` emits `Key=Value` lines rather than JSON; we convert them
/// into an object so the model gets structure, not a wall of text.
pub fn unit_properties(name: &str) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let props = run_key_values("systemctl", &["show", "--no-pager", "--", name])?;

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
pub fn unit_logs(name: &str, lines: u64, priority: Option<u64>) -> Result<Value, BackendError> {
    validate_unit_name(name)?;
    let lines = lines.clamp(1, 1000);
    let n = lines.to_string();
    // `--unit=`/`--priority=` keep flag and value paired in one argv
    // element; a bare `-u` followed by other flags is how option parsing
    // accidents happen. The name is validated above and can never start
    // with '-'.
    let unit_arg = format!("--unit={name}");
    let mut args = vec!["--output=json", "--no-pager", "-n", &n, &unit_arg];
    let priority_arg;
    if let Some(p) = priority {
        if p > 7 {
            return Err(BackendError(format!(
                "priority {p} out of range (0=emerg .. 7=debug)"
            )));
        }
        priority_arg = format!("--priority={p}");
        args.push(&priority_arg);
    }
    let stdout = run("journalctl", &args)?;

    let text = String::from_utf8_lossy(&stdout);
    let entries: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|entry| {
            // journald hands us strings for everything; give the model real
            // types where the field has one. `MESSAGE` stays verbatim — it
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

    Ok(json!({ "unit": name, "entries": entries }))
}

/// Formats a realtime microsecond timestamp as RFC 3339 UTC.
///
/// Twenty lines of calendar arithmetic beat a chrono dependency for one
/// format in one direction. The days-to-civil conversion is the standard
/// Gregorian-cycle algorithm (146097 days per 400 years).
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
/// over varlink (`io.systemd.Manager.Describe`, systemd ≥ 257), else via
/// the stable `Key=Value` output of `systemctl show` — structured data at
/// the source either way, never systemd-analyze's prose summary.
pub fn boot_times() -> Result<Value, BackendError> {
    let [firmware, loader, initrd, userspace, finish] =
        crate::varlink::boot_timestamps().or_else(|_| cli_boot_timestamps())?;
    compute_boot_times(firmware, loader, initrd, userspace, finish)
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
/// `systemd-analyze critical-chain` prose. So we parse it — in a pure
/// function, with tests over captured output, ready to be deleted the day
/// systemd grows a structured equivalent.
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

/// Parses critical-chain output into `{unit, depth, activated, duration}`
/// entries. `activated` is the `@` time (when the unit became active,
/// relative to boot start), `duration` the `+` time (how long it took to
/// start); both stay verbatim strings ("1min 30.5s") — we structure the
/// tree, we don't reinterpret systemd's timespans.
fn parse_critical_chain(text: &str) -> Vec<Value> {
    let mut chain = Vec::new();
    for line in text.lines() {
        // Strip the tree drawing; what's left starts with the unit name.
        let rest = line.trim_start_matches([' ', '└', '├', '│', '─']);
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

/// `boot_blame`: units ordered by how long their own startup took —
/// `systemd-analyze blame`.
///
/// Like `critical_chain`, this has no machine-readable source anywhere in
/// systemd, so its prose is parsed by a pure, tested function that gets
/// deleted the day systemd grows a structured equivalent. Timespans stay
/// verbatim strings; we don't reinterpret systemd's formatting.
pub fn boot_blame() -> Result<Value, BackendError> {
    let stdout = run("systemd-analyze", &["blame", "--no-pager"])?;
    let text = String::from_utf8_lossy(&stdout);
    let blame = parse_blame(&text);
    if blame.is_empty() {
        return Err(BackendError(format!(
            "could not parse systemd-analyze blame output: {}",
            text.trim()
        )));
    }
    Ok(json!({ "blame": blame }))
}

/// Each blame line is a timespan and a unit name. The timespan can be
/// several tokens ("1min 30.5s"); the unit name is always the last token
/// and cannot contain spaces, so parse from the right. A timespan starts
/// with a digit and a unit name carries a type suffix — anything else is
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
        assert!(validate_unit_name("-rf").is_err());
        assert!(validate_unit_name("a b").is_err());
        assert!(validate_unit_name("x;reboot").is_err());
        assert!(validate_unit_name(&"x".repeat(300)).is_err());
    }

    #[test]
    fn grants() {
        let g = Grants::from_args("units:read, journal:read,boot:read").unwrap();
        assert!(g.allows(Scope::UnitsRead));
        assert!(g.allows(Scope::JournalRead));
        assert!(g.allows(Scope::BootRead));
        assert!(Grants::from_args("units:write").is_err());
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
        let v = compute_boot_times(5_000_000, 2_000_000, 3_000_000, 4_000_000, 10_000_000).unwrap();
        assert_eq!(v["firmware_usec"], json!(3_000_000));
        assert_eq!(v["loader_usec"], json!(2_000_000));
        assert_eq!(v["kernel_usec"], json!(3_000_000));
        assert_eq!(v["initrd_usec"], json!(1_000_000));
        assert_eq!(v["userspace_usec"], json!(6_000_000));
        assert_eq!(v["total_usec"], json!(15_000_000));

        // BIOS boot, no initrd: absent phases are absent, not zero.
        let v = compute_boot_times(0, 0, 0, 4_000_000, 10_000_000).unwrap();
        assert!(v.get("firmware_usec").is_none());
        assert!(v.get("loader_usec").is_none());
        assert!(v.get("initrd_usec").is_none());
        assert_eq!(v["kernel_usec"], json!(4_000_000));
        assert_eq!(v["total_usec"], json!(10_000_000));

        // Still booting: an error the model can act on, not garbage math.
        assert!(compute_boot_times(0, 0, 0, 4_000_000, 0).is_err());
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
