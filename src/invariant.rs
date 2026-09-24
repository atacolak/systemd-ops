//! Read-only machine invariants.
//!
//! `invariant check` reports per-check status and never deletes, resets,
//! or recreates substrate. Configuration is paths only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::scope::{self, PREFERRED_MANIFEST};
use crate::systemd::BackendError;

const HEALTHY: &str = "healthy";
const DEGRADED: &str = "degraded";
const BLOCKED: &str = "blocked";

#[derive(Clone, Debug)]
pub struct InvariantConfig {
    pub home: PathBuf,
    pub hcom_db: PathBuf,
    pub dotfiles: PathBuf,
    pub systemd_ops_root: PathBuf,
    pub scope_manifests: Vec<PathBuf>,
    pub actor_registry: PathBuf,
    pub unit_dir: Option<PathBuf>,
}

impl InvariantConfig {
    pub fn example_toml() -> &'static str {
        "\
# Paths are expanded from $HOME. No username is stored.
# Copy to $XDG_CONFIG_HOME/systemd-ops/invariants.toml, or pass --invariants PATH.

[paths]
home = \"$HOME\"
hcom_db = \"$HOME/.hcom/hcom.db\"
dotfiles = \"$HOME/dotfiles\"
systemd_ops_root = \"$HOME/workspace/systemd-ops\"
actor_registry = \"$HOME/.omp/agent/sessions\"

# Optional extra scope manifests. The resolved --scope-root is always checked.
# scope_manifests = [\"$HOME/workspace/example/.systemd-ops/scope.toml\"]

# Omit to use the active manager unit directory (user: $XDG_CONFIG_HOME/systemd/user).
# unit_dir = \"$HOME/.config/systemd/user\"
"
    }
}

pub fn load_config(explicit: Option<&Path>, env_root: Option<&str>) -> InvariantConfig {
    let text = explicit
        .and_then(|path| fs::read_to_string(path).ok())
        .or_else(|| fs::read_to_string(default_config_path()).ok());
    let mut cfg = parse_config(text.as_deref().unwrap_or(""));
    if cfg.systemd_ops_root.as_os_str().is_empty() {
        if let Some(root) = env_root {
            cfg.systemd_ops_root = PathBuf::from(expand_home(root));
        }
    }
    cfg
}

fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("systemd-ops").join("invariants.toml")
}

#[derive(Deserialize, Default)]
struct RawInvariants {
    paths: Option<RawPaths>,
}

#[derive(Deserialize, Default)]
struct RawPaths {
    home: Option<String>,
    hcom_db: Option<String>,
    dotfiles: Option<String>,
    systemd_ops_root: Option<String>,
    actor_registry: Option<String>,
    scope_manifests: Option<Vec<String>>,
    unit_dir: Option<String>,
}

pub fn parse_config(text: &str) -> InvariantConfig {
    let raw: RawInvariants = toml::from_str(text).unwrap_or_default();
    let paths = raw.paths.unwrap_or_default();
    let home = paths
        .home
        .map(|value| PathBuf::from(expand_home(&value)))
        .unwrap_or_else(home_dir);
    let hcom_db = paths
        .hcom_db
        .map(|value| PathBuf::from(expand_home(&value)))
        .unwrap_or_else(|| home.join(".hcom/hcom.db"));
    let dotfiles = paths
        .dotfiles
        .map(|value| PathBuf::from(expand_home(&value)))
        .unwrap_or_else(|| home.join("dotfiles"));
    let systemd_ops_root = paths
        .systemd_ops_root
        .map(|value| PathBuf::from(expand_home(&value)))
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("SYSTEMD_OPS_SCOPE_ROOT").unwrap_or_default())
        });
    let actor_registry = paths
        .actor_registry
        .map(|value| PathBuf::from(expand_home(&value)))
        .unwrap_or_else(|| home.join(".omp/agent/sessions"));
    let scope_manifests = paths
        .scope_manifests
        .unwrap_or_default()
        .into_iter()
        .map(|value| PathBuf::from(expand_home(&value)))
        .collect();
    let unit_dir = paths
        .unit_dir
        .map(|value| PathBuf::from(expand_home(&value)));
    InvariantConfig {
        home,
        hcom_db,
        dotfiles,
        systemd_ops_root,
        scope_manifests,
        actor_registry,
        unit_dir,
    }
}

