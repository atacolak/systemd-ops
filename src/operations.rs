//! Semantic operations over write-prefix unit stems.
//!
//! An operation is one stem (`managed-mail-check`) plus its constituent
//! units (`managed-mail-check.service`, optional `.timer`). The read path
//! covers every unit file matching `--write-prefix`. The authoring path
//! writes only a small templated subset, and only when a managed marker
//! is present (or, for create, when the stem does not exist yet).
//!
//! `# managed: systemd-ops 1` grants definition ownership.
//! Title/purpose/tags/origin comments are informational.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::sha256::sha256_hex;
use crate::systemd::{self, BackendError};
use crate::write::{self, AuthoringVerb, AuthoringWork, FileSnapshot};

pub const MANAGED_MARKER: &str = "# managed: systemd-ops 1";

const SUFFIXES: &[&str] = &[
    ".service",
    ".timer",
    ".socket",
    ".path",
    ".target",
    ".slice",
    ".scope",
    ".mount",
    ".automount",
    ".swap",
    ".device",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Simple,
    Oneshot,
    OneshotLinger,
}

impl Kind {
    fn parse(s: &str) -> Result<Self, BackendError> {
        match s {
            "simple" => Ok(Kind::Simple),
            "oneshot" => Ok(Kind::Oneshot),
            "oneshot-linger" => Ok(Kind::OneshotLinger),
            other => Err(BackendError(format!(
                "unknown kind '{other}' (simple, oneshot, oneshot-linger)"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Kind::Simple => "simple",
            Kind::Oneshot => "oneshot",
            Kind::OneshotLinger => "oneshot-linger",
        }
    }

    fn service_type(self) -> &'static str {
        match self {
            Kind::Simple => "simple",
            Kind::Oneshot | Kind::OneshotLinger => "oneshot",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schedule {
    Interval {
        on_boot_sec: String,
        on_unit_active_sec: String,
        persistent: bool,
        accuracy_sec: Option<String>,
    },
    Calendar {
        on_calendar: String,
        persistent: bool,
        accuracy_sec: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedSpec {
    pub unit: String,
    pub kind: Kind,
    pub title: Option<String>,
    pub purpose: Option<String>,
    pub tags: Vec<String>,
    pub description: String,
    pub exec_path: String,
    pub exec_argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub path: Vec<String>,
    pub environment_files: Vec<String>,
    pub after: Vec<String>,
    pub wants_network_online: bool,
    pub restart: Option<String>,
    pub nice: Option<i32>,
    pub schedule: Option<Schedule>,
    pub enabled: bool,
    pub start_now: bool,
    pub origin_cwd: Option<String>,
    pub created_at: String,
}

impl NormalizedSpec {
    pub fn service_name(&self) -> String {
        format!("{}.service", self.unit)
    }

    pub fn timer_name(&self) -> Option<String> {
        self.schedule
            .as_ref()
            .map(|_| format!("{}.timer", self.unit))
    }
    pub fn to_json(&self) -> Value {
        let mut v = canonical_spec_json(self);
        if let Value::Object(map) = &mut v {
            map.insert("origin_cwd".into(), json!(self.origin_cwd));
            map.insert("created_at".into(), json!(self.created_at));
        }
        v
    }

    pub fn from_json(v: &Value) -> Result<Self, BackendError> {
        let origin = v
            .get("origin_cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
        let created = v
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_string);
        parse_spec(&json!({ "spec": v }), origin, created)
    }

    fn has_install(&self) -> bool {
        self.schedule.is_some() || matches!(self.kind, Kind::Simple | Kind::OneshotLinger)
    }

    fn enable_unit(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.timer_name().or_else(|| {
            if self.has_install() {
                Some(self.service_name())
            } else {
                None
            }
        })
    }

    fn start_unit(&self) -> Option<String> {
        if !self.start_now {
            return None;
        }
        self.timer_name().or(Some(self.service_name()))
    }
}
#[derive(Clone, Debug, Default)]
struct FileMeta {
    managed: bool,
    spec_sha: Option<String>,
    title: Option<String>,
    purpose: Option<String>,
    tags: Vec<String>,
    origin_cwd: Option<String>,
    created_at: Option<String>,
    service_type: Option<String>,
    remain_after_exit: bool,
    description: Option<String>,
    exec_start: Option<String>,
    working_directory: Option<String>,
    on_calendar: Option<String>,
    on_boot_sec: Option<String>,
    on_unit_active_sec: Option<String>,
    persistent: Option<bool>,
    accuracy_sec: Option<String>,
}

pub fn parse_context_cwd(args: &Value) -> Result<Option<String>, BackendError> {
    let Some(ctx) = args.get("context") else {
        return Ok(None);
    };
    if ctx.is_null() {
        return Ok(None);
    }
    let Some(obj) = ctx.as_object() else {
        return Err(BackendError("context must be an object".into()));
    };
    match obj.get("cwd") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let cwd = normalize_cwd(s)?;
            Ok(Some(cwd))
        }
        Some(_) => Err(BackendError("context.cwd must be a string".into())),
    }
}

fn normalize_cwd(s: &str) -> Result<String, BackendError> {
    if s.is_empty() || s.len() > 1024 {
        return Err(BackendError(
            "context.cwd must be 1..1024 characters".into(),
        ));
    }
    if !s.starts_with('/') {
        return Err(BackendError(format!(
            "context.cwd must be an absolute path; got '{s}'"
        )));
    }
    if s.contains('\n') || s.contains('\0') {
        return Err(BackendError("context.cwd must not contain newlines".into()));
    }
    Ok(s.trim_end_matches('/').to_string())
}

fn cross_context_warning(origin: Option<&str>, current: Option<&str>) -> Option<String> {
    match (origin, current) {
        (Some(o), Some(c)) if o != c => Some(format!(
            "cross-context: origin_cwd is {o}, current context.cwd is {c}"
        )),
        _ => None,
    }
}

fn stem_of(name: &str) -> String {
    for suffix in SUFFIXES {
        if let Some(stem) = name.strip_suffix(suffix) {
            return stem.to_string();
        }
    }
    name.to_string()
}

fn is_operation_stem(stem: &str) -> bool {
    let name = format!("{stem}.service");
    systemd::validate_unit_name(&name).is_ok() && systemd::write_unit_allowed(&name)
}

fn write_prefix_label() -> String {
    match systemd::write_prefix() {
        Some(globs) => globs.join(","),
        None => "no write-prefix".into(),
    }
}

fn validate_stem(stem: &str) -> Result<(), BackendError> {
    if SUFFIXES.iter().any(|s| stem.ends_with(s)) {
        return Err(BackendError(format!(
            "operation unit is the stem without a suffix; got '{stem}'"
        )));
    }
    if !is_operation_stem(stem) {
        return Err(BackendError(format!(
            "operation unit must match '{}'; refused '{stem}'",
            write_prefix_label()
        )));
    }
    systemd::require_write_unit(&format!("{stem}.service"))?;
    Ok(())
}

fn validate_time_word(label: &str, spec: &str) -> Result<(), BackendError> {
    if spec.is_empty() || spec.len() > 64 {
        return Err(BackendError(format!(
            "{label} must be 1..64 characters of systemd time/calendar syntax"
        )));
    }
    let ok = spec
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " :+-*/,._@".contains(c));
    if ok {
        Ok(())
    } else {
        Err(BackendError(format!(
            "'{spec}' is not valid systemd time/calendar syntax for {label}"
        )))
    }
}

fn validate_abs_path(label: &str, path: &str, must_exist: bool) -> Result<(), BackendError> {
    if path.is_empty() || path.len() > 512 {
        return Err(BackendError(format!("{label} must be 1..512 characters")));
    }
    if !path.starts_with('/') {
        return Err(BackendError(format!("{label} must be an absolute path")));
    }
    if path.contains('\n') || path.contains('\0') {
        return Err(BackendError(format!("{label} must not contain newlines")));
    }
    if must_exist {
        let meta = fs::metadata(path)
            .map_err(|e| BackendError(format!("{label} '{path}' is not reachable: {e}")))?;
        if !meta.is_file() {
            return Err(BackendError(format!("{label} '{path}' is not a file")));
        }
    }
    Ok(())
}

fn validate_ident(label: &str, s: &str) -> Result<(), BackendError> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .enumerate()
            .all(|(i, c)| c.is_ascii_alphanumeric() || c == '_' || (i > 0 && c == '-'));
    if ok {
        Ok(())
    } else {
        Err(BackendError(format!(
            "{label} '{s}' is not a valid identifier"
        )))
    }
}

fn quote_word(s: &str) -> String {
    let special = s.is_empty()
        || s.bytes().any(|b| {
            b <= b' '
                || matches!(
                    b,
                    b'"' | b'\''
                        | b'\\'
                        | b'$'
                        | b'`'
                        | b'|'
                        | b'&'
                        | b';'
                        | b'('
                        | b')'
                        | b'<'
                        | b'>'
                        | b'{'
                        | b'}'
                        | b'['
                        | b']'
                        | b'*'
                        | b'?'
                        | b'#'
                        | b'~'
                        | b'='
                )
        });
    if special {
        let escaped = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

fn exec_line(path: &str, argv: &[String]) -> String {
    let mut words = Vec::with_capacity(1 + argv.len());
    words.push(quote_word(path));
    words.extend(argv.iter().map(|a| quote_word(a)));
    words.join(" ")
}

fn now_rfc3339() -> String {
    let usec = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    systemd::usec_to_rfc3339(usec)
}

fn text_field(label: &str, v: Option<&str>, max: usize) -> Result<Option<String>, BackendError> {
    let Some(s) = v.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if s.len() > max || s.contains('\n') || s.contains('\0') {
        return Err(BackendError(format!(
            "{label} must be 1..{max} characters with no newlines"
        )));
    }
    Ok(Some(s.to_string()))
}

fn parse_tags(v: Option<&Value>) -> Result<Vec<String>, BackendError> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| BackendError("tags must be an array of strings".into()))?;
    if arr.len() > 32 {
        return Err(BackendError("tags is limited to 32 entries".into()));
    }
    let mut tags = Vec::new();
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| BackendError("tags must be strings".into()))?
            .trim();
        if s.is_empty() || s.len() > 64 || s.contains(',') || s.contains('\n') {
            return Err(BackendError(
                "each tag must be 1..64 characters without commas or newlines".into(),
            ));
        }
        if !tags.iter().any(|t| t == s) {
            tags.push(s.to_string());
        }
    }
    Ok(tags)
}

fn parse_string_list(label: &str, v: Option<&Value>) -> Result<Vec<String>, BackendError> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| BackendError(format!("{label} must be an array of strings")))?;
    if arr.len() > 32 {
        return Err(BackendError(format!("{label} is limited to 32 entries")));
    }
    let mut out = Vec::new();
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| BackendError(format!("{label} must be strings")))?;
        if s.is_empty() || s.len() > 256 {
            return Err(BackendError(format!(
                "each {label} entry must be 1..256 characters"
            )));
        }
        out.push(s.to_string());
    }
    Ok(out)
}

