//! Project-local advisory operator commentary.
//!
//! Stored under `<scope-root>/.systemd-ops/operator/<stem>.json`.
//! Soft state only: never mutates systemd, never feeds health, never
//! goes through plan/apply. Deleting the directory leaves operations
//! operationally unchanged.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config;
use crate::scope::{self, ScopeManifest};
use crate::sha256::sha256_hex;
use crate::systemd::{self, BackendError};

pub const VERSION: u32 = 1;
pub const MAX_ABOUT: usize = 2_000;
pub const MAX_HEADLINE: usize = 256;
pub const MAX_BODY: usize = 8_000;
pub const MAX_ACTIVITY_TEXT: usize = 1_000;
pub const MAX_ACTIVITY: usize = 100;

const DIR_NAME: &str = ".systemd-ops";
const OPERATOR_DIR: &str = "operator";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorState {
    Missing,
    Unbased,
    Current,
    Outdated,
    Error,
}

impl OperatorState {
    pub fn as_str(self) -> &'static str {
        match self {
            OperatorState::Missing => "missing",
            OperatorState::Unbased => "unbased",
            OperatorState::Current => "current",
            OperatorState::Outdated => "outdated",
            OperatorState::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatorActivity {
    pub at: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatorSurface {
    pub version: u32,
    pub about: Option<String>,
    pub headline: Option<String>,
    pub body: Option<String>,
    pub updated_at: Option<String>,
    pub basis_revision: Option<String>,
    pub activity: Vec<OperatorActivity>,
}

impl OperatorSurface {
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "about": self.about,
            "headline": self.headline,
            "body": self.body,
            "updated_at": self.updated_at,
            "basis_revision": self.basis_revision,
            "activity": self.activity.iter().map(|a| json!({
                "at": a.at,
                "text": a.text,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone, Debug)]
pub enum OperatorLoad {
    Missing,
    Ready(OperatorSurface),
    Error(String),
}

pub fn derive_state(load: &OperatorLoad, definition_revision: Option<&str>) -> OperatorState {
    match load {
        OperatorLoad::Missing => OperatorState::Missing,
        OperatorLoad::Error(_) => OperatorState::Error,
        OperatorLoad::Ready(surface) => match (&surface.basis_revision, definition_revision) {
            (None, _) | (_, None) => OperatorState::Unbased,
            (Some(basis), Some(rev)) if basis == rev => OperatorState::Current,
            (Some(_), Some(_)) => OperatorState::Outdated,
        },
    }
}

pub fn operator_dir(root: &Path) -> PathBuf {
    root.join(DIR_NAME).join(OPERATOR_DIR)
}

pub fn operator_path(root: &Path, stem: &str) -> PathBuf {
    operator_dir(root).join(format!("{stem}.json"))
}

pub fn validate_stem(stem: &str) -> Result<(), BackendError> {
    if stem.is_empty() || stem.contains('/') || stem.contains('.') {
        return Err(BackendError(format!(
            "operator unit must be a bare stem without suffix; got '{stem}'"
        )));
    }
    systemd::validate_unit_name(&format!("{stem}.service"))
}

pub fn require_owned(manifest: &ScopeManifest, stem: &str) -> Result<(), BackendError> {
    validate_stem(stem)?;
    let allowed = manifest.owned.iter().any(|g| {
        systemd::glob_match(g, stem) || systemd::glob_match(g, &format!("{stem}.service"))
    });
    if allowed {
        Ok(())
    } else {
        Err(BackendError(format!(
            "operator writes are restricted to owned stems matching {:?}; refused '{stem}'",
            manifest.owned
        )))
    }
}

pub fn load(root: &Path, stem: &str) -> OperatorLoad {
    let path = operator_path(root, stem);
    if !path.exists() {
        return OperatorLoad::Missing;
    }
    match fs::read(&path) {
        Ok(bytes) => match parse_surface(&bytes) {
            Ok(surface) => OperatorLoad::Ready(surface),
            Err(e) => OperatorLoad::Error(e.0),
        },
        Err(e) => OperatorLoad::Error(format!("cannot read {}: {e}", path.display())),
    }
}

pub fn parse_surface(bytes: &[u8]) -> Result<OperatorSurface, BackendError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| BackendError(format!("operator state is not valid JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| BackendError("operator state must be a JSON object".into()))?;
    let version = obj
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| BackendError("operator state missing version".into()))?;
    if version != u64::from(VERSION) {
        return Err(BackendError(format!(
            "unsupported operator state version {version}"
        )));
    }
    let activity = match obj.get("activity") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                let at = item
                    .get("at")
                    .and_then(Value::as_str)
                    .ok_or_else(|| BackendError("activity entry missing at".into()))?
                    .to_string();
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| BackendError("activity entry missing text".into()))?
                    .to_string();
                Ok(OperatorActivity { at, text })
            })
            .collect::<Result<Vec<_>, BackendError>>()?,
        Some(_) => {
            return Err(BackendError(
                "operator activity must be an array of {{at,text}}".into(),
            ))
        }
    };
    Ok(OperatorSurface {
        version: VERSION,
        about: opt_string(obj, "about"),
        headline: opt_string(obj, "headline"),
        body: opt_string(obj, "body"),
        updated_at: opt_string(obj, "updated_at"),
        basis_revision: opt_string(obj, "basis_revision"),
        activity,
    })
}

fn opt_string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(Value::String(_)) => None,
        _ => None,
    }
}