fn strip_comment(line: &str) -> &str {
    let mut in_quote = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '"' => in_quote = !in_quote,
            '#' if !in_quote => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> String {
    let value = value.trim().trim_end_matches(',');
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}



fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn expand_home(value: &str) -> String {
    let home = home_dir();
    let home = home.to_string_lossy();
    if let Some(rest) = value.strip_prefix("$HOME") {
        format!("{home}{rest}")
    } else if let Some(rest) = value.strip_prefix('~') {
        format!("{home}{rest}")
    } else {
        value.to_string()
    }
}

#[derive(Clone, Debug)]
struct Check {
    id: &'static str,
    status: &'static str,
    summary: String,
    details: Value,
}

impl Check {
    fn healthy(id: &'static str, summary: impl Into<String>) -> Self {
        Self {
            id,
            status: HEALTHY,
            summary: summary.into(),
            details: Value::Null,
        }
    }

    fn degraded(id: &'static str, summary: impl Into<String>, details: Value) -> Self {
        Self {
            id,
            status: DEGRADED,
            summary: summary.into(),
            details,
        }
    }

    fn blocked(id: &'static str, summary: impl Into<String>, details: Value) -> Self {
        Self {
            id,
            status: BLOCKED,
            summary: summary.into(),
            details,
        }
    }
}

/// Inspect substrate. Read-only: opens sqlite with `mode=ro` and never writes.
pub fn check(cfg: &InvariantConfig, scope_root: Option<&str>, cwd: Option<&str>) -> Value {
    let mut checks = Vec::new();
    checks.push(path_check("home", &cfg.home, true));
    checks.push(path_check("hcom_db", &cfg.hcom_db, false));
    checks.push(path_check("dotfiles", &cfg.dotfiles, true));
    checks.push(path_check("systemd_ops_root", &cfg.systemd_ops_root, true));
    checks.push(path_check("actor_registry", &cfg.actor_registry, true));
    checks.push(hcom_relations(&cfg.hcom_db));
    checks.push(transcripts(&cfg.hcom_db));
    checks.push(scope_leads(cfg, scope_root, cwd));
    checks.push(managed_unit_paths(cfg));
    report(&checks)
}

fn path_check(id: &'static str, path: &Path, directory: bool) -> Check {
    if path.as_os_str().is_empty() {
        return Check::blocked(id, format!("{id} is not configured"), json!({"path": ""}));
    }
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            return Check::blocked(
                id,
                format!("{id} is absent: {}", path.display()),
                json!({"path": path.display().to_string(), "error": err.to_string()}),
            );
        }
    };
    let kind_ok = if directory {
        meta.is_dir()
    } else {
        meta.is_file()
    };
    if !kind_ok {
        let expected = if directory { "directory" } else { "file" };
        return Check::blocked(
            id,
            format!("{id} is not a {expected}: {}", path.display()),
            json!({"path": path.display().to_string()}),
        );
    }
    Check::healthy(id, format!("{id} exists: {}", path.display()))
}

fn hcom_relations(db: &Path) -> Check {
    if !db.is_file() {
        return Check::blocked(
            "hcom_relations",
            "hcom.db is absent; actor registry cannot be read",
            json!({"path": db.display().to_string()}),
        );
    }
    let sql = "\
SELECT 'duplicate_session', count(*) FROM (
  SELECT session_id FROM instances
  WHERE session_id IS NOT NULL AND session_id != ''
  GROUP BY session_id HAVING count(*) > 1
)
UNION ALL
SELECT 'orphan_parent', count(*) FROM instances AS child
WHERE child.parent_session_id IS NOT NULL AND child.parent_session_id != ''
  AND NOT EXISTS (
    SELECT 1 FROM instances AS parent WHERE parent.session_id = child.parent_session_id
  )
