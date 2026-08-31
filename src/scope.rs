//! Project-local responsibility scopes.
//!
//! `.systemd-ops/scope.toml` (preferred) or `.systemd-ops.toml`
//! (legacy) is discovered by walking upward from cwd. The directory
//! that contains it is the scope root. ScopeView is derived on demand.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::operations;
use crate::systemd::{self, BackendError};

pub const PREFERRED_MANIFEST: &str = ".systemd-ops/scope.toml";
pub const LEGACY_MANIFEST: &str = ".systemd-ops.toml";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeManifest {
    pub id: String,
    pub root: PathBuf,
    pub owned: Vec<String>,
    pub critical: Vec<String>,
    pub watch: Vec<String>,
    pub automation_agent_root: Option<PathBuf>,
    pub coordination_lead: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeHealth {
    Healthy,
    Degraded,
    Failed,
    Unknown,
}

impl ScopeHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeHealth::Healthy => "healthy",
            ScopeHealth::Degraded => "degraded",
            ScopeHealth::Failed => "failed",
            ScopeHealth::Unknown => "unknown",
        }
    }

    fn rank(self) -> u8 {
        match self {
            ScopeHealth::Healthy => 0,
            ScopeHealth::Unknown => 1,
            ScopeHealth::Degraded => 2,
            ScopeHealth::Failed => 3,
        }
    }

    fn raise(self, other: ScopeHealth) -> ScopeHealth {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attention {
    pub operation: String,
    pub relationship: &'static str,
    pub code: &'static str,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct ScopeView {
    pub id: String,
    pub automation_agent_root: Option<PathBuf>,
    pub coordination_lead: Option<String>,
    pub root: PathBuf,
    pub health: ScopeHealth,
    pub owned: Vec<Value>,
    pub watching: Vec<Value>,
    pub attention: Vec<Attention>,
    pub warnings: Vec<String>,
}

impl ScopeView {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "root": self.root.to_string_lossy(),
            "automation": {
                "agent_root": self.automation_agent_root.as_ref().map(|path| path.to_string_lossy()),
            },
            "coordination": {
                "lead": self.coordination_lead,
            },
            "health": self.health.as_str(),
            "owned": self.owned,
            "watching": self.watching,
            "attention": self.attention.iter().map(|a| json!({
                "operation": a.operation,
                "relationship": a.relationship,
                "code": a.code,
                "reason": a.reason,
            })).collect::<Vec<_>>(),
            "warnings": self.warnings,
        })
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawFile {
    scope: Option<RawScope>,
    automation: Option<RawAutomation>,
    coordination: Option<RawCoordination>,
    watch: Option<Vec<RawWatch>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawScope {
    id: Option<String>,
    owned: Option<Vec<String>>,
    critical: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawAutomation {
    agent_root: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawCoordination {
    lead: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawWatch {
    operation: Option<String>,
}

fn manifest_at(root: &Path) -> Result<Option<PathBuf>, BackendError> {
    let preferred = root.join(PREFERRED_MANIFEST);
    let legacy = root.join(LEGACY_MANIFEST);
    let preferred_exists = preferred.is_file();
    let legacy_exists = legacy.is_file();
    if preferred_exists && legacy_exists {
        return Err(BackendError(format!(
            "ambiguous scope at {}: both {PREFERRED_MANIFEST} and {LEGACY_MANIFEST} exist",
            root.display()
        )));
    }
    Ok(if preferred_exists {
        Some(preferred)
    } else if legacy_exists {
        Some(legacy)
    } else {
        None
    })
}

fn read_manifest(root: PathBuf, path: PathBuf) -> Result<ScopeManifest, BackendError> {
    let text = fs::read_to_string(&path)
        .map_err(|e| BackendError(format!("cannot read {}: {e}", path.display())))?;
    parse_manifest_named(&text, root, &path)
}

pub fn discover(start: &Path) -> Result<ScopeManifest, BackendError> {
    let start = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| BackendError(format!("cannot resolve cwd: {e}")))?
            .join(start)
    };
    let mut dir = if start.is_dir() {
        start
    } else {
        start
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| BackendError("cannot discover a scope from this path".into()))?
    };
    loop {
        if let Some(path) = manifest_at(&dir)? {
            return read_manifest(dir, path);
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => {
                return Err(BackendError(format!(
                    "no {PREFERRED_MANIFEST} or {LEGACY_MANIFEST} found walking up from the working directory; this is a per-project console, not an all-systemd dashboard"
                )))
            }
        }
    }
}

pub fn discover_explicit(root: &Path) -> Result<ScopeManifest, BackendError> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| BackendError(format!("cannot resolve cwd: {e}")))?
            .join(root)
    };
    let path = manifest_at(&root)?.ok_or_else(|| {
        BackendError(format!(
            "explicit scope root {} contains neither {PREFERRED_MANIFEST} nor {LEGACY_MANIFEST}",
            root.display()
        ))
    })?;
    read_manifest(root, path)
}