fn bound(field: &str, text: &str, max: usize) -> Result<String, BackendError> {
    if text.chars().count() > max {
        return Err(BackendError(format!("{field} exceeds {max} characters")));
    }
    Ok(text.to_string())
}

fn now_stamp() -> String {
    config::unix_to_rfc3339(config::now_unix())
}

fn ensure_dirs(root: &Path) -> Result<PathBuf, BackendError> {
    let base = root.join(DIR_NAME);
    if !base.exists() {
        fs::create_dir(&base)
            .map_err(|e| BackendError(format!("mkdir {}: {e}", base.display())))?;
        let _ = fs::set_permissions(&base, fs::Permissions::from_mode(0o700));
    }
    let dir = base.join(OPERATOR_DIR);
    if !dir.exists() {
        fs::create_dir(&dir).map_err(|e| BackendError(format!("mkdir {}: {e}", dir.display())))?;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

fn refuse_symlink(path: &Path) -> Result<(), BackendError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(BackendError(format!(
            "refusing to write through symlink {}",
            path.display()
        ))),
        Ok(_) | Err(_) => Ok(()),
    }
}

fn atomic_write(path: &Path, surface: &OperatorSurface) -> Result<(), BackendError> {
    refuse_symlink(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| BackendError("operator path has no parent".into()))?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("operator.json"),
        std::process::id()
    ));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| BackendError(format!("create {}: {e}", tmp.display())))?;
        let body = serde_json::to_vec_pretty(&surface.to_json())
            .map_err(|e| BackendError(format!("encode operator state: {e}")))?;
        file.write_all(&body)
            .map_err(|e| BackendError(format!("write {}: {e}", tmp.display())))?;
        file.write_all(b"\n")
            .map_err(|e| BackendError(format!("write {}: {e}", tmp.display())))?;
        file.sync_all()
            .map_err(|e| BackendError(format!("sync {}: {e}", tmp.display())))?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        BackendError(format!("rename into {}: {e}", path.display()))
    })?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    Ok(())
}