fn parse_schedule(v: Option<&Value>, kind: Kind) -> Result<Option<Schedule>, BackendError> {
    let Some(v) = v else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    if kind == Kind::Simple {
        return Err(BackendError(
            "simple operations cannot have a schedule; use oneshot for a timer pair".into(),
        ));
    }
    if kind == Kind::OneshotLinger {
        return Err(BackendError(
            "oneshot-linger operations cannot have a schedule".into(),
        ));
    }
    let obj = v
        .as_object()
        .ok_or_else(|| BackendError("schedule must be an object".into()))?;
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("schedule.type is required (interval, calendar)".into()))?;
    let persistent = obj
        .get("persistent")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let accuracy_sec = match obj.get("accuracy_sec").and_then(Value::as_str) {
        Some(s) => {
            validate_time_word("schedule.accuracy_sec", s)?;
            Some(s.to_string())
        }
        None => None,
    };
    match ty {
        "interval" => {
            let on_boot_sec = obj
                .get("on_boot_sec")
                .and_then(Value::as_str)
                .ok_or_else(|| BackendError("schedule.on_boot_sec is required".into()))?;
            let on_unit_active_sec = obj
                .get("on_unit_active_sec")
                .and_then(Value::as_str)
                .ok_or_else(|| BackendError("schedule.on_unit_active_sec is required".into()))?;
            validate_time_word("schedule.on_boot_sec", on_boot_sec)?;
            validate_time_word("schedule.on_unit_active_sec", on_unit_active_sec)?;
            Ok(Some(Schedule::Interval {
                on_boot_sec: on_boot_sec.to_string(),
                on_unit_active_sec: on_unit_active_sec.to_string(),
                persistent,
                accuracy_sec,
            }))
        }
        "calendar" => {
            let on_calendar = obj
                .get("on_calendar")
                .and_then(Value::as_str)
                .ok_or_else(|| BackendError("schedule.on_calendar is required".into()))?;
            validate_time_word("schedule.on_calendar", on_calendar)?;
            Ok(Some(Schedule::Calendar {
                on_calendar: on_calendar.to_string(),
                persistent,
                accuracy_sec,
            }))
        }
        other => Err(BackendError(format!(
            "unknown schedule.type '{other}' (interval, calendar)"
        ))),
    }
}

fn spec_object<'a>(args: &'a Value) -> Result<&'a Value, BackendError> {
    args.get("spec")
        .ok_or_else(|| BackendError("missing required argument: spec".into()))
}