/// Resolve a scope with explicit root, environment root, then cwd discovery precedence.
pub fn resolve(
    explicit_root: Option<&str>,
    environment_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<ScopeManifest, BackendError> {
    if let Some(root) = explicit_root.filter(|s| !s.is_empty()) {
        return discover_explicit(Path::new(root));
    }
    if let Some(root) = environment_root.filter(|s| !s.is_empty()) {
        return discover_explicit(Path::new(root));
    }
    match cwd {
        Some(c) => discover(Path::new(c)),
        None => {
            let here = std::env::current_dir()
                .map_err(|e| BackendError(format!("cannot resolve cwd: {e}")))?;
            discover(&here)
        }
    }
}

pub fn discover_from_cwd(cwd: Option<&str>) -> Result<ScopeManifest, BackendError> {
    let env_root = std::env::var("SYSTEMD_OPS_SCOPE_ROOT").ok();
    resolve(None, env_root.as_deref(), cwd)
}

pub fn parse_manifest(text: &str, root: PathBuf) -> Result<ScopeManifest, BackendError> {
    parse_manifest_named(text, root, Path::new(PREFERRED_MANIFEST))
}

fn parse_manifest_named(
    text: &str,
    root: PathBuf,
    manifest_path: &Path,
) -> Result<ScopeManifest, BackendError> {
    let name = manifest_path.to_string_lossy();
    let raw: RawFile =
        toml::from_str(text).map_err(|e| BackendError(format!("malformed {name}: {e}")))?;
    let scope = raw
        .scope
        .ok_or_else(|| BackendError(format!("{name} is missing [scope]")))?;
    let id = scope
        .id
        .ok_or_else(|| BackendError(format!("{name} is missing scope.id")))?;
    validate_scope_id(&id)?;
    let owned = scope.owned.unwrap_or_default();
    if owned.is_empty() {
        return Err(BackendError(
            "scope.owned must list at least one operation glob".into(),
        ));
    }
    for g in &owned {
        validate_owned_glob(g)?;
    }
    let mut critical = scope.critical.unwrap_or_default();
    critical.sort();
    critical.dedup();
    for stem in &critical {
        validate_stem_name(stem, "critical")?;
        if !owned.iter().any(|g| glob_matches_stem(g, stem)) {
            return Err(BackendError(format!(
                "critical operation '{stem}' is not matched by scope.owned"
            )));
        }
    }
    let mut watch = Vec::new();
    for entry in raw.watch.unwrap_or_default() {
        let op = entry
            .operation
            .ok_or_else(|| BackendError("[[watch]] entry is missing operation".into()))?;
        validate_stem_name(&op, "watch")?;
        if owned.iter().any(|g| glob_matches_stem(g, &op)) {
            return Err(BackendError(format!(
                "operation '{op}' cannot be both owned and watched"
            )));
        }
        watch.push(op);
    }
    watch.sort();
    watch.dedup();
    let automation_agent_root = match raw.automation.and_then(|automation| automation.agent_root) {
        Some(value) => {
            let path = PathBuf::from(&value);
            if !path.is_absolute() {
                return Err(BackendError(
                    "automation.agent_root must be an absolute path".into(),
                ));
            }
            if !path.is_dir() {
                return Err(BackendError(format!(
                    "automation.agent_root '{}' is not a directory",
                    path.display()
                )));
            }
            Some(path)
        }
        None => None,
    };
    let coordination_lead = match raw.coordination.and_then(|coordination| coordination.lead) {
        Some(value) => Some(validate_coordination_lead(&value)?),
        None => None,
    };
    Ok(ScopeManifest {
        id,
        root,
        owned,
        critical,
        watch,
        automation_agent_root,
        coordination_lead,
    })
}

pub fn validate_resolved(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let env_root = std::env::var("SYSTEMD_OPS_SCOPE_ROOT").ok();
    let manifest = resolve(explicit_root, env_root.as_deref(), cwd)?;
    Ok(json!({
        "ok": true,
        "id": manifest.id,
        "root": manifest.root.to_string_lossy(),
        "owned": manifest.owned,
        "critical": manifest.critical,
        "watch": manifest.watch,
    }))
}

pub fn validate(cwd: Option<&str>) -> Result<Value, BackendError> {
    validate_resolved(None, cwd)
}

pub fn show_resolved(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<ScopeView, BackendError> {
    let env_root = std::env::var("SYSTEMD_OPS_SCOPE_ROOT").ok();
    let manifest = resolve(explicit_root, env_root.as_deref(), cwd)?;
    show_manifest(&manifest)
}

pub fn show(cwd: Option<&str>) -> Result<ScopeView, BackendError> {
    show_resolved(None, cwd)
}

pub fn show_manifest(manifest: &ScopeManifest) -> Result<ScopeView, BackendError> {
    let mut owned_views = operations::list_operations_matching(&manifest.owned)?;
    let present: Vec<String> = owned_views
        .iter()
        .filter_map(|v| v.get("unit").and_then(Value::as_str).map(str::to_string))
        .collect();
    for stem in &manifest.critical {
        if !present.iter().any(|s| s == stem) {
            owned_views.push(missing_view(stem));
        }
    }
    owned_views.sort_by(|a, b| {
        a.get("unit")
            .and_then(Value::as_str)
            .cmp(&b.get("unit").and_then(Value::as_str))
    });

    let mut watching = Vec::new();
    for stem in &manifest.watch {
        watching.push(match operations::get_operation_any(stem) {
            Ok(v) => v,
            Err(_) => missing_view(stem),
        });
    }

    Ok(aggregate(manifest, owned_views, watching))
}

pub fn aggregate(
    manifest: &ScopeManifest,
    owned_ops: Vec<Value>,
    watching_ops: Vec<Value>,
) -> ScopeView {
    let mut health = ScopeHealth::Healthy;
    let mut attention = Vec::new();
    let mut warnings = Vec::new();
    let mut owned = Vec::new();
    for mut view in owned_ops {
        let unit = view
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let critical = manifest.critical.iter().any(|c| c == &unit);
        let (automation, automation_warning) = crate::automation::join_operation(manifest, &unit);
        let completed = automation
            .get("lifecycle")
            .and_then(|lifecycle| lifecycle.get("status"))
            .and_then(Value::as_str)
            == Some("completed");
        let op_health = if completed {
            "healthy"
        } else {
            operations::operation_health(&view)
        };
        view["health"] = json!(op_health);
        view["relationship"] = json!("owned");
        view["critical"] = json!(critical);
        view["scope_id"] = json!(manifest.id);
        view["scope_root"] = json!(manifest.root.to_string_lossy());
        view["operation_home"] =
            json!(crate::automation::operation_home(&manifest.root, &unit).to_string_lossy());

        view["automation"] = automation;
        if view.get("definition_revision").is_none() {
            view["definition_revision"] = Value::Null;
        }
        let def_rev = view
            .get("definition_revision")
            .and_then(Value::as_str)
            .map(str::to_string);
        let (operator, operator_state, warn) =
            crate::operator::join_for_scope(&manifest.root, &unit, def_rev.as_deref());
        view["operator"] = operator;
        view["operator_state"] = json!(operator_state.as_str());
        view["basis_revision"] = view
            .get("operator")
            .and_then(|operator| operator.get("basis_revision"))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(w) = automation_warning {
            warnings.push(w);
        }
        if let Some(w) = warn {
            warnings.push(w);
        }
        match op_health {
            "failed" if critical => {
                health = health.raise(ScopeHealth::Failed);
                attention.push(Attention {
                    operation: unit,
                    relationship: "owned",
                    code: "operation_failed",
                    reason: "failed".into(),
                });
            }
            "failed" => {
                health = health.raise(ScopeHealth::Degraded);
                attention.push(Attention {
                    operation: unit,
                    relationship: "owned",
                    code: "operation_failed",
                    reason: "failed".into(),
                });
            }
            "unknown" if critical => {
                health = health.raise(ScopeHealth::Unknown);
                attention.push(Attention {
                    operation: unit,
                    relationship: "owned",
                    code: "operation_unknown",
                    reason: "unknown".into(),
                });
            }
            _ => {}
        }
        owned.push(view);
    }
    crate::automation::derive_semantic_states(&mut owned);
    crate::automation::attach_relations(&mut owned);
    let mut watching = Vec::new();
    for mut view in watching_ops {
        let unit = view
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let op_health = operations::operation_health(&view);
        view["health"] = json!(op_health);
        view["relationship"] = json!("watching");
        view["critical"] = json!(false);
        view["scope_id"] = json!(manifest.id);
        view["scope_root"] = json!(manifest.root.to_string_lossy());
        view["operation_home"] =
            json!(crate::automation::operation_home(&manifest.root, &unit).to_string_lossy());

        if view.get("definition_revision").is_none() {
            view["definition_revision"] = Value::Null;
        }
        view["operator"] = Value::Null;
        view["operator_state"] = Value::Null;
        view["basis_revision"] = Value::Null;
        if op_health == "failed" {
            health = health.raise(ScopeHealth::Degraded);
            attention.push(Attention {
                operation: unit.clone(),
                relationship: "watching",
                code: "operation_failed",
                reason: "failed".into(),
            });
        }
        watching.push(view);
    }
    if owned.is_empty() && watching.is_empty() {
        health = ScopeHealth::Unknown;
    }
    ScopeView {
        id: manifest.id.clone(),
        automation_agent_root: manifest.automation_agent_root.clone(),
        coordination_lead: manifest.coordination_lead.clone(),
        root: manifest.root.clone(),
        health,
        owned,
        watching,
        attention,
        warnings,
    }
}

fn validate_coordination_lead(value: &str) -> Result<String, BackendError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(BackendError(
            "coordination.lead must be 1..128 characters".into(),
        ));
    }
    if value.contains(['\n', '\r', ' ']) {
        return Err(BackendError(
            "coordination.lead must be a single token".into(),
        ));
    }
    if !value.starts_with("hcom:") {
        return Err(BackendError(
            "coordination.lead must be an opaque hcom:<name> handle".into(),
        ));
    }
    let name = &value[5..];
    if name.len() != 4 || !name.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(BackendError(
            "coordination.lead must be hcom: plus a four-letter lowercase name".into(),
        ));
    }
    Ok(value.to_string())
}