UNION ALL
SELECT 'orphan_binding', count(*) FROM session_bindings AS binding
LEFT JOIN instances ON instances.name = binding.instance_name
WHERE instances.name IS NULL
UNION ALL
SELECT 'binding_session_mismatch', count(*) FROM session_bindings AS binding
JOIN instances ON instances.name = binding.instance_name
WHERE instances.session_id IS NOT binding.session_id
UNION ALL
SELECT 'duplicate_binding', count(*) FROM (
  SELECT instance_name FROM session_bindings
  GROUP BY instance_name HAVING count(*) > 1
)
UNION ALL
SELECT 'instances', count(*) FROM instances
UNION ALL
SELECT 'session_bindings', count(*) FROM session_bindings;";
    match sqlite_rows(db, sql) {
        Ok(rows) => relation_status(&rows),
        Err(err) => Check::blocked(
            "hcom_relations",
            "hcom.db could not be opened read-only",
            json!({"path": db.display().to_string(), "error": err}),
        ),
    }
}

fn relation_status(rows: &[(String, String)]) -> Check {
    let mut details = serde_json::Map::new();
    let mut blocked = Vec::new();
    for (key, value) in rows {
        let count: i64 = value.parse().unwrap_or(0);
        details.insert(key.clone(), json!(count));
        if matches!(
            key.as_str(),
            "duplicate_session"
                | "orphan_parent"
                | "orphan_binding"
                | "binding_session_mismatch"
                | "duplicate_binding"
        ) && count > 0
        {
            blocked.push(format!("{key}={count}"));
        }
    }
    if blocked.is_empty() {
        Check::healthy(
            "hcom_relations",
            "instances and session bindings are consistent",
        )
    } else {
        Check::blocked(
            "hcom_relations",
            format!("hcom relational invariant failed: {}", blocked.join(", ")),
            Value::Object(details),
        )
    }
}

fn transcripts(db: &Path) -> Check {
    if !db.is_file() {
        return Check::blocked(
            "transcripts",
            "hcom.db is absent; registered transcripts cannot be listed",
            json!({"path": db.display().to_string()}),
        );
    }
    let sql = "SELECT name, COALESCE(transcript_path, '') FROM instances ORDER BY name;";
    let rows = match sqlite_rows(db, sql) {
        Ok(rows) => rows,
        Err(err) => {
            return Check::blocked(
                "transcripts",
                "registered transcripts could not be read",
                json!({"error": err}),
            );
        }
    };
    let mut missing = Vec::new();
    for (name, path) in &rows {
        if path.is_empty() || !Path::new(path).is_file() {
            missing.push(json!({"instance": name, "transcript_path": path}));
        }
    }
    if missing.is_empty() {
        Check::healthy(
            "transcripts",
            format!("{} registered transcript paths exist", rows.len()),
        )
    } else {
        Check::blocked(
            "transcripts",
            format!("{} registered transcript path(s) are absent", missing.len()),
            json!({"missing": missing}),
        )
    }
}

fn scope_leads(cfg: &InvariantConfig, scope_root: Option<&str>, cwd: Option<&str>) -> Check {
    let mut manifests = cfg.scope_manifests.clone();
    match scope::resolve(scope_root, std::env::var("SYSTEMD_OPS_SCOPE_ROOT").ok().as_deref(), cwd) {
        Ok(manifest) => {
            let path = manifest.root.join(PREFERRED_MANIFEST);
            if !manifests.iter().any(|item| item == &path) {
                manifests.push(path);
            }
        }
        Err(err) => {
            if manifests.is_empty() {
                return Check::blocked(
                    "scope_leads",
                    "no scope manifest could be resolved",
                    json!({"error": err.0}),
                );
            }
        }
    }
    if manifests.is_empty() {
        return Check::blocked(
            "scope_leads",
            "no scope manifest is configured",
            Value::Null,
        );
    }
    let mut unresolved = Vec::new();
    let mut resolved = 0usize;
    for path in &manifests {
        match lead_of(path) {
            Ok(Some(_)) => resolved += 1,
            Ok(None) => unresolved.push(json!({
                "manifest": path.display().to_string(),
                "reason": "coordination.lead is absent",
            })),
            Err(reason) => unresolved.push(json!({
                "manifest": path.display().to_string(),
                "reason": reason,
            })),
        }
    }
    if unresolved.is_empty() {
        Check::healthy(
            "scope_leads",
            format!("{resolved} scope manifest(s) resolve an hcom lead"),
        )
    } else if resolved > 0 {
        Check::degraded(
            "scope_leads",
            format!("{} scope manifest(s) do not resolve an hcom lead", unresolved.len()),
            json!({"unresolved": unresolved, "resolved": resolved}),
        )
    } else {
        Check::blocked(
            "scope_leads",
            "no configured scope manifest resolves an hcom lead",
            json!({"unresolved": unresolved}),
        )
    }
}