pub fn parse_spec(
    args: &Value,
    origin_cwd: Option<String>,
    created_at: Option<String>,
) -> Result<NormalizedSpec, BackendError> {
    let spec = spec_object(args)?;
    let obj = spec
        .as_object()
        .ok_or_else(|| BackendError("spec must be an object".into()))?;
    let unit = obj
        .get("unit")
        .and_then(Value::as_str)
        .or_else(|| args.get("unit").and_then(Value::as_str))
        .ok_or_else(|| BackendError("spec.unit is required".into()))?;
    validate_stem(unit)?;

    let kind = Kind::parse(
        obj.get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError("spec.kind is required".into()))?,
    )?;

    let exec = obj
        .get("exec")
        .and_then(Value::as_object)
        .ok_or_else(|| BackendError("spec.exec is required".into()))?;
    let exec_path = exec
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("spec.exec.path is required".into()))?;
    validate_abs_path("spec.exec.path", exec_path, true)?;
    let exec_argv = parse_string_list("spec.exec.argv", exec.get("argv"))?;
    for arg in &exec_argv {
        if arg.contains('\n')
            || arg.contains('\0')
            || arg.contains('$')
            || arg.contains('|')
            || arg.contains('>')
            || arg.contains('<')
            || arg.contains(';')
            || arg.contains('&')
            || arg.contains('`')
        {
            return Err(BackendError(
                "spec.exec.argv entries cannot contain shell metacharacters".into(),
            ));
        }
    }

    let cwd = match obj.get("cwd").and_then(Value::as_str) {
        Some(s) => {
            let n = normalize_cwd(s)?;
            if !Path::new(&n).is_dir() {
                return Err(BackendError(format!("spec.cwd '{n}' is not a directory")));
            }
            Some(n)
        }
        None => None,
    };

    let mut env = BTreeMap::new();
    if let Some(map) = obj.get("env").and_then(Value::as_object) {
        if map.len() > 32 {
            return Err(BackendError("spec.env is limited to 32 entries".into()));
        }
        for (k, v) in map {
            validate_ident("spec.env key", k)?;
            let val = v
                .as_str()
                .ok_or_else(|| BackendError(format!("spec.env.{k} must be a string")))?;
            if val.len() > 1024 || val.contains('\n') || val.contains('\0') {
                return Err(BackendError(format!(
                    "spec.env.{k} must be 0..1024 characters with no newlines"
                )));
            }
            env.insert(k.clone(), val.to_string());
        }
    }

    let path = parse_string_list("spec.path", obj.get("path"))?;
    for p in &path {
        if !p.starts_with('/') {
            return Err(BackendError(format!(
                "spec.path entries must be absolute; got '{p}'"
            )));
        }
    }

    let environment_files =
        parse_string_list("spec.environment_files", obj.get("environment_files"))?;
    for f in &environment_files {
        validate_abs_path("spec.environment_files", f, true)?;
    }

    let after = parse_string_list("spec.after", obj.get("after"))?;
    for dep in &after {
        systemd::validate_unit_name(dep)?;
        let allowed = dep == "network-online.target"
            || dep == "default.target"
            || dep == "timers.target"
            || systemd::write_unit_allowed(dep);
        if !allowed {
            return Err(BackendError(format!(
                "spec.after may name network-online.target, default.target, timers.target, or units matching '{}'; refused '{dep}'",
                write_prefix_label()
            )));
        }
    }

    let wants_network_online = obj
        .get("wants_network_online")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let restart = match obj.get("restart").and_then(Value::as_str) {
        Some("no") | None if kind != Kind::Simple => None,
        Some(s @ ("no" | "on-failure" | "always")) if kind == Kind::Simple => Some(s.to_string()),
        Some(s) if kind != Kind::Simple => {
            return Err(BackendError(format!(
                "spec.restart is only valid on simple operations; got '{s}'"
            )));
        }
        Some(s) => {
            return Err(BackendError(format!(
                "unknown spec.restart '{s}' (no, on-failure, always)"
            )));
        }
        None => None,
    };

    let nice = match obj.get("nice") {
        Some(Value::Number(n)) => {
            let n = n
                .as_i64()
                .ok_or_else(|| BackendError("spec.nice must be an integer".into()))?;
            if !(-20..=19).contains(&n) {
                return Err(BackendError("spec.nice must be -20..19".into()));
            }
            Some(n as i32)
        }
        None | Some(Value::Null) => None,
        Some(_) => return Err(BackendError("spec.nice must be an integer".into())),
    };

    let schedule = parse_schedule(obj.get("schedule"), kind)?;
    let enabled = obj.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let start_now = obj
        .get("start_now")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let title = text_field("spec.title", obj.get("title").and_then(Value::as_str), 200)?;
    let purpose = text_field(
        "spec.purpose",
        obj.get("purpose").and_then(Value::as_str),
        500,
    )?;
    let tags = parse_tags(obj.get("tags"))?;
    let description = text_field(
        "spec.description",
        obj.get("description").and_then(Value::as_str),
        200,
    )?
    .or_else(|| title.clone())
    .unwrap_or_else(|| unit.to_string());

    Ok(NormalizedSpec {
        unit: unit.to_string(),
        kind,
        title,
        purpose,
        tags,
        description,
        exec_path: exec_path.to_string(),
        exec_argv,
        cwd,
        env,
        path,
        environment_files,
        after,
        wants_network_online,
        restart,
        nice,
        schedule,
        enabled,
        start_now,
        origin_cwd,
        created_at: created_at.unwrap_or_else(now_rfc3339),
    })
}

fn canonical_spec_json(spec: &NormalizedSpec) -> Value {
    let mut env = Map::new();
    for (k, v) in &spec.env {
        env.insert(k.clone(), json!(v));
    }
    let schedule = match &spec.schedule {
        None => Value::Null,
        Some(Schedule::Interval {
            on_boot_sec,
            on_unit_active_sec,
            persistent,
            accuracy_sec,
        }) => json!({
            "type": "interval",
            "on_boot_sec": on_boot_sec,
            "on_unit_active_sec": on_unit_active_sec,
            "persistent": persistent,
            "accuracy_sec": accuracy_sec,
        }),
        Some(Schedule::Calendar {
            on_calendar,
            persistent,
            accuracy_sec,
        }) => json!({
            "type": "calendar",
            "on_calendar": on_calendar,
            "persistent": persistent,
            "accuracy_sec": accuracy_sec,
        }),
    };
    json!({
        "after": spec.after,
        "cwd": spec.cwd,
        "description": spec.description,
        "enabled": spec.enabled,
        "env": env,
        "environment_files": spec.environment_files,
        "exec": { "argv": spec.exec_argv, "path": spec.exec_path },
        "kind": spec.kind.as_str(),
        "nice": spec.nice,
        "path": spec.path,
        "purpose": spec.purpose,
        "restart": spec.restart,
        "schedule": schedule,
        "start_now": spec.start_now,
        "tags": spec.tags,
        "title": spec.title,
        "unit": spec.unit,
        "wants_network_online": spec.wants_network_online,
    })
}

fn spec_sha(spec: &NormalizedSpec) -> String {
    sha256_hex(canonical_spec_json(spec).to_string().as_bytes())
}

fn header_lines(spec: &NormalizedSpec) -> String {
    let mut lines = vec![
        MANAGED_MARKER.to_string(),
        format!("# systemd-ops-spec-sha256: {}", spec_sha(spec)),
    ];
    if let Some(title) = &spec.title {
        lines.push(format!("# systemd-ops-title: {title}"));
    }
    if let Some(purpose) = &spec.purpose {
        lines.push(format!("# systemd-ops-purpose: {purpose}"));
    }
    if !spec.tags.is_empty() {
        lines.push(format!("# systemd-ops-tags: {}", spec.tags.join(",")));
    }
    if let Some(cwd) = &spec.origin_cwd {
        lines.push(format!("# systemd-ops-origin-cwd: {cwd}"));
    }
    lines.push(format!("# systemd-ops-created-at: {}", spec.created_at));
    lines.join("\n")
}