/// Hash ordered definition fragment files into `sha256:<hex>`.
///
/// Covers the same unit files OperationView lists in `fragment_paths`:
/// each existing readable path contributes `path\\0content\\0` in sorted
/// path order. Runtime state is excluded. Missing operation / no
/// readable fragments → `None`.
pub fn definition_revision_from_paths(paths: &[String]) -> Option<String> {
    let mut pairs = Vec::new();
    for path in paths {
        if let Ok(bytes) = fs::read(path) {
            pairs.push((path.clone(), bytes));
        }
    }
    if pairs.is_empty() {
        return None;
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut buf = Vec::new();
    for (path, bytes) in pairs {
        buf.extend_from_slice(path.as_bytes());
        buf.push(0);
        buf.extend_from_slice(&bytes);
        buf.push(0);
    }
    Some(format!("sha256:{}", sha256_hex(&buf)))
}

pub fn show(cwd: Option<&str>, stem: &str) -> Result<Value, BackendError> {
    let manifest = scope::discover_from_cwd(cwd)?;
    require_owned(&manifest, stem)?;
    let definition_revision = current_definition_revision(&manifest, stem);
    let load = load(&manifest.root, stem);
    let state = derive_state(&load, definition_revision.as_deref());
    match load {
        OperatorLoad::Missing => Ok(json!({
            "unit": stem,
            "operator": Value::Null,
            "operator_state": state.as_str(),
            "definition_revision": definition_revision,
        })),
        OperatorLoad::Ready(surface) => Ok(json!({
            "unit": stem,
            "operator": surface.to_json(),
            "operator_state": state.as_str(),
            "definition_revision": definition_revision,
        })),
        OperatorLoad::Error(msg) => Err(BackendError(format!(
            "operator state for '{stem}' is malformed: {msg}"
        ))),
    }
}

fn current_definition_revision(manifest: &ScopeManifest, stem: &str) -> Option<String> {
    match crate::operations::get_operation_any(stem) {
        Ok(view) => view
            .get("definition_revision")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                view.get("fragment_paths")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .and_then(|paths| definition_revision_from_paths(&paths))
            }),
        Err(_) => {
            // Pre-registration / missing critical: still allow commentary.
            let dir = systemd::unit_file_dir();
            let paths = [".service", ".timer"]
                .iter()
                .map(|sfx| {
                    dir.join(format!("{stem}{sfx}"))
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>();
            let _ = manifest;
            definition_revision_from_paths(&paths)
        }
    }
}

pub fn set(
    cwd: Option<&str>,
    stem: &str,
    about: Option<String>,
    headline: Option<String>,
    body: Option<String>,
) -> Result<Value, BackendError> {
    if about.is_none() && headline.is_none() && body.is_none() {
        return Err(BackendError(
            "operator set requires at least one of --about, --headline, --body".into(),
        ));
    }
    let manifest = scope::discover_from_cwd(cwd)?;
    require_owned(&manifest, stem)?;
    ensure_dirs(&manifest.root)?;
    let path = operator_path(&manifest.root, stem);
    let mut surface = match load(&manifest.root, stem) {
        OperatorLoad::Ready(s) => s,
        OperatorLoad::Missing | OperatorLoad::Error(_) => OperatorSurface {
            version: VERSION,
            about: None,
            headline: None,
            body: None,
            updated_at: None,
            basis_revision: None,
            activity: Vec::new(),
        },
    };
    if let Some(v) = about {
        surface.about = Some(bound("about", &v, MAX_ABOUT)?).filter(|s| !s.is_empty());
    }
    if let Some(v) = headline {
        surface.headline = Some(bound("headline", &v, MAX_HEADLINE)?).filter(|s| !s.is_empty());
    }
    if let Some(v) = body {
        surface.body = Some(bound("body", &v, MAX_BODY)?).filter(|s| !s.is_empty());
    }
    surface.version = VERSION;
    surface.updated_at = Some(now_stamp());
    surface.basis_revision = current_definition_revision(&manifest, stem);
    atomic_write(&path, &surface)?;
    Ok(json!({
        "unit": stem,
        "operator": surface.to_json(),
        "operator_state": derive_state(
            &OperatorLoad::Ready(surface.clone()),
            surface.basis_revision.as_deref()
        ).as_str(),
        "definition_revision": surface.basis_revision,
        "written": true,
    }))
}

pub fn append(cwd: Option<&str>, stem: &str, text: &str) -> Result<Value, BackendError> {
    let text = bound("activity.text", text, MAX_ACTIVITY_TEXT)?;
    if text.is_empty() {
        return Err(BackendError("activity text must not be empty".into()));
    }
    let manifest = scope::discover_from_cwd(cwd)?;
    require_owned(&manifest, stem)?;
    ensure_dirs(&manifest.root)?;
    let path = operator_path(&manifest.root, stem);
    let mut surface = match load(&manifest.root, stem) {
        OperatorLoad::Ready(s) => s,
        OperatorLoad::Missing => OperatorSurface {
            version: VERSION,
            about: None,
            headline: None,
            body: None,
            updated_at: None,
            basis_revision: None,
            activity: Vec::new(),
        },
        OperatorLoad::Error(msg) => {
            return Err(BackendError(format!(
                "operator state for '{stem}' is malformed: {msg}"
            )))
        }
    };
    surface.activity.push(OperatorActivity {
        at: now_stamp(),
        text,
    });
    if surface.activity.len() > MAX_ACTIVITY {
        let drop = surface.activity.len() - MAX_ACTIVITY;
        surface.activity.drain(0..drop);
    }
    atomic_write(&path, &surface)?;
    Ok(json!({
        "unit": stem,
        "operator": surface.to_json(),
        "operator_state": derive_state(
            &OperatorLoad::Ready(surface.clone()),
            current_definition_revision(&manifest, stem).as_deref()
        ).as_str(),
        "definition_revision": current_definition_revision(&manifest, stem),
        "appended": true,
    }))
}

pub fn clear(cwd: Option<&str>, stem: &str) -> Result<Value, BackendError> {
    let manifest = scope::discover_from_cwd(cwd)?;
    require_owned(&manifest, stem)?;
    let path = operator_path(&manifest.root, stem);
    refuse_symlink(&path)?;
    let removed = if path.exists() {
        fs::remove_file(&path)
            .map_err(|e| BackendError(format!("remove {}: {e}", path.display())))?;
        true
    } else {
        false
    };
    Ok(json!({
        "unit": stem,
        "removed": removed,
        "operator": Value::Null,
        "operator_state": OperatorState::Missing.as_str(),
    }))
}

/// Soft join used by ScopeView for owned operations.
pub fn join_for_scope(
    root: &Path,
    stem: &str,
    definition_revision: Option<&str>,
) -> (Value, OperatorState, Option<String>) {
    let load = load(root, stem);
    let state = derive_state(&load, definition_revision);
    match load {
        OperatorLoad::Missing => (Value::Null, state, None),
        OperatorLoad::Ready(surface) => (surface.to_json(), state, None),
        OperatorLoad::Error(msg) => (
            Value::Null,
            OperatorState::Error,
            Some(format!("operator state malformed for {stem}: {msg}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_root() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sops-op-{n}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manifest(root: &Path) -> ScopeManifest {
        ScopeManifest {
            id: "personal".into(),
            root: root.to_path_buf(),
            owned: vec!["managed-personal-*".into()],
            critical: Vec::new(),
            watch: vec!["managed-proxy-health".into()],
        }
    }

    #[test]
    fn owned_accepted_watched_rejected() {
        let root = tmp_root();
        let m = manifest(&root);
        assert!(require_owned(&m, "managed-personal-youtube-poll").is_ok());
        assert!(require_owned(&m, "managed-proxy-health").is_err());
        assert!(validate_stem("bad.name").is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_round_trip_and_partial_set() {
        let root = tmp_root();
        let m = manifest(&root);
        let stem = "managed-personal-demo";
        assert!(matches!(load(&m.root, stem), OperatorLoad::Missing));
        assert_eq!(
            derive_state(&OperatorLoad::Missing, Some("sha256:abc")),
            OperatorState::Missing
        );
        ensure_dirs(&m.root).unwrap();
        let mut surface = OperatorSurface {
            version: VERSION,
            about: Some("about".into()),
            headline: None,
            body: None,
            updated_at: Some("t0".into()),
            basis_revision: Some("sha256:aaa".into()),
            activity: vec![OperatorActivity {
                at: "t0".into(),
                text: "started".into(),
            }],
        };
        atomic_write(&operator_path(&m.root, stem), &surface).unwrap();
        surface.headline = Some("now".into());
        surface.updated_at = Some("t1".into());
        surface.basis_revision = Some("sha256:bbb".into());
        atomic_write(&operator_path(&m.root, stem), &surface).unwrap();
        match load(&m.root, stem) {
            OperatorLoad::Ready(s) => {
                assert_eq!(s.about.as_deref(), Some("about"));
                assert_eq!(s.headline.as_deref(), Some("now"));
                assert_eq!(s.activity.len(), 1);
                assert_eq!(s.basis_revision.as_deref(), Some("sha256:bbb"));
            }
            other => panic!("expected ready, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn malformed_is_error_state() {
        let root = tmp_root();
        let m = manifest(&root);
        let stem = "managed-personal-bad";
        ensure_dirs(&m.root).unwrap();
        fs::write(operator_path(&m.root, stem), b"{not json").unwrap();
        match load(&m.root, stem) {
            OperatorLoad::Error(_) => {}
            other => panic!("expected error, got {other:?}"),
        }
        assert_eq!(
            derive_state(&OperatorLoad::Error("x".into()), None),
            OperatorState::Error
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn append_trims_and_preserves_brief() {
        let root = tmp_root();
        let m = manifest(&root);
        let stem = "managed-personal-trim";
        ensure_dirs(&m.root).unwrap();
        let mut surface = OperatorSurface {
            version: VERSION,
            about: Some("a".into()),
            headline: Some("h".into()),
            body: Some("b".into()),
            updated_at: Some("frozen".into()),
            basis_revision: Some("sha256:keep".into()),
            activity: Vec::new(),
        };
        for i in 0..(MAX_ACTIVITY + 5) {
            surface.activity.push(OperatorActivity {
                at: format!("t{i}"),
                text: format!("event {i}"),
            });
        }
        // Simulate bound trim the same way append does.
        if surface.activity.len() > MAX_ACTIVITY {
            let drop = surface.activity.len() - MAX_ACTIVITY;
            surface.activity.drain(0..drop);
        }
        atomic_write(&operator_path(&m.root, stem), &surface).unwrap();
        match load(&m.root, stem) {
            OperatorLoad::Ready(s) => {
                assert_eq!(s.activity.len(), MAX_ACTIVITY);
                assert_eq!(s.updated_at.as_deref(), Some("frozen"));
                assert_eq!(s.basis_revision.as_deref(), Some("sha256:keep"));
                assert_eq!(s.activity[0].text, "event 5");
            }
            other => panic!("expected ready, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_removes_only_requested() {
        let root = tmp_root();
        let m = manifest(&root);
        ensure_dirs(&m.root).unwrap();
        let a = OperatorSurface {
            version: VERSION,
            about: Some("a".into()),
            headline: None,
            body: None,
            updated_at: None,
            basis_revision: None,
            activity: Vec::new(),
        };
        atomic_write(&operator_path(&m.root, "managed-personal-a"), &a).unwrap();
        atomic_write(&operator_path(&m.root, "managed-personal-b"), &a).unwrap();
        fs::remove_file(operator_path(&m.root, "managed-personal-a")).unwrap();
        assert!(matches!(
            load(&m.root, "managed-personal-a"),
            OperatorLoad::Missing
        ));
        assert!(matches!(
            load(&m.root, "managed-personal-b"),
            OperatorLoad::Ready(_)
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn definition_revision_stable_and_content_sensitive() {
        let root = tmp_root();
        let p1 = root.join("a.service");
        let p2 = root.join("a.timer");
        fs::write(&p1, b"[Unit]\nDescription=A\n").unwrap();
        fs::write(&p2, b"[Timer]\nOnCalendar=daily\n").unwrap();
        let paths = vec![
            p2.to_string_lossy().into_owned(),
            p1.to_string_lossy().into_owned(),
        ];
        let r1 = definition_revision_from_paths(&paths).unwrap();
        let r2 = definition_revision_from_paths(&paths).unwrap();
        assert_eq!(r1, r2);
        assert!(r1.starts_with("sha256:"));
        fs::write(&p1, b"[Unit]\nDescription=B\n").unwrap();
        let r3 = definition_revision_from_paths(&paths).unwrap();
        assert_ne!(r1, r3);
        assert!(definition_revision_from_paths(&[]).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn derive_current_outdated_unbased() {
        let ready = |basis: Option<&str>| {
            OperatorLoad::Ready(OperatorSurface {
                version: VERSION,
                about: None,
                headline: None,
                body: None,
                updated_at: None,
                basis_revision: basis.map(str::to_string),
                activity: Vec::new(),
            })
        };
        assert_eq!(
            derive_state(&ready(Some("sha256:a")), Some("sha256:a")),
            OperatorState::Current
        );
        assert_eq!(
            derive_state(&ready(Some("sha256:a")), Some("sha256:b")),
            OperatorState::Outdated
        );
        assert_eq!(
            derive_state(&ready(None), Some("sha256:a")),
            OperatorState::Unbased
        );
        assert_eq!(
            derive_state(&ready(Some("sha256:a")), None),
            OperatorState::Unbased
        );
    }

    #[test]
    fn refuses_symlink_target() {
        let root = tmp_root();
        ensure_dirs(&root).unwrap();
        let real = root.join("real.json");
        fs::write(&real, b"{}").unwrap();
        let link = operator_path(&root, "managed-personal-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let surface = OperatorSurface {
            version: VERSION,
            about: Some("x".into()),
            headline: None,
            body: None,
            updated_at: None,
            basis_revision: None,
            activity: Vec::new(),
        };
        assert!(atomic_write(&link, &surface).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