fn missing_view(stem: &str) -> Value {
    json!({
        "unit": stem,
        "title": Value::Null,
        "purpose": Value::Null,
        "tags": [],
        "management": "missing",
        "kind": Value::Null,
        "state": Value::Null,
        "sub": Value::Null,
        "last_result": Value::Null,
        "last": Value::Null,
        "next": Value::Null,
        "editable_spec": Value::Null,
        "definition_revision": Value::Null,
        "missing": true,
    })
}

fn validate_scope_id(id: &str) -> Result<(), BackendError> {
    let ok = (1..=64).contains(&id.len())
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(BackendError(format!(
            "scope.id '{id}' must be 1..64 characters matching [a-z][a-z0-9-]*"
        )))
    }
}

fn validate_owned_glob(g: &str) -> Result<(), BackendError> {
    if g.is_empty() || g.len() > 128 {
        return Err(BackendError(
            "scope.owned glob must be 1..128 characters".into(),
        ));
    }
    if g.contains('\n') || g.contains('\0') || g.contains(',') {
        return Err(BackendError(format!(
            "malformed owned glob '{g}': commas and newlines are not allowed"
        )));
    }
    Ok(())
}

fn validate_stem_name(stem: &str, label: &str) -> Result<(), BackendError> {
    if stem.is_empty() || stem.len() > 256 {
        return Err(BackendError(format!(
            "{label} operation stem must be 1..256 characters"
        )));
    }
    if stem.contains('.') {
        return Err(BackendError(format!(
            "{label} operation '{stem}' must be a stem without a unit suffix"
        )));
    }
    systemd::validate_unit_name(&format!("{stem}.service"))
        .map_err(|e| BackendError(format!("{label} operation '{stem}': {e}")))?;
    Ok(())
}