fn render_service(spec: &NormalizedSpec) -> String {
    let mut body = String::new();
    body.push_str(&header_lines(spec));
    body.push_str("\n[Unit]\n");
    body.push_str(&format!("Description={}\n", spec.description));
    let mut after = spec.after.clone();
    if spec.wants_network_online && !after.iter().any(|a| a == "network-online.target") {
        after.push("network-online.target".into());
    }
    if !after.is_empty() {
        body.push_str(&format!("After={}\n", after.join(" ")));
    }
    if spec.wants_network_online {
        body.push_str("Wants=network-online.target\n");
    }
    body.push_str("\n[Service]\n");
    body.push_str(&format!("Type={}\n", spec.kind.service_type()));
    if spec.kind == Kind::OneshotLinger {
        body.push_str("RemainAfterExit=yes\n");
    }
    if let Some(cwd) = &spec.cwd {
        body.push_str(&format!("WorkingDirectory={cwd}\n"));
    }
    if !spec.path.is_empty() {
        body.push_str(&format!("Environment=PATH={}\n", spec.path.join(":")));
    }
    for (k, v) in &spec.env {
        body.push_str(&format!("Environment={}={}\n", k, quote_word(v)));
    }
    for f in &spec.environment_files {
        body.push_str(&format!("EnvironmentFile={f}\n"));
    }
    body.push_str(&format!(
        "ExecStart={}\n",
        exec_line(&spec.exec_path, &spec.exec_argv)
    ));
    if let Some(restart) = &spec.restart {
        body.push_str(&format!("Restart={restart}\n"));
        if restart != "no" {
            body.push_str("RestartSec=3\n");
        }
    }
    if let Some(nice) = spec.nice {
        body.push_str(&format!("Nice={nice}\n"));
    }
    if spec.schedule.is_none() && spec.has_install() {
        body.push_str("\n[Install]\nWantedBy=default.target\n");
    }
    body
}

fn render_timer(spec: &NormalizedSpec) -> Option<String> {
    let schedule = spec.schedule.as_ref()?;
    let mut body = String::new();
    body.push_str(&header_lines(spec));
    body.push_str("\n[Unit]\n");
    body.push_str(&format!("Description={}\n", spec.description));
    body.push_str("\n[Timer]\n");
    match schedule {
        Schedule::Interval {
            on_boot_sec,
            on_unit_active_sec,
            persistent,
            accuracy_sec,
        } => {
            body.push_str(&format!("OnBootSec={on_boot_sec}\n"));
            body.push_str(&format!("OnUnitActiveSec={on_unit_active_sec}\n"));
            body.push_str(&format!("Persistent={}\n", bool_word(*persistent)));
            if let Some(a) = accuracy_sec {
                body.push_str(&format!("AccuracySec={a}\n"));
            }
        }
        Schedule::Calendar {
            on_calendar,
            persistent,
            accuracy_sec,
        } => {
            body.push_str(&format!("OnCalendar={on_calendar}\n"));
            body.push_str(&format!("Persistent={}\n", bool_word(*persistent)));
            if let Some(a) = accuracy_sec {
                body.push_str(&format!("AccuracySec={a}\n"));
            }
        }
    }
    body.push_str(&format!("Unit={}\n", spec.service_name()));
    body.push_str("\n[Install]\nWantedBy=timers.target\n");
    Some(body)
}

fn bool_word(v: bool) -> &'static str {
    if v {
        "true"
    } else {
        "false"
    }
}

fn comment_field<'a>(t: &'a str, field: &str) -> Option<&'a str> {
    let ops = format!("# systemd-ops-{field}:");
    let managed = format!("# managed-{field}:");
    t.strip_prefix(ops.as_str())
        .or_else(|| t.strip_prefix(managed.as_str()))
        .map(str::trim)
}

fn is_managed_marker(t: &str) -> bool {
    t == MANAGED_MARKER
}

fn parse_meta(text: &str) -> FileMeta {
    let mut meta = FileMeta::default();
    for line in text.lines() {
        let t = line.trim();
        if is_managed_marker(t) {
            meta.managed = true;
        } else if let Some(rest) = comment_field(t, "spec-sha256") {
            meta.spec_sha = Some(rest.to_string());
        } else if let Some(rest) = comment_field(t, "title") {
            meta.title = Some(rest.to_string());
        } else if let Some(rest) = comment_field(t, "purpose") {
            meta.purpose = Some(rest.to_string());
        } else if let Some(rest) = comment_field(t, "tags") {
            meta.tags = rest
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if let Some(rest) = comment_field(t, "origin-cwd") {
            meta.origin_cwd = Some(rest.to_string());
        } else if let Some(rest) = comment_field(t, "created-at") {
            meta.created_at = Some(rest.to_string());
        } else if let Some(rest) = t.strip_prefix("Type=") {
            meta.service_type = Some(rest.trim().to_string());
        } else if t.eq_ignore_ascii_case("RemainAfterExit=yes")
            || t.eq_ignore_ascii_case("RemainAfterExit=true")
            || t == "RemainAfterExit=1"
        {
            meta.remain_after_exit = true;
        } else if let Some(rest) = t.strip_prefix("Description=") {
            if meta.description.is_none() {
                meta.description = Some(rest.trim().to_string());
            }
        } else if let Some(rest) = t.strip_prefix("ExecStart=") {
            meta.exec_start = Some(rest.trim().to_string());
        } else if let Some(rest) = t.strip_prefix("WorkingDirectory=") {
            meta.working_directory = Some(rest.trim().to_string());
        } else if let Some(rest) = t.strip_prefix("OnCalendar=") {
            meta.on_calendar = Some(rest.trim().to_string());
        } else if let Some(rest) = t.strip_prefix("OnBootSec=") {
            meta.on_boot_sec = Some(rest.trim().to_string());
        } else if let Some(rest) = t.strip_prefix("OnUnitActiveSec=") {
            meta.on_unit_active_sec = Some(rest.trim().to_string());
        } else if let Some(rest) = t.strip_prefix("AccuracySec=") {
            meta.accuracy_sec = Some(rest.trim().to_string());
        } else if t.eq_ignore_ascii_case("Persistent=true")
            || t.eq_ignore_ascii_case("Persistent=yes")
            || t == "Persistent=1"
        {
            meta.persistent = Some(true);
        } else if t.eq_ignore_ascii_case("Persistent=false")
            || t.eq_ignore_ascii_case("Persistent=no")
            || t == "Persistent=0"
        {
            meta.persistent = Some(false);
        }
    }
    meta
}

fn read_unit_file(path: &Path) -> Option<(String, FileMeta, String)> {
    let text = fs::read_to_string(path).ok()?;
    let sha = sha256_hex(text.as_bytes());
    let meta = parse_meta(&text);
    Some((text, meta, sha))
}

fn snapshot(path: &Path) -> FileSnapshot {
    match fs::read(path) {
        Ok(bytes) => FileSnapshot {
            path: path.to_string_lossy().into_owned(),
            sha256: Some(sha256_hex(&bytes)),
        },
        Err(_) => FileSnapshot {
            path: path.to_string_lossy().into_owned(),
            sha256: None,
        },
    }
}

fn current_sha(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|b| sha256_hex(&b))
}

fn require_snapshot_fresh(snap: &FileSnapshot) -> Result<(), BackendError> {
    let now = current_sha(Path::new(&snap.path));
    if now != snap.sha256 {
        return Err(BackendError(format!(
            "plan is stale: '{}' was {} at plan time but is {} now; re-plan",
            snap.path,
            snap.sha256.as_deref().unwrap_or("missing"),
            now.as_deref().unwrap_or("missing")
        )));
    }
    Ok(())
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), BackendError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| BackendError(format!("cannot create {}: {e}", parent.display())))?;
    }
    let tmp = path.with_extension(format!(
        "{}.mcpd-new",
        path.extension().and_then(|s| s.to_str()).unwrap_or("tmp")
    ));
    fs::write(&tmp, contents)
        .map_err(|e| BackendError(format!("cannot write {}: {e}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        BackendError(format!("cannot replace {}: {e}", path.display()))
    })
}

