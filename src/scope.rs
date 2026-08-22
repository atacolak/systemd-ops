//! Project-local responsibility scopes.
//!
//! `.systemd-ops.toml` is discovered by walking upward from cwd. The
//! directory that contains it is the scope root. ScopeView is derived
//! on demand; CLI `--json` and the TUI both call [`show`].

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::operations;
use crate::systemd::{self, BackendError};

const MANIFEST_NAME: &str = ".systemd-ops.toml";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeManifest {
    pub id: String,
    pub root: PathBuf,
    pub owned: Vec<String>,
    pub critical: Vec<String>,
    pub watch: Vec<String>,
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
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct ScopeView {
    pub id: String,
    pub root: PathBuf,
    pub health: ScopeHealth,
    pub owned: Vec<Value>,
    pub watching: Vec<Value>,
    pub attention: Vec<Attention>,
}

impl ScopeView {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "root": self.root.to_string_lossy(),
            "health": self.health.as_str(),
            "owned": self.owned,
            "watching": self.watching,
            "attention": self.attention.iter().map(|a| json!({
                "operation": a.operation,
                "relationship": a.relationship,
                "reason": a.reason,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Deserialize, Default)]
struct RawFile {
    scope: Option<RawScope>,
    critical: Option<Vec<String>>,
    watch: Option<Vec<RawWatch>>,
}

#[derive(Deserialize, Default)]
struct RawScope {
    id: Option<String>,
    owned: Option<Vec<String>>,
    critical: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct RawWatch {
    operation: Option<String>,
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
        let candidate = dir.join(MANIFEST_NAME);
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate)
                .map_err(|e| BackendError(format!("cannot read {}: {e}", candidate.display())))?;
            return parse_manifest(&text, dir);
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => {
                return Err(BackendError(format!(
                    "no {MANIFEST_NAME} found walking up from the working directory; this is a per-project console, not an all-systemd dashboard"
                )))
            }
        }
    }
}

pub fn discover_from_cwd(cwd: Option<&str>) -> Result<ScopeManifest, BackendError> {
    match cwd {
        Some(c) => discover(Path::new(c)),
        None => {
            let here = std::env::current_dir()
                .map_err(|e| BackendError(format!("cannot resolve cwd: {e}")))?;
            discover(&here)
        }
    }
}

pub fn parse_manifest(text: &str, root: PathBuf) -> Result<ScopeManifest, BackendError> {
    let raw: RawFile = toml::from_str(text)
        .map_err(|e| BackendError(format!("malformed {MANIFEST_NAME}: {e}")))?;
    let scope = raw
        .scope
        .ok_or_else(|| BackendError(format!("{MANIFEST_NAME} is missing [scope]")))?;
    let id = scope
        .id
        .ok_or_else(|| BackendError(format!("{MANIFEST_NAME} is missing scope.id")))?;
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
    let mut critical = raw.critical.unwrap_or_default();
    if let Some(extra) = scope.critical {
        critical.extend(extra);
    }
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
    Ok(ScopeManifest {
        id,
        root,
        owned,
        critical,
        watch,
    })
}

pub fn validate(cwd: Option<&str>) -> Result<Value, BackendError> {
    let manifest = discover_from_cwd(cwd)?;
    Ok(json!({
        "ok": true,
        "id": manifest.id,
        "root": manifest.root.to_string_lossy(),
        "owned": manifest.owned,
        "critical": manifest.critical,
        "watch": manifest.watch,
    }))
}

pub fn show(cwd: Option<&str>) -> Result<ScopeView, BackendError> {
    let manifest = discover_from_cwd(cwd)?;
    show_manifest(&manifest)
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
    let mut owned = Vec::new();
    for mut view in owned_ops {
        let unit = view
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let critical = manifest.critical.iter().any(|c| c == &unit);
        let op_health = operations::operation_health(&view);
        view["health"] = json!(op_health);
        view["relationship"] = json!("owned");
        view["critical"] = json!(critical);
        match op_health {
            "failed" if critical => {
                health = health.raise(ScopeHealth::Failed);
                attention.push(Attention {
                    operation: unit,
                    relationship: "owned",
                    reason: "failed".into(),
                });
            }
            "failed" => {
                health = health.raise(ScopeHealth::Degraded);
                attention.push(Attention {
                    operation: unit,
                    relationship: "owned",
                    reason: "failed".into(),
                });
            }
            "unknown" if critical => {
                health = health.raise(ScopeHealth::Unknown);
                attention.push(Attention {
                    operation: unit,
                    relationship: "owned",
                    reason: "unknown".into(),
                });
            }
            _ => {}
        }
        owned.push(view);
    }
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
        if op_health == "failed" {
            health = health.raise(ScopeHealth::Degraded);
            attention.push(Attention {
                operation: unit.clone(),
                relationship: "watching",
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
        root: manifest.root.clone(),
        health,
        owned,
        watching,
        attention,
    }
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
    fn walks_up_from_nested_cwd() {
        let root = tmp("walk");
        fs::write(root.join(MANIFEST_NAME), sample()).unwrap();
        let nested = root.join("src").join("bin");
        fs::create_dir_all(&nested).unwrap();
        let m = discover(&nested).unwrap();
        assert_eq!(m.id, "speech");
        assert_eq!(m.root, root);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let dir = tmp("none");
        let err = discover(&dir).unwrap_err();
        assert!(err.0.contains(MANIFEST_NAME), "got {}", err.0);
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
                json!({"kind":"oneshot","activation":"timer","last_result":"success","last":"2026-01-01T00:00:00Z"}),
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
    }

    #[test]
    fn same_scope_id_is_not_a_warning() {
        let root = tmp("prov");
        fs::write(root.join(MANIFEST_NAME), sample()).unwrap();
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
            a.join(MANIFEST_NAME),
            "[scope]\nid=\"speech\"\nowned=[\"managed-speech-*\"]\n",
        )
        .unwrap();
        fs::write(
            b.join(MANIFEST_NAME),
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