fn lead_of(path: &Path) -> Result<Option<String>, String> {
    let text = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let mut section = "";
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            section = name.trim();
            continue;
        }
        if section == "coordination" {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "lead" {
                    let lead = unquote(value.trim());
                    if lead.starts_with("hcom:") && !lead[5..].is_empty() {
                        return Ok(Some(lead));
                    }
                    return Err(format!("coordination.lead is not an hcom handle: {lead}"));
                }
            }
        }
    }
    Ok(None)
}

fn default_unit_dir() -> PathBuf {
    match std::env::var("SYSTEMD_OPS_MANAGER").as_deref() {
        Ok("system") => PathBuf::from("/etc/systemd/system"),
        _ => {
            if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
                PathBuf::from(xdg).join("systemd/user")
            } else {
                home_dir().join(".config/systemd/user")
            }
        }
    }
}

fn managed_unit_paths(cfg: &InvariantConfig) -> Check {
    let dir = cfg.unit_dir.clone().unwrap_or_else(default_unit_dir);
    if !dir.is_dir() {
        return Check::blocked(
            "managed_unit_paths",
            format!("unit directory is absent: {}", dir.display()),
            json!({"path": dir.display().to_string()}),
        );
    }
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) => {
            return Check::blocked(
                "managed_unit_paths",
                "unit directory could not be read",
                json!({"path": dir.display().to_string(), "error": err.to_string()}),
            );
        }
    };
    let mut missing = Vec::new();
    let mut checked = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("managed-") || !name.ends_with(".service") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            missing.push(json!({"unit": name, "field": "unit", "path": path.display().to_string()}));
            continue;
        };
        checked += 1;
        for (field, value) in unit_paths(&text) {
            if !path_exists(&value) {
                missing.push(json!({
                    "unit": name,
                    "field": field,
                    "path": value,
                }));
            }
        }
    }
    if missing.is_empty() {
        Check::healthy(
            "managed_unit_paths",
            format!("{checked} managed unit(s) point at existing paths"),
        )
    } else {
        Check::blocked(
            "managed_unit_paths",
            format!("{} managed ExecStart/WorkingDirectory path(s) are absent", missing.len()),
            json!({"missing": missing}),
        )
    }
}

fn unit_paths(text: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("WorkingDirectory=") {
            let value = rest.trim();
            if !value.is_empty() && value != "-" && !value.starts_with('-') {
                out.push(("WorkingDirectory", expand_specifiers(value)));
            }
        } else if let Some(rest) = line.strip_prefix("ExecStart=") {
            if let Some(path) = exec_path(rest) {
                out.push(("ExecStart", expand_specifiers(&path)));
            }
        }
    }
    out
}

fn exec_path(rest: &str) -> Option<String> {
    let rest = rest.trim().trim_start_matches(['-', '@', ':']);
    if rest.is_empty() {
        return None;
    }
    let mut chars = rest.chars().peekable();
    let mut word = String::new();
    if chars.peek() == Some(&'"') {
        chars.next();
        for ch in chars {
            if ch == '"' {
                break;
            }
            word.push(ch);
        }
    } else {
        for ch in chars {
            if ch.is_whitespace() {
                break;
            }
            word.push(ch);
        }
    }
    if word.is_empty() || word.starts_with('$') || word.starts_with('%') {
        None
    } else {
        Some(word)
    }
}

fn expand_specifiers(value: &str) -> String {
    let home = home_dir();
    let home = home.to_string_lossy();
    value.replace("%h", home.as_ref()).replace("$HOME", home.as_ref())
}

fn path_exists(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    Path::new(value).exists()
}