fn planned_files(spec: &NormalizedSpec) -> Vec<(PathBuf, String)> {
    let dir = systemd::unit_file_dir();
    let mut files = vec![(dir.join(spec.service_name()), render_service(spec))];
    if let Some(timer) = render_timer(spec) {
        files.push((dir.join(format!("{}.timer", spec.unit)), timer));
    }
    files
}

fn json_or_empty_array(v: Result<Value, BackendError>) -> Vec<Value> {
    match v {
        Ok(Value::Array(a)) => a,
        _ => Vec::new(),
    }
}

fn find_row<'a>(rows: &'a [Value], name: &str) -> Option<&'a Value> {
    rows.iter().find(|row| {
        row.get("unit")
            .and_then(Value::as_str)
            .is_some_and(|u| u == name)
            || row
                .get("unit_file")
                .and_then(Value::as_str)
                .is_some_and(|u| u == name || u.ends_with(&format!("/{name}")))
    })
}

fn infer_kind(meta: &FileMeta, has_timer: bool) -> Option<&'static str> {
    if has_timer {
        return Some("oneshot");
    }
    match meta.service_type.as_deref() {
        Some("simple") => Some("simple"),
        Some("oneshot") if meta.remain_after_exit => Some("oneshot-linger"),
        Some("oneshot") => Some("oneshot"),
        _ => None,
    }
}

fn split_exec(line: &str) -> (Option<String>, Vec<String>) {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in line.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    if words.is_empty() {
        return (None, Vec::new());
    }
    let path = words.remove(0);
    (Some(path), words)
}

fn never_ran(shown: Option<&Value>, last: &Value) -> bool {
    last.is_null()
        && shown
            .and_then(|v| v.get("exec_start"))
            .map(|s| s.is_null() || s.as_str().is_none_or(|t| t.is_empty() || t == "n/a"))
            .unwrap_or(true)
}