pub fn glob_matches_stem(glob: &str, stem: &str) -> bool {
    systemd::glob_match(glob, stem) || systemd::glob_match(glob, &format!("{stem}.service"))
}

pub fn provenance_warning(
    origin_cwd: Option<&str>,
    origin_scope: Option<&str>,
    current_cwd: Option<&str>,
) -> Option<String> {
    let current_scope = current_cwd.and_then(|c| discover(Path::new(c)).ok().map(|m| m.id));
    match (origin_scope, current_scope.as_deref()) {
        (Some(o), Some(c)) if o == c => None,
        (Some(o), Some(c)) => Some(format!(
            "cross-scope: origin_scope is {o}, current scope is {c}"
        )),
        _ => match (origin_cwd, current_cwd) {
            (Some(o), Some(c)) if o != c => Some(format!(
                "cross-context: origin_cwd is {o}, current context.cwd is {c}"
            )),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("systemd-ops-scope-{name}-{nanos}"));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn sample() -> &'static str {
        r#"
[scope]
id = "speech"
owned = ["managed-speech-*"]
critical = [
  "managed-speech-asr",
  "managed-speech-tts",
]

[[watch]]
operation = "managed-proxy-health"
"#
    }

    #[test]
    fn parses_v1_schema() {
        let m = parse_manifest(sample(), PathBuf::from("/proj/speech-core")).unwrap();
        assert_eq!(m.id, "speech");
        assert_eq!(m.root, PathBuf::from("/proj/speech-core"));
        assert_eq!(m.owned, vec!["managed-speech-*"]);
        assert_eq!(m.critical, vec!["managed-speech-asr", "managed-speech-tts"]);
        assert_eq!(m.watch, vec!["managed-proxy-health"]);
    }

    #[test]
    fn discovers_preferred_legacy_and_upward() {
        let preferred = tmp("preferred");
        fs::create_dir_all(preferred.join(".systemd-ops")).unwrap();
        fs::write(preferred.join(PREFERRED_MANIFEST), sample()).unwrap();
        let nested = preferred.join("src/bin");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(discover(&nested).unwrap().root, preferred);

        let legacy = tmp("legacy");
        fs::write(legacy.join(LEGACY_MANIFEST), sample()).unwrap();
        assert_eq!(discover(&legacy).unwrap().root, legacy);
        let _ = fs::remove_dir_all(&preferred);
        let _ = fs::remove_dir_all(&legacy);
    }

    #[test]
    fn same_root_manifest_coexistence_is_ambiguous() {
        let root = tmp("ambiguous");
        fs::create_dir_all(root.join(".systemd-ops")).unwrap();
        fs::write(root.join(PREFERRED_MANIFEST), sample()).unwrap();
        fs::write(root.join(LEGACY_MANIFEST), sample()).unwrap();
        let err = discover(&root).unwrap_err();
        assert!(err.0.contains("ambiguous scope"), "got {}", err.0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_then_environment_then_cwd_precedence() {
        let explicit = tmp("explicit");
        let environment = tmp("environment");
        let cwd = tmp("cwd");
        for (root, id) in [
            (&explicit, "explicit"),
            (&environment, "environment"),
            (&cwd, "cwd"),
        ] {
            fs::write(
                root.join(LEGACY_MANIFEST),
                format!("[scope]\nid=\"{id}\"\nowned=[\"managed-{id}-*\"]\n"),
            )
            .unwrap();
        }
        assert_eq!(
            resolve(
                Some(&explicit.to_string_lossy()),
                Some(&environment.to_string_lossy()),
                Some(&cwd.to_string_lossy()),
            )
            .unwrap()
            .id,
            "explicit"
        );
        assert_eq!(
            resolve(
                None,
                Some(&environment.to_string_lossy()),
                Some(&cwd.to_string_lossy()),
            )
            .unwrap()
            .id,
            "environment"
        );
        assert_eq!(
            resolve(None, None, Some(&cwd.to_string_lossy()))
                .unwrap()
                .id,
            "cwd"
        );
        let _ = fs::remove_dir_all(explicit);
        let _ = fs::remove_dir_all(environment);
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn worktree_cwd_discovers_its_own_scope() {
        let root = tmp("worktree");
        fs::create_dir_all(root.join(".systemd-ops")).unwrap();
        fs::write(root.join(PREFERRED_MANIFEST), sample()).unwrap();
        let worktree_cwd = root.join("crates/systemd-ops/src");
        fs::create_dir_all(&worktree_cwd).unwrap();
        let manifest = resolve(None, None, Some(&worktree_cwd.to_string_lossy())).unwrap();
        assert_eq!(manifest.root, root);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let dir = tmp("none");
        let err = discover(&dir).unwrap_err();
        assert!(err.0.contains(PREFERRED_MANIFEST), "got {}", err.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_toml_is_an_error() {
        let err = parse_manifest("[[[nope", PathBuf::from("/x")).unwrap_err();
        assert!(err.0.contains("malformed"), "got {}", err.0);
    }

    #[test]
    fn missing_scope_id_is_an_error() {
        let err =
            parse_manifest("[scope]\nowned=[\"managed-x-*\"]\n", PathBuf::from("/x")).unwrap_err();
        assert!(err.0.contains("scope.id"), "got {}", err.0);
    }

    #[test]
    fn malformed_owned_glob_is_an_error() {
        let err = parse_manifest("[scope]\nid=\"x\"\nowned=[\"a,b\"]\n", PathBuf::from("/x"))
            .unwrap_err();
        assert!(err.0.contains("malformed owned glob"), "got {}", err.0);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = parse_manifest(
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\ncritcal=[\"managed-speech-asr\"]\n",
            PathBuf::from("/x"),
        )
        .unwrap_err();
        assert!(err.0.contains("malformed"), "got {}", err.0);
    }

    #[test]
    fn coordination_lead_is_optional_and_opaque() {
        let without = parse_manifest(
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\n",
            PathBuf::from("/x"),
        )
        .unwrap();
        assert!(without.coordination_lead.is_none());
        let with = parse_manifest(
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\n[coordination]\nlead=\"hcom:riko\"\n",
            PathBuf::from("/x"),
        )
        .unwrap();
        assert_eq!(with.coordination_lead.as_deref(), Some("hcom:riko"));
        let err = parse_manifest(
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\n[coordination]\nlead=\"operator\"\n",
            PathBuf::from("/x"),
        )
        .unwrap_err();
        assert!(err.0.contains("hcom:"), "{}", err.0);
        let err = parse_manifest(
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\n[coordination]\nlead=\"hcom:RIKO\"\n",
            PathBuf::from("/x"),
        )
        .unwrap_err();
        assert!(err.0.contains("four-letter"), "{}", err.0);
    }

    #[test]
    fn top_level_critical_is_rejected() {
        let err = parse_manifest(
            "critical=[\"managed-speech-asr\"]\n[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\n",
            PathBuf::from("/x"),
        )
        .unwrap_err();
        assert!(err.0.contains("malformed"), "got {}", err.0);
    }

    #[test]
    fn critical_must_match_owned() {
        let err = parse_manifest(
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\ncritical=[\"managed-proxy-x\"]\n",
            PathBuf::from("/x"),
        )
        .unwrap_err();
        assert!(
            err.0.contains("not matched by scope.owned"),
            "got {}",
            err.0
        );
    }

    #[test]
    fn watch_owned_overlap_is_an_error() {
        let err = parse_manifest(
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\n\n[[watch]]\noperation=\"managed-speech-tts\"\n",
            PathBuf::from("/x"),
        )
        .unwrap_err();
        assert!(err.0.contains("both owned and watched"), "got {}", err.0);
    }

    #[test]
    fn watch_requires_explicit_stem() {
        let err = parse_manifest(
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\n\n[[watch]]\noperation=\"managed-proxy-health.service\"\n",
            PathBuf::from("/x"),
        )
        .unwrap_err();
        assert!(err.0.contains("without a unit suffix"), "got {}", err.0);
    }

    fn op(unit: &str, health_bits: Value) -> Value {
        let mut v = health_bits;
        v["unit"] = json!(unit);
        v
    }

    fn manifest() -> ScopeManifest {
        parse_manifest(sample(), PathBuf::from("/proj/speech-core")).unwrap()
    }

    #[test]
    fn all_healthy_is_healthy() {
        let view = aggregate(
            &manifest(),
            vec![
                op(
                    "managed-speech-asr",
                    json!({"kind":"simple","state":"active","sub":"running"}),
                ),
                op(
                    "managed-speech-tts",
                    json!({"kind":"simple","state":"active","sub":"running"}),
                ),
            ],
            vec![op(
                "managed-proxy-health",
                json!({"kind":"oneshot","activation":"timer","enablement":"enabled","next":"2026-01-02T07:00:00Z","last_result":"success","last":"2026-01-01T00:00:00Z"}),
            )],
        );
        assert_eq!(view.health, ScopeHealth::Healthy);
        assert!(view.attention.is_empty());
        assert_eq!(view.owned.len(), 2);
        assert_eq!(view.watching.len(), 1);
        assert_eq!(view.owned[0]["relationship"], json!("owned"));
        assert_eq!(view.owned[0]["critical"], json!(true));
    }

    #[test]
    fn critical_owned_failure_fails_scope() {
        let view = aggregate(
            &manifest(),
            vec![op(
                "managed-speech-asr",
                json!({"kind":"simple","state":"failed","sub":"failed"}),
            )],
            vec![],
        );
        assert_eq!(view.health, ScopeHealth::Failed);
        assert_eq!(view.attention[0].relationship, "owned");
        assert_eq!(view.attention[0].reason, "failed");
        assert_eq!(view.attention[0].code, "operation_failed");
    }

    #[test]
    fn noncritical_owned_failure_degrades() {
        let mut m = manifest();
        m.owned.push("managed-speech-aux-*".into());
        let view = aggregate(
            &m,
            vec![
                op(
                    "managed-speech-asr",
                    json!({"kind":"simple","state":"active","sub":"running"}),
                ),
                op(
                    "managed-speech-aux-log",
                    json!({"kind":"simple","state":"failed","sub":"failed"}),
                ),
            ],
            vec![],
        );
        assert_eq!(view.health, ScopeHealth::Degraded);
        assert_eq!(view.attention[0].operation, "managed-speech-aux-log");
        assert_eq!(view.owned[1]["critical"], json!(false));
    }

    #[test]
    fn watched_failure_degrades_as_watching() {
        let view = aggregate(
            &manifest(),
            vec![op(
                "managed-speech-asr",
                json!({"kind":"simple","state":"active","sub":"running"}),
            )],
            vec![op(
                "managed-proxy-health",
                json!({"kind":"oneshot","activation":"timer","last_result":"exit-code","last":"2026-01-01T00:00:00Z"}),
            )],
        );
        assert_eq!(view.health, ScopeHealth::Degraded);
        assert_eq!(view.attention[0].relationship, "watching");
        assert_eq!(view.attention[0].operation, "managed-proxy-health");
        assert_eq!(view.attention[0].code, "operation_failed");
    }

    #[test]
    fn unknown_critical_is_unknown() {
        let view = aggregate(
            &manifest(),
            vec![op(
                "managed-speech-asr",
                json!({"kind":"oneshot","activation":"timer","last_result":Value::Null,"last":Value::Null}),
            )],
            vec![],
        );
        assert_eq!(view.health, ScopeHealth::Unknown);
        assert_eq!(view.attention[0].reason, "unknown");
        assert_eq!(view.attention[0].code, "operation_unknown");
    }

    #[test]
    fn same_scope_id_is_not_a_warning() {
        let root = tmp("prov");
        fs::write(root.join(LEGACY_MANIFEST), sample()).unwrap();
        let nested = root.join("src");
        fs::create_dir_all(&nested).unwrap();
        assert!(provenance_warning(
            Some(&root.to_string_lossy()),
            Some("speech"),
            Some(&nested.to_string_lossy()),
        )
        .is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn different_scope_warns() {
        let a = tmp("scope-a");
        let b = tmp("scope-b");
        fs::write(
            a.join(LEGACY_MANIFEST),
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\n",
        )
        .unwrap();
        fs::write(
            b.join(LEGACY_MANIFEST),
            "[scope]\nid=\"personal\"\nowned=[\"managed-personal-*\"]\n",
        )
        .unwrap();
        let w = provenance_warning(
            Some(&a.to_string_lossy()),
            Some("speech"),
            Some(&b.to_string_lossy()),
        )
        .unwrap();
        assert!(w.contains("cross-scope"), "{w}");
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn no_scope_falls_back_to_cwd() {
        let w = provenance_warning(Some("/a"), None, Some("/b")).unwrap();
        assert!(w.contains("cross-context"), "{w}");
        assert!(provenance_warning(Some("/a"), None, Some("/a")).is_none());
    }
}