fn sqlite_rows(db: &Path, sql: &str) -> Result<Vec<(String, String)>, String> {
    let uri = sqlite_uri(db);
    let output = Command::new("sqlite3")
        .arg("-readonly")
        .arg("-batch")
        .arg("-noheader")
        .arg("-separator")
        .arg("\t")
        .arg(&uri)
        .arg(sql)
        .env("SQLITE_TMPDIR", "/tmp")
        .output()
        .map_err(|err| format!("sqlite3: {err}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(err.trim().to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let (left, right) = line.split_once('\t').unwrap_or((line, ""));
        rows.push((left.to_string(), right.to_string()));
    }
    Ok(rows)
}

fn sqlite_uri(db: &Path) -> String {
    let path = db.to_string_lossy().replace('?', "%3f");
    format!("file:{path}?mode=ro")
}

fn report(checks: &[Check]) -> Value {
    let blocked = checks.iter().filter(|check| check.status == BLOCKED).count();
    let degraded = checks.iter().filter(|check| check.status == DEGRADED).count();
    let status = if blocked > 0 {
        BLOCKED
    } else if degraded > 0 {
        DEGRADED
    } else {
        HEALTHY
    };
    json!({
        "command": "invariant",
        "readonly": true,
        "status": status,
        "exit_code": if blocked > 0 { 1 } else { 0 },
        "blocked": blocked,
        "degraded": degraded,
        "healthy": checks.len() - blocked - degraded,
        "checks": checks.iter().map(check_json).collect::<Vec<_>>(),
    })
}

fn check_json(check: &Check) -> Value {
    json!({
        "id": check.id,
        "status": check.status,
        "summary": check.summary,
        "details": check.details,
    })
}

pub fn exit_blocked(data: &Value) -> bool {
    data.get("status").and_then(Value::as_str) == Some(BLOCKED)
        || data.get("exit_code").and_then(Value::as_u64) == Some(1)
}
pub fn run(
    args: &mut Vec<String>,
    scope_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<(Value, bool), BackendError> {
    let mut config_path = None;
    let mut kept = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        let arg = args[idx].clone();
        if arg == "--invariants" {
            let value = args.get(idx + 1).cloned().ok_or_else(|| {
                BackendError("missing --invariants value".into())
            })?;
            config_path = Some(PathBuf::from(value));
            idx += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--invariants=") {
            config_path = Some(PathBuf::from(value));
            idx += 1;
            continue;
        }
        kept.push(arg);
        idx += 1;
    }
    *args = kept;
    if args.first().map(String::as_str) != Some("check") {
        let got = args.first().map(String::as_str).unwrap_or("(none)");
        return Err(BackendError(format!(
            "unknown invariant command '{got}' (check)"
        )));
    }
    args.remove(0);
    if !args.is_empty() {
        return Err(BackendError(format!(
            "unknown flag '{}' (invariant check [--invariants PATH])",
            args[0]
        )));
    }
    let cfg = load_config(config_path.as_deref(), scope_root);
    let data = check(&cfg, scope_root, cwd);
    let blocked = exit_blocked(&data);
    Ok((data, blocked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("systemd-ops-invariant-{label}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_db(path: &Path, sql: &str) {
        let status = Command::new("sqlite3")
            .arg(path)
            .arg(sql)
            .status()
            .expect("sqlite3");
        assert!(status.success());
    }

    #[test]
    fn missing_hcom_db_and_actor_registry_are_blocked() {
        let root = scratch("missing");
        let cfg = InvariantConfig {
            home: root.join("no-home"),
            hcom_db: root.join("no-hcom.db"),
            dotfiles: root.join("no-dotfiles"),
            systemd_ops_root: root.join("no-ops"),
            scope_manifests: vec![root.join("no-scope.toml")],
            actor_registry: root.join("no-actors"),
            unit_dir: Some(root.join("no-units")),
        };
        let before = fs::read_dir(&root).unwrap().count();
        let report = check(&cfg, None, None);
        let after = fs::read_dir(&root).unwrap().count();
        assert_eq!(before, after, "invariant check must not create files");
        assert_eq!(report["status"], json!("blocked"));
        assert_eq!(report["readonly"], json!(true));
        assert_eq!(report["exit_code"], json!(1));
        assert!(exit_blocked(&report));
        let ids: Vec<&str> = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|check| check["status"] == json!("blocked"))
            .map(|check| check["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"hcom_db"));
        assert!(ids.contains(&"actor_registry"));
        assert!(ids.contains(&"home"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn consistent_fixture_is_healthy_and_orphan_blocks() {
        let root = scratch("healthy");
        let home = root.join("home");
        let actors = home.join(".omp/agent/sessions");
        let dotfiles = root.join("dotfiles");
        let ops = root.join("ops");
        let units = root.join("units");
        fs::create_dir_all(&actors).unwrap();
        fs::create_dir_all(&dotfiles).unwrap();
        fs::create_dir_all(ops.join(".systemd-ops")).unwrap();
        fs::create_dir_all(&units).unwrap();
        fs::write(actors.join("hera.jsonl"), "ok\n").unwrap();
        fs::write(root.join("bin"), "#!/bin/true\n").unwrap();
        fs::write(
            ops.join(".systemd-ops/scope.toml"),
            "[scope]\nid = \"ops\"\nowned = [\"managed-ops-*\"]\n\n[coordination]\nlead = \"hcom:hera\"\n",
        )
        .unwrap();
        fs::write(
            units.join("managed-ops.service"),
            format!(
                "[Service]\nWorkingDirectory={}\nExecStart={}\n",
                ops.display(),
                root.join("bin").display()
            ),
        )
        .unwrap();
        let db = home.join(".hcom/hcom.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        write_db(
            &db,
            "CREATE TABLE instances (name TEXT PRIMARY KEY, session_id TEXT, parent_session_id TEXT, transcript_path TEXT);
             CREATE TABLE session_bindings (session_id TEXT PRIMARY KEY, instance_name TEXT NOT NULL);
             INSERT INTO instances VALUES ('hera', 'sess-hera', NULL, '');
             INSERT INTO session_bindings VALUES ('sess-hera', 'hera');",
        );
        // transcript path filled after the file exists
        let transcript = actors.join("hera.jsonl");
        write_db(
            &db,
            &format!(
                "UPDATE instances SET transcript_path = '{}' WHERE name = 'hera';",
                transcript.display()
            ),
        );
        let cfg = InvariantConfig {
            home: home.clone(),
            hcom_db: db.clone(),
            dotfiles,
            systemd_ops_root: ops.clone(),
            scope_manifests: vec![ops.join(".systemd-ops/scope.toml")],
            actor_registry: actors,
            unit_dir: Some(units.clone()),
        };
        let report = check(&cfg, None, Some(root.to_str().unwrap()));
        assert_eq!(report["status"], json!("healthy"), "{report}");
        assert!(!exit_blocked(&report));

        write_db(
            &db,
            "INSERT INTO instances VALUES ('orphan', 'sess-orphan', 'missing-parent', '');",
        );
        let broken = check(&cfg, None, Some(root.to_str().unwrap()));
        assert_eq!(broken["status"], json!("blocked"), "{broken}");
        let relations = broken["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == json!("hcom_relations"))
            .unwrap();
        assert_eq!(relations["status"], json!("blocked"));
        assert_eq!(relations["details"]["orphan_parent"], json!(1));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn example_config_expands_home_without_a_username() {
        let text = InvariantConfig::example_toml();
        assert!(!text.contains("/home/"));
        assert!(text.contains("$HOME/.hcom/hcom.db"));
        let cfg = parse_config(text);
        let home = home_dir();
        assert_eq!(cfg.hcom_db, home.join(".hcom/hcom.db"));
        assert_eq!(cfg.actor_registry, home.join(".omp/agent/sessions"));
        assert_eq!(cfg.dotfiles, home.join("dotfiles"));
    }

    #[test]
    fn multiline_scope_manifests_expand_each_path() {
        let cfg = parse_config(
            "[paths]\nscope_manifests = [\n  \"$HOME/workspace/cpa-farm/.systemd-ops/scope.toml\",\n  \"$HOME/workspace/oh-my-pi/.systemd-ops/scope.toml\",\n]\n",
        );
        let home = home_dir();
        assert_eq!(
            cfg.scope_manifests,
            vec![
                home.join("workspace/cpa-farm/.systemd-ops/scope.toml"),
                home.join("workspace/oh-my-pi/.systemd-ops/scope.toml"),
            ]
        );
        assert!(cfg.scope_manifests.iter().all(|path| path != Path::new("[")));
    }
}