fn honest_result(shown: Option<&Value>, last: &Value) -> Value {
    if never_ran(shown, last) {
        return Value::Null;
    }
    shown
        .and_then(|v| v.get("result"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn unit_view(name: &str, loaded: &[Value], files: &[Value], timers: &[Value]) -> Value {
    let loaded_row = find_row(loaded, name);
    let file_row = find_row(files, name);
    let timer_row = find_row(timers, name);
    let shown = systemd::get_unit(name).ok();
    let last = timer_row
        .and_then(|r| r.get("last"))
        .cloned()
        .unwrap_or(Value::Null);
    let shown_ref = shown.as_ref();
    json!({
        "unit": name,
        "type": SUFFIXES.iter().find(|s| name.ends_with(*s)).map(|s| s.trim_start_matches('.')),
        "load": shown_ref.and_then(|v| v.get("load")).cloned()
            .or_else(|| loaded_row.and_then(|r| r.get("load").cloned()))
            .unwrap_or(Value::Null),
        "active": shown_ref.and_then(|v| v.get("active")).cloned()
            .or_else(|| loaded_row.and_then(|r| r.get("active").cloned()))
            .unwrap_or(Value::Null),
        "sub": shown_ref.and_then(|v| v.get("sub")).cloned()
            .or_else(|| loaded_row.and_then(|r| r.get("sub").cloned()))
            .unwrap_or(Value::Null),
        "enabled": shown_ref.and_then(|v| v.get("enabled")).cloned()
            .or_else(|| file_row.and_then(|r| r.get("state").cloned()))
            .unwrap_or(Value::Null),
        "pid": shown_ref.and_then(|v| v.get("pid")).cloned().unwrap_or(Value::Null),
        "result": honest_result(shown_ref, &last),
        "fragment": shown_ref.and_then(|v| v.get("unit_file")).cloned().unwrap_or(Value::Null),
        "next": timer_row.and_then(|r| r.get("next")).cloned().unwrap_or(Value::Null),
        "last": last,
        "activates": timer_row.and_then(|r| r.get("activates")).cloned().unwrap_or(Value::Null),
    })
}

fn schedule_view(timer_meta: Option<&FileMeta>) -> Value {
    let Some(m) = timer_meta else {
        return Value::Null;
    };
    if let Some(cal) = &m.on_calendar {
        return json!({
            "type": "calendar",
            "on_calendar": cal,
            "persistent": m.persistent,
            "accuracy_sec": m.accuracy_sec,
        });
    }
    if m.on_boot_sec.is_some() || m.on_unit_active_sec.is_some() {
        return json!({
            "type": "interval",
            "on_boot_sec": m.on_boot_sec,
            "on_unit_active_sec": m.on_unit_active_sec,
            "persistent": m.persistent,
            "accuracy_sec": m.accuracy_sec,
        });
    }
    Value::Null
}

fn operation_view(stem: &str, loaded: &[Value], files: &[Value], timers: &[Value]) -> Value {
    let dir = systemd::unit_file_dir();
    let constituents: Vec<String> = files
        .iter()
        .filter_map(|f| {
            let name = f.get("unit_file").and_then(Value::as_str)?;
            let base = name.rsplit('/').next().unwrap_or(name);
            (stem_of(base) == stem).then(|| base.to_string())
        })
        .collect();
    let mut names = constituents.clone();
    if names.is_empty() {
        for suffix in [".service", ".timer"] {
            let candidate = format!("{stem}{suffix}");
            if loaded
                .iter()
                .any(|r| r.get("unit").and_then(Value::as_str) == Some(candidate.as_str()))
            {
                names.push(candidate);
            }
        }
    }
    names.sort();
    names.dedup();

    let mut metas = Vec::new();
    let mut fragments = Vec::new();
    for name in &names {
        let path = dir.join(name);
        if let Some((_, meta, _)) = read_unit_file(&path) {
            fragments.push(path.to_string_lossy().into_owned());
            metas.push(meta);
        } else if let Ok(shown) = systemd::get_unit(name) {
            if let Some(frag) = shown.get("unit_file").and_then(Value::as_str) {
                if !frag.is_empty() {
                    fragments.push(frag.to_string());
                    if let Some((_, meta, _)) = read_unit_file(Path::new(frag)) {
                        metas.push(meta);
                    }
                }
            }
        }
    }

    let managed = !metas.is_empty() && metas.iter().all(|m| m.managed);
    let first = metas.first();
    let has_timer = names.iter().any(|n| n.ends_with(".timer"));
    let service_idx = names.iter().position(|n| n.ends_with(".service"));
    let timer_idx = names.iter().position(|n| n.ends_with(".timer"));
    let service_meta = service_idx.and_then(|i| metas.get(i)).or(first);
    let timer_meta = timer_idx.and_then(|i| metas.get(i));

    let title = first.and_then(|m| m.title.clone());
    let purpose = first.and_then(|m| m.purpose.clone());
    let tags = first.map(|m| m.tags.clone()).unwrap_or_default();
    let origin = first.and_then(|m| m.origin_cwd.clone());
    let description = service_meta
        .and_then(|m| m.description.clone())
        .or_else(|| title.clone());
    let (exec_path, exec_argv) = service_meta
        .and_then(|m| m.exec_start.as_deref())
        .map(split_exec)
        .unwrap_or((None, Vec::new()));
    let cwd = service_meta.and_then(|m| m.working_directory.clone());

    let units: Vec<Value> = names
        .iter()
        .map(|n| unit_view(n, loaded, files, timers))
        .collect();

    let timer_name = names.iter().find(|n| n.ends_with(".timer"));
    let service_name = names.iter().find(|n| n.ends_with(".service"));
    let timer_view = timer_name.map(|n| unit_view(n, loaded, files, timers));
    let service_view = service_name.map(|n| unit_view(n, loaded, files, timers));
    let primary_view = timer_view.as_ref().or(service_view.as_ref());
    let last = timer_view
        .as_ref()
        .and_then(|v| v.get("last"))
        .cloned()
        .unwrap_or(Value::Null);
    let shown_service = service_name.and_then(|n| systemd::get_unit(n).ok());
    let last_result = honest_result(shown_service.as_ref(), &last);
    let activation = if has_timer { "timer" } else { "direct" };
    let enablement = if has_timer {
        timer_view
            .as_ref()
            .and_then(|v| v.get("enabled"))
            .cloned()
            .unwrap_or(Value::Null)
    } else {
        service_view
            .as_ref()
            .and_then(|v| v.get("enabled"))
            .cloned()
            .unwrap_or(Value::Null)
    };

    json!({
        "unit": stem,
        "title": title,
        "purpose": purpose,
        "tags": tags,
        "description": description,
        "management": if managed { "systemd-ops-managed" } else { "project-managed" },
        "kind": service_meta.and_then(|m| infer_kind(m, has_timer)),
        "activation": activation,
        "exec": { "path": exec_path, "argv": exec_argv },
        "cwd": cwd,
        "schedule": schedule_view(timer_meta),
        "constituents": units,
        "state": primary_view.and_then(|v| v.get("active")).cloned().unwrap_or(Value::Null),
        "sub": primary_view.and_then(|v| v.get("sub")).cloned().unwrap_or(Value::Null),
        "enablement": enablement,
        "last_result": last_result,
        "next": timer_view.as_ref().and_then(|v| v.get("next")).cloned().unwrap_or(Value::Null),
        "last": last,
        "origin_cwd": origin,
        "fragment_paths": fragments,
        "editable_definition": managed,
        "retire_definition": managed,
    })
}

fn inventory_globs(pattern: Option<&str>) -> Result<Vec<String>, BackendError> {
    match systemd::write_prefix() {
        Some(globs) => Ok(globs),
        None => match pattern {
            Some(p) => Ok(vec![p.to_string()]),
            None => Err(BackendError(
                "list_operations requires a pattern when --write-prefix is not set".into(),
            )),
        },
    }
}

fn collect_inventory(globs: &[String]) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let mut files = Vec::new();
    let mut loaded = Vec::new();
    let mut timers = Vec::new();
    for g in globs {
        files.extend(json_or_empty_array(systemd::list_unit_files(None, Some(g))));
        loaded.extend(json_or_empty_array(systemd::list_units(None, Some(g))));
        timers.extend(json_or_empty_array(systemd::list_timers(Some(g))));
    }
    (files, loaded, timers)
}

pub fn list_operations(pattern: Option<&str>) -> Result<Value, BackendError> {
    if let Some(p) = pattern {
        if p.is_empty() || p.len() > 256 {
            return Err(BackendError(
                "pattern must be 1..256 characters, e.g. 'managed-test-*'".into(),
            ));
        }
    }
    let globs = inventory_globs(pattern)?;
    let (files, loaded, timers) = collect_inventory(&globs);
    let mut stems: Vec<String> = files
        .iter()
        .filter_map(|f| {
            let name = f.get("unit_file").and_then(Value::as_str)?;
            let base = name.rsplit('/').next().unwrap_or(name);
            Some(stem_of(base))
        })
        .filter(|s| is_operation_stem(s))
        .collect();
    stems.sort();
    stems.dedup();
    if let Some(p) = pattern {
        stems.retain(|s| {
            systemd::glob_match(p, s) || systemd::glob_match(p, &format!("{s}.service"))
        });
    }
    let views: Vec<Value> = stems
        .iter()
        .map(|s| operation_view(s, &loaded, &files, &timers))
        .collect();
    Ok(Value::Array(views))
}

pub fn get_operation(name: &str) -> Result<Value, BackendError> {
    systemd::validate_unit_name(&if name.contains('.') {
        name.to_string()
    } else {
        format!("{name}.service")
    })?;
    let stem = stem_of(name);
    if !is_operation_stem(&stem) {
        return Err(BackendError(format!(
            "not a '{}' operation: {name}",
            write_prefix_label()
        )));
    }
    let globs = match systemd::write_prefix() {
        Some(g) => g,
        None => vec![format!("{stem}*")],
    };
    let (files, loaded, timers) = collect_inventory(&globs);
    let view = operation_view(&stem, &loaded, &files, &timers);
    let empty = view
        .get("constituents")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty);
    if empty {
        return Err(BackendError(format!("no such operation: {stem}")));
    }
    Ok(view)
}

fn existing_meta(spec_unit: &str) -> Option<FileMeta> {
    let dir = systemd::unit_file_dir();
    read_unit_file(&dir.join(format!("{spec_unit}.service"))).map(|(_, m, _)| m)
}

fn authoring_extra(
    verb: AuthoringVerb,
    spec: Option<&NormalizedSpec>,
    stem: &str,
    files: &[FileSnapshot],
    warning: Option<String>,
) -> Value {
    let mut predicted_files = Vec::new();
    if let Some(spec) = spec {
        for (path, _) in planned_files(spec) {
            predicted_files.push(path.to_string_lossy().into_owned());
        }
    } else {
        for snap in files {
            predicted_files.push(snap.path.clone());
        }
    }
    let mut out = json!({
        "unit": stem,
        "predicted_files": predicted_files,
    });
    if let Some(spec) = spec {
        out["spec_sha256"] = json!(spec_sha(spec));
        out["kind"] = json!(spec.kind.as_str());
        out["enabled"] = json!(spec.enabled);
        out["start_now"] = json!(spec.start_now);
        out["enable_unit"] = json!(spec.enable_unit());
        out["start_unit"] = json!(spec.start_unit());
        out["origin_cwd"] = json!(spec.origin_cwd);
    }
    if let Some(w) = warning {
        out["warning"] = json!(w);
    }
    let _ = verb;
    out
}

pub fn plan_create(args: &Value) -> Result<Value, BackendError> {
    let cwd = parse_context_cwd(args)?;
    let spec = parse_spec(args, cwd.clone(), None)?;
    systemd::require_write_unit(&spec.service_name())?;
    if let Some(timer) = spec.timer_name() {
        systemd::require_write_unit(&timer)?;
    }
    let planned = planned_files(&spec);
    let mut snapshots = Vec::new();
    for (path, _) in &planned {
        let snap = snapshot(path);
        if snap.sha256.is_some() {
            return Err(BackendError(format!(
                "create refused: '{}' already exists",
                path.display()
            )));
        }
        snapshots.push(snap);
    }
    let extra = authoring_extra(
        AuthoringVerb::Create,
        Some(&spec),
        &spec.unit,
        &snapshots,
        None,
    );
    write::plan_authoring(
        AuthoringWork {
            verb: AuthoringVerb::Create,
            spec: Some(spec.clone()),
            snapshots,
            origin_cwd: spec.origin_cwd,
        },
        extra,
    )
}

pub fn plan_update(args: &Value) -> Result<Value, BackendError> {
    let cwd = parse_context_cwd(args)?;
    let existing = {
        let spec_preview = spec_object(args)?;
        let unit = spec_preview
            .get("unit")
            .and_then(Value::as_str)
            .or_else(|| args.get("unit").and_then(Value::as_str))
            .ok_or_else(|| BackendError("spec.unit is required".into()))?;
        existing_meta(unit)
    };
    let Some(existing) = existing else {
        return Err(BackendError(
            "update refused: no existing service file for this stem".into(),
        ));
    };
    if !existing.managed {
        return Err(BackendError(
            "not systemd-ops-authored; lifecycle via plan_change only".into(),
        ));
    }
    let spec = parse_spec(
        args,
        existing.origin_cwd.clone(),
        existing.created_at.clone(),
    )?;
    systemd::require_write_unit(&spec.service_name())?;
    let planned = planned_files(&spec);
    let mut snapshots = Vec::new();
    for (path, _) in &planned {
        let snap = snapshot(path);
        if let Some((_, meta, _)) = read_unit_file(path) {
            if !meta.managed {
                return Err(BackendError(format!(
                    "not systemd-ops-authored; lifecycle via plan_change only ({})",
                    path.display()
                )));
            }
        } else if path.extension().and_then(|s| s.to_str()) == Some("service") {
            return Err(BackendError(format!(
                "update refused: '{}' is missing",
                path.display()
            )));
        }
        snapshots.push(snap);
    }
    let warning = cross_context_warning(spec.origin_cwd.as_deref(), cwd.as_deref());
    let extra = authoring_extra(
        AuthoringVerb::Update,
        Some(&spec),
        &spec.unit,
        &snapshots,
        warning,
    );
    write::plan_authoring(
        AuthoringWork {
            verb: AuthoringVerb::Update,
            spec: Some(spec.clone()),
            snapshots,
            origin_cwd: spec.origin_cwd,
        },
        extra,
    )
}

pub fn plan_retire(args: &Value) -> Result<Value, BackendError> {
    let cwd = parse_context_cwd(args)?;
    let unit = args
        .get("unit")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: unit".into()))?;
    let stem = stem_of(unit);
    validate_stem(&stem)?;
    let dir = systemd::unit_file_dir();
    let candidates = [
        dir.join(format!("{stem}.service")),
        dir.join(format!("{stem}.timer")),
    ];
    let mut snapshots = Vec::new();
    let mut origin = None;
    let mut saw_managed = false;
    for path in &candidates {
        if !path.exists() {
            continue;
        }
        let Some((_, meta, _)) = read_unit_file(path) else {
            continue;
        };
        if !meta.managed {
            return Err(BackendError(format!(
                "not systemd-ops-authored; lifecycle via plan_change only ({})",
                path.display()
            )));
        }
        saw_managed = true;
        if origin.is_none() {
            origin = meta.origin_cwd;
        }
        snapshots.push(snapshot(path));
    }
    if !saw_managed {
        return Err(BackendError(format!(
            "retire refused: no systemd-ops-managed files for '{stem}'"
        )));
    }
    let warning = cross_context_warning(origin.as_deref(), cwd.as_deref());
    let extra = authoring_extra(AuthoringVerb::Retire, None, &stem, &snapshots, warning);
    write::plan_authoring(
        AuthoringWork {
            verb: AuthoringVerb::Retire,
            spec: None,
            snapshots,
            origin_cwd: origin,
        },
        extra,
    )
}

pub fn apply_authoring(
    stem: &str,
    work: AuthoringWork,
    apply_cwd: Option<&str>,
) -> Result<Value, BackendError> {
    for snap in &work.snapshots {
        require_snapshot_fresh(snap)?;
    }
    systemd::require_write_unit(&format!("{stem}.service"))?;
    let warning = cross_context_warning(work.origin_cwd.as_deref(), apply_cwd);

    match work.verb {
        AuthoringVerb::Create | AuthoringVerb::Update => {
            let spec = work.spec.ok_or_else(|| {
                BackendError("authoring plan is missing its spec; re-plan".into())
            })?;
            validate_abs_path("spec.exec.path", &spec.exec_path, true)?;
            let mut written = Vec::new();
            let planned = planned_files(&spec);
            let planned_paths: Vec<String> = planned
                .iter()
                .map(|(p, _)| p.to_string_lossy().into_owned())
                .collect();
            for (path, contents) in &planned {
                write_atomic(path, contents)?;
                written.push(path.to_string_lossy().into_owned());
            }
            for snap in &work.snapshots {
                if !planned_paths.iter().any(|p| p == &snap.path) {
                    let path = Path::new(&snap.path);
                    if path.exists() {
                        if let Some((_, meta, _)) = read_unit_file(path) {
                            if meta.managed {
                                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                                    let _ = systemd::apply_verb("stop", name, None);
                                    let _ = systemd::try_disable(name);
                                }
                                let _ = fs::remove_file(path);
                            }
                        }
                    }
                }
            }
            let mut changes = systemd::daemon_reload()?;
            if let Some(unit) = spec.enable_unit() {
                changes.extend(systemd::apply_verb("enable", &unit, None)?);
            }
            if let Some(unit) = spec.start_unit() {
                changes.extend(systemd::apply_verb("start", &unit, None)?);
            }
            let view = get_operation(stem).ok();
            let mut out = json!({
                "class": "author",
                "unit": stem,
                "action": work.verb.as_str(),
                "applied": true,
                "files": written,
                "daemon_reload": true,
                "enable_unit": spec.enable_unit(),
                "start_unit": spec.start_unit(),
                "changes": changes,
                "spec_sha256": spec_sha(&spec),
                "operation": view,
                "rollback": {
                    "action": if work.verb == AuthoringVerb::Create { "retire" } else { "restore-files" },
                    "files": work.snapshots.iter().map(|s| json!({
                        "path": s.path,
                        "sha256": s.sha256,
                    })).collect::<Vec<_>>(),
                },
            });
            if let Some(w) = warning {
                out["warning"] = json!(w);
            }
            Ok(out)
        }
        AuthoringVerb::Retire => {
            let service = format!("{stem}.service");
            let timer = format!("{stem}.timer");
            let mut changes = Vec::new();
            for unit in [timer.as_str(), service.as_str()] {
                if systemd::ensure_unit_known(unit).is_ok() {
                    if let Ok(lines) = systemd::apply_verb("stop", unit, None) {
                        changes.extend(lines);
                    }
                    if let Ok(lines) = systemd::try_disable(unit) {
                        changes.extend(lines);
                    }
                }
            }
            changes.extend(systemd::daemon_reload()?);
            let mut removed = Vec::new();
            for snap in &work.snapshots {
                let path = Path::new(&snap.path);
                if path.exists() {
                    fs::remove_file(path).map_err(|e| {
                        BackendError(format!("cannot remove {}: {e}", path.display()))
                    })?;
                    removed.push(snap.path.clone());
                }
            }
            changes.extend(systemd::daemon_reload()?);
            let mut out = json!({
                "class": "author",
                "unit": stem,
                "action": "retire",
                "applied": true,
                "files": removed,
                "daemon_reload": true,
                "changes": changes,
                "operation": Value::Null,
                "rollback": Value::Null,
            });
            if let Some(w) = warning {
                out["warning"] = json!(w);
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_json() -> Value {
        json!({
            "spec": {
                "unit": "managed-test-demoop",
                "kind": "oneshot",
                "title": "Demo",
                "purpose": "exercise authoring",
                "tags": ["test", "demo"],
                "exec": { "path": "/bin/true", "argv": [] },
                "enabled": true,
                "start_now": false,
                "schedule": {
                    "type": "interval",
                    "on_boot_sec": "1min",
                    "on_unit_active_sec": "15min",
                    "persistent": true,
                    "accuracy_sec": "1min"
                }
            }
        })
    }

    #[test]
    fn stem_strips_suffix() {
        assert_eq!(stem_of("managed-mail-check.service"), "managed-mail-check");
        assert_eq!(stem_of("managed-mail-check.timer"), "managed-mail-check");
        assert_eq!(stem_of("managed-mail-check"), "managed-mail-check");
    }

    #[test]
    fn refuses_non_matching_stem() {
        systemd::set_write_prefix(Some("managed-*".into()));
        assert!(validate_stem("bluetooth").is_err());
        assert!(validate_stem("managed-test-demoop.service").is_err());
        assert!(validate_stem("managed-test-demoop").is_ok());
        systemd::set_write_prefix(None);
    }

    #[test]
    fn parses_interval_oneshot() {
        systemd::set_write_prefix(Some("managed-*".into()));
        let spec = parse_spec(&spec_json(), Some("/tmp".into()), Some("t0".into())).unwrap();
        assert_eq!(spec.kind, Kind::Oneshot);
        assert!(spec.schedule.is_some());
        assert_eq!(
            spec.timer_name().as_deref(),
            Some("managed-test-demoop.timer")
        );
        let service = render_service(&spec);
        assert!(service.contains(MANAGED_MARKER));
        assert!(service.contains("Type=oneshot"));
        assert!(!service.contains("WantedBy=default.target"));
        let timer = render_timer(&spec).unwrap();
        assert!(timer.contains("WantedBy=timers.target"));
        assert!(timer.contains("Unit=managed-test-demoop.service"));
        systemd::set_write_prefix(None);
    }

    #[test]
    fn unscheduled_oneshot_has_no_install() {
        systemd::set_write_prefix(Some("managed-*".into()));
        let args = json!({
            "spec": {
                "unit": "managed-test-ondemand",
                "kind": "oneshot",
                "exec": { "path": "/bin/true", "argv": ["ok"] }
            }
        });
        let spec = parse_spec(&args, None, Some("t0".into())).unwrap();
        let service = render_service(&spec);
        assert!(!service.contains("[Install]"));
        assert!(spec.enable_unit().is_none());
        systemd::set_write_prefix(None);
    }

    #[test]
    fn simple_cannot_schedule() {
        systemd::set_write_prefix(Some("managed-*".into()));
        let args = json!({
            "spec": {
                "unit": "managed-test-daemon",
                "kind": "simple",
                "exec": { "path": "/bin/true", "argv": [] },
                "schedule": { "type": "interval", "on_boot_sec": "1min", "on_unit_active_sec": "1min" }
            }
        });
        assert!(parse_spec(&args, None, None).is_err());
        systemd::set_write_prefix(None);
    }

    #[test]
    fn argv_rejects_shell_metacharacters() {
        systemd::set_write_prefix(Some("managed-*".into()));
        let args = json!({
            "spec": {
                "unit": "managed-test-shell",
                "kind": "oneshot",
                "exec": { "path": "/bin/true", "argv": ["a;b"] }
            }
        });
        assert!(parse_spec(&args, None, None).is_err());
        systemd::set_write_prefix(None);
    }

    #[test]
    fn after_rejects_vendor_units() {
        systemd::set_write_prefix(Some("managed-*".into()));
        let args = json!({
            "spec": {
                "unit": "managed-test-dep",
                "kind": "simple",
                "exec": { "path": "/bin/true", "argv": [] },
                "after": ["docker.service"]
            }
        });
        assert!(parse_spec(&args, None, None).is_err());
        systemd::set_write_prefix(None);
    }

    #[test]
    fn managed_marker_is_the_only_authority() {
        let text = "# systemd-ops-title: Speech Core\n[Service]\nType=simple\n";
        let meta = parse_meta(text);
        assert!(!meta.managed);
        assert_eq!(meta.title.as_deref(), Some("Speech Core"));
        let managed = format!("{MANAGED_MARKER}\n# systemd-ops-title: x\n");
        assert!(parse_meta(&managed).managed);
        assert_eq!(parse_meta(&managed).title.as_deref(), Some("x"));
        let unmarked = "# managed-title: z\n";
        assert!(!parse_meta(unmarked).managed);
        assert_eq!(parse_meta(unmarked).title.as_deref(), Some("z"));
        let old = "# managed: systemd-mcpd 1\n# managed-title: y\n";
        assert!(!parse_meta(old).managed);
    }

    #[test]
    fn managed_prefix_allows_managed_stems() {
        systemd::set_write_prefix(Some("managed-*".into()));
        assert!(validate_stem("managed-mail-check").is_ok());
        assert!(validate_stem("other-x").is_err());
        let args = json!({
            "spec": {
                "unit": "managed-test-demo",
                "kind": "oneshot",
                "title": "Managed",
                "exec": { "path": "/bin/true", "argv": [] }
            }
        });
        let spec = parse_spec(&args, Some("/tmp".into()), Some("t0".into())).unwrap();
        let service = render_service(&spec);
        assert!(service.contains(MANAGED_MARKER));
        assert!(service.contains("# systemd-ops-title: Managed"));
        systemd::set_write_prefix(None);
    }

    #[test]
    fn dual_prefix_is_comma_separated_globs() {
        systemd::set_write_prefix(Some("managed-*,tmp-*".into()));
        assert!(validate_stem("managed-mail-check").is_ok());
        assert!(validate_stem("tmp-op").is_ok());
        assert!(validate_stem("bluetooth").is_err());
        systemd::set_write_prefix(None);
    }

    #[test]
    fn cross_context_is_warning_only() {
        assert!(cross_context_warning(Some("/a"), Some("/a")).is_none());
        assert!(cross_context_warning(Some("/a"), Some("/b"))
            .unwrap()
            .contains("cross-context"));
        assert!(cross_context_warning(Some("/a"), None).is_none());
    }

    #[test]
    fn quote_empty_and_spaces() {
        assert_eq!(quote_word("ok"), "ok");
        assert_eq!(quote_word("a b"), "\"a b\"");
        assert_eq!(quote_word(""), "\"\"");
    }

    #[test]
    fn spec_sha_is_stable() {
        systemd::set_write_prefix(Some("managed-*".into()));
        let a = parse_spec(&spec_json(), Some("/tmp".into()), Some("t0".into())).unwrap();
        let b = parse_spec(&spec_json(), Some("/other".into()), Some("t1".into())).unwrap();
        assert_eq!(spec_sha(&a), spec_sha(&b));
        systemd::set_write_prefix(None);
    }
}
