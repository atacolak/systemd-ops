//! Project-local advisory operator commentary.
//!
//! Stored under `<scope-root>/.systemd-ops/operations/<stem>/state/operator.json`.

//! The former `.systemd-ops/operator/<stem>.json` location is read-only
//! compatibility and is migrated after the next successful write.
//! Soft state never feeds objective operation or scope health.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::scope::{self, ScopeManifest};
use crate::sha256::sha256_hex;
use crate::systemd::{self, BackendError};

pub const VERSION: u32 = 1;
pub const MAX_ABOUT: usize = 2_000;
pub const MAX_HEADLINE: usize = 256;
pub const MAX_BODY: usize = 8_000;
pub const MAX_ACTIVITY_TEXT: usize = 1_000;
pub const MAX_ACTIVITY: usize = 100;
pub const MAX_ITERATIONS: usize = 20;
pub const MAX_AUTOMATION_HEADLINE: usize = 80;
pub const MIN_AUTOMATION_SUMMARY_ITEMS: usize = 1;
pub const MAX_AUTOMATION_SUMMARY_ITEMS: usize = 5;
pub const MAX_AUTOMATION_SUMMARY_ITEM: usize = 280;
pub const MAX_AUTOMATION_ACTIVITY: usize = 200;

const DIR_NAME: &str = ".systemd-ops";
const LEGACY_OPERATOR_DIR: &str = "operator";

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
pub struct ActiveIteration {
    pub id: String,
    pub started_at: String,
    pub observed_updated_at: Option<String>,
    pub reported_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperatorIteration {
    pub id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub exit_code: Option<i32>,
    pub reconsolidated: bool,
    pub headline: Option<String>,
    pub summary: Option<String>,
    pub outcome: Option<String>,
    pub route: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperatorSurface {
    pub version: u32,
    pub about: Option<String>,
    pub headline: Option<String>,
    pub body: Option<String>,
    pub updated_at: Option<String>,
    pub basis_revision: Option<String>,
    pub outcome: Option<String>,
    pub route: Option<String>,
    pub activity: Vec<OperatorActivity>,
    pub active_iteration: Option<ActiveIteration>,
    pub iterations: Vec<OperatorIteration>,
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
            "outcome": self.outcome,
            "route": self.route,
            "activity": self.activity.iter().map(|a| json!({
                "at": a.at,
                "text": a.text,
            })).collect::<Vec<_>>(),
            "active_iteration": self.active_iteration.as_ref().map(|iteration| json!({
                "id": iteration.id,
                "started_at": iteration.started_at,
                "observed_updated_at": iteration.observed_updated_at,
                "reported_at": iteration.reported_at,
            })),
            "iterations": self.iterations.iter().map(|iteration| json!({
                "id": iteration.id,
                "started_at": iteration.started_at,
                "finished_at": iteration.finished_at,
                "exit_code": iteration.exit_code,
                "reconsolidated": iteration.reconsolidated,
                "headline": iteration.headline,
                "summary": iteration.summary,
                "outcome": iteration.outcome,
                "route": iteration.route,
            })).collect::<Vec<_>>(),
        })
    }
}

// Successful loads dominate this CLI-only path. Keep the value inline rather than
// allocating every parsed operator surface solely to shrink the error variant.
#[allow(clippy::large_enum_variant)]
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

pub fn operator_dir(root: &Path, stem: &str) -> PathBuf {
    crate::automation::operation_home(root, stem).join("state")
}

pub fn operator_path(root: &Path, stem: &str) -> PathBuf {
    operator_dir(root, stem).join("operator.json")
}

pub fn legacy_operator_path(root: &Path, stem: &str) -> PathBuf {
    root.join(DIR_NAME)
        .join(LEGACY_OPERATOR_DIR)
        .join(format!("{stem}.json"))
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

fn selected_path(root: &Path, stem: &str) -> Result<(PathBuf, Option<String>), BackendError> {
    let operation = crate::automation::operation_home_checked(root, stem)?;
    let canonical = operation.join("state/operator.json");
    let legacy = legacy_operator_path(root, stem);
    if canonical.exists() {
        let warning = legacy.exists().then(|| {
            format!(
                "canonical operator state {} wins over legacy {}",
                canonical.display(),
                legacy.display()
            )
        });
        Ok((canonical, warning))
    } else {
        Ok((legacy, None))
    }
}

pub fn load_with_warning(root: &Path, stem: &str) -> (OperatorLoad, Option<String>) {
    let (path, warning) = match selected_path(root, stem) {
        Ok(selected) => selected,
        Err(error) => return (OperatorLoad::Error(error.0), None),
    };
    if !path.exists() {
        return (OperatorLoad::Missing, warning);
    }
    let load = match fs::read(&path) {
        Ok(bytes) => match parse_surface(&bytes) {
            Ok(surface) => OperatorLoad::Ready(surface),
            Err(e) => OperatorLoad::Error(e.0),
        },
        Err(e) => OperatorLoad::Error(format!("cannot read {}: {e}", path.display())),
    };
    (load, warning)
}

pub fn load(root: &Path, stem: &str) -> OperatorLoad {
    load_with_warning(root, stem).0
}

pub fn parse_surface(bytes: &[u8]) -> Result<OperatorSurface, BackendError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| BackendError(format!("malformed operator state: {error}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| BackendError("operator state must be an object".into()))?;
    let activity = parse_activity(obj)?;
    let active_iteration = match obj.get("active_iteration") {
        None | Some(Value::Null) => None,
        Some(Value::Object(item)) => Some(ActiveIteration {
            id: required_string(item, "id", "active iteration")?,
            started_at: required_string(item, "started_at", "active iteration")?,
            observed_updated_at: opt_string(item, "observed_updated_at"),
            reported_at: opt_string(item, "reported_at"),
        }),
        Some(_) => {
            return Err(BackendError(
                "active_iteration must be an object or null".into(),
            ))
        }
    };
    let mut iterations = match obj.get("iterations") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(parse_iteration)
            .collect::<Result<Vec<_>, BackendError>>()?,
        Some(_) => return Err(BackendError("iterations must be an array".into())),
    };
    iterations.truncate(MAX_ITERATIONS);
    Ok(OperatorSurface {
        version: VERSION,
        about: opt_string(obj, "about"),
        headline: opt_string(obj, "headline"),
        body: opt_string(obj, "body"),
        updated_at: opt_string(obj, "updated_at"),
        basis_revision: opt_string(obj, "basis_revision"),
        outcome: opt_string(obj, "outcome"),
        route: opt_string(obj, "route"),
        activity,
        active_iteration,
        iterations,
    })
}

fn parse_activity(obj: &Map<String, Value>) -> Result<Vec<OperatorActivity>, BackendError> {
    match obj.get("activity") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                let item = item
                    .as_object()
                    .ok_or_else(|| BackendError("activity entry must be an object".into()))?;
                Ok(OperatorActivity {
                    at: required_string(item, "at", "activity entry")?,
                    text: required_string(item, "text", "activity entry")?,
                })
            })
            .collect(),
        Some(_) => Err(BackendError(
            "operator activity must be an array of {at,text}".into(),
        )),
    }
}

fn required_string(
    obj: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<String, BackendError> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| BackendError(format!("{context} missing {key}")))
}

fn parse_iteration(value: &Value) -> Result<OperatorIteration, BackendError> {
    let obj = value
        .as_object()
        .ok_or_else(|| BackendError("iteration entry must be an object".into()))?;
    let exit_code = match obj.get("exit_code") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| BackendError("iteration exit_code must be a 32-bit integer".into()))
            .map(Some)?,
        Some(_) => {
            return Err(BackendError(
                "iteration exit_code must be an integer or null".into(),
            ))
        }
    };
    Ok(OperatorIteration {
        id: required_string(obj, "id", "iteration")?,
        started_at: required_string(obj, "started_at", "iteration")?,
        finished_at: opt_string(obj, "finished_at"),
        exit_code,
        reconsolidated: obj
            .get("reconsolidated")
            .and_then(Value::as_bool)
            .ok_or_else(|| BackendError("iteration missing reconsolidated".into()))?,
        headline: opt_string(obj, "headline"),
        summary: opt_string(obj, "summary"),
        outcome: opt_string(obj, "outcome"),
        route: opt_string(obj, "route"),
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
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    systemd::usec_to_rfc3339(micros)
}

fn new_iteration_id() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("it-{nanos:032x}-{:08x}-{sequence:016x}", std::process::id())
}

fn ensure_dirs(root: &Path, stem: &str) -> Result<PathBuf, BackendError> {
    let base = root.join(DIR_NAME);
    refuse_symlink(&base)?;
    if !base.exists() {
        fs::create_dir(&base)
            .map_err(|e| BackendError(format!("mkdir {}: {e}", base.display())))?;
        let _ = fs::set_permissions(&base, fs::Permissions::from_mode(0o700));
    }
    let operations = base.join("operations");
    refuse_symlink(&operations)?;
    if !operations.exists() {
        fs::create_dir(&operations)
            .map_err(|e| BackendError(format!("mkdir {}: {e}", operations.display())))?;
        let _ = fs::set_permissions(&operations, fs::Permissions::from_mode(0o700));
    }
    let operation = crate::automation::operation_home_checked(root, stem)?;
    refuse_symlink(&operation)?;
    if !operation.exists() {
        fs::create_dir(&operation)
            .map_err(|e| BackendError(format!("mkdir {}: {e}", operation.display())))?;
        let _ = fs::set_permissions(&operation, fs::Permissions::from_mode(0o700));
    }
    let dir = operation.join("state");
    refuse_symlink(&dir)?;
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

fn empty_surface() -> OperatorSurface {
    OperatorSurface {
        version: VERSION,
        about: None,
        headline: None,
        body: None,
        updated_at: None,
        basis_revision: None,
        outcome: None,
        route: None,
        activity: Vec::new(),
        active_iteration: None,
        iterations: Vec::new(),
    }
}

fn resolved_manifest(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<ScopeManifest, BackendError> {
    let env_root = std::env::var("SYSTEMD_OPS_SCOPE_ROOT").ok();
    scope::resolve(explicit_root, env_root.as_deref(), cwd)
}
fn bound_operation() -> Result<String, BackendError> {
    let stem = std::env::var("SYSTEMD_OPS_OPERATION").map_err(|_| {
        BackendError("SYSTEMD_OPS_OPERATION is required for automation commands".into())
    })?;
    if stem.is_empty() {
        return Err(BackendError(
            "SYSTEMD_OPS_OPERATION is required for automation commands".into(),
        ));
    }
    validate_stem(&stem)?;
    Ok(stem)
}

pub fn bound_operation_manifest(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<(ScopeManifest, String), BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    let stem = bound_operation()?;
    require_owned(&manifest, &stem)?;
    Ok((manifest, stem))
}

pub fn strict_line(label: &str, value: &str, max: usize) -> Result<String, BackendError> {
    strict_single_line(label, value, max)
}

fn strict_single_line(label: &str, value: &str, max: usize) -> Result<String, BackendError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BackendError(format!("{label} must not be empty")));
    }
    if value.contains(['\n', '\r']) {
        return Err(BackendError(format!("{label} must be a single line")));
    }
    if value.chars().count() > max {
        return Err(BackendError(format!("{label} exceeds {max} characters")));
    }
    Ok(value.to_string())
}

fn strict_summary(items: &[String]) -> Result<Vec<String>, BackendError> {
    if !(MIN_AUTOMATION_SUMMARY_ITEMS..=MAX_AUTOMATION_SUMMARY_ITEMS).contains(&items.len()) {
        return Err(BackendError(format!(
            "summary must contain {MIN_AUTOMATION_SUMMARY_ITEMS}..{MAX_AUTOMATION_SUMMARY_ITEMS} paragraphs"
        )));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let item = item.trim();
            if item.is_empty() {
                return Err(BackendError(format!(
                    "summary paragraph {} must not be empty",
                    index + 1
                )));
            }
            if item.contains(['\n', '\r']) {
                return Err(BackendError(format!(
                    "summary paragraph {} must be one paragraph",
                    index + 1
                )));
            }
            if item.chars().count() > MAX_AUTOMATION_SUMMARY_ITEM {
                return Err(BackendError(format!(
                    "summary paragraph {} exceeds {} characters",
                    index + 1,
                    MAX_AUTOMATION_SUMMARY_ITEM
                )));
            }
            Ok(item.to_string())
        })
        .collect()
}

pub fn automation_context(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let (manifest, stem) = bound_operation_manifest(explicit_root, cwd)?;

    let view = crate::scope::show_manifest(&manifest)?;
    let operation = view
        .owned
        .iter()
        .find(|operation| operation.get("unit").and_then(Value::as_str) == Some(stem.as_str()))
        .ok_or_else(|| {
            BackendError(format!(
                "bound operation '{stem}' is not present in the scope"
            ))
        })?;
    let operator = operation.get("operator").cloned().unwrap_or(Value::Null);
    let automation = operation.get("automation").cloned().unwrap_or(Value::Null);
    let relations = operation.get("relations").cloned().unwrap_or_else(|| {
        json!({
            "parent": Value::Null,
            "children": [],
        })
    });
    Ok(json!({
        "scope": {
            "id": manifest.id,
            "root": manifest.root.to_string_lossy(),
        },
        "operation": {
            "unit": stem,
            "title": operation.get("title").cloned().unwrap_or(Value::Null),
            "purpose": operation.get("purpose").cloned().unwrap_or(Value::Null),
            "health": operation.get("health").cloned().unwrap_or(Value::Null),
            "operator_state": operation.get("operator_state").cloned().unwrap_or(Value::Null),
            "definition_revision": operation.get("definition_revision").cloned().unwrap_or(Value::Null),
        },
        "automation": {
            "agent": automation.get("agent").cloned().unwrap_or(Value::Null),
            "agent_root": automation.get("agent_root").cloned().unwrap_or(Value::Null),
            "brain_revision": automation.get("brain_revision").cloned().unwrap_or(Value::Null),
            "lifecycle": automation.get("lifecycle").cloned().unwrap_or(Value::Null),
            "parent": automation.get("parent").cloned().unwrap_or(Value::Null),
            "observation": automation.get("observation").cloned().unwrap_or(Value::Null),
            "processed": automation.get("processed").cloned().unwrap_or(Value::Null),
            "checkpoint": automation.get("checkpoint").cloned().unwrap_or(Value::Null),
            "blocker": automation.get("blocker").cloned().unwrap_or(Value::Null),
            "semantic_state": automation.get("semantic_state").cloned().unwrap_or(Value::Null),
        },

        "relations": relations,
        "runtime": {
            "state": operation.get("state").cloned().unwrap_or(Value::Null),
            "substate": operation.get("sub").cloned().unwrap_or(Value::Null),
            "last": operation.get("last").cloned().unwrap_or(Value::Null),
            "last_result": operation.get("last_result").cloned().unwrap_or(Value::Null),
            "next": operation.get("next").cloned().unwrap_or(Value::Null),
            "activation": operation.get("activation").cloned().unwrap_or(Value::Null),
            "kind": operation.get("kind").cloned().unwrap_or(Value::Null),
        },
        "current_report": {
            "headline": operator.get("headline").cloned().unwrap_or(Value::Null),
            "summary": operator.get("body").cloned().unwrap_or(Value::Null),
            "updated_at": operator.get("updated_at").cloned().unwrap_or(Value::Null),
            "basis_revision": operator.get("basis_revision").cloned().unwrap_or(Value::Null),
            "outcome": operator.get("outcome").cloned().unwrap_or(Value::Null),
            "route": operator.get("route").cloned().unwrap_or(Value::Null),
        },
        "active_iteration": operator.get("active_iteration").cloned().unwrap_or(Value::Null),
        "iterations": operator.get("iterations").cloned().unwrap_or_else(|| json!([])),
        "activity": operator.get("activity").cloned().unwrap_or_else(|| json!([])),
    }))
}

pub fn automation_report(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    headline: &str,
    summary: &[String],
    outcome: Option<&str>,
    route: Option<&str>,
) -> Result<Value, BackendError> {
    let headline = strict_single_line("headline", headline, MAX_AUTOMATION_HEADLINE)?;
    let summary = strict_summary(summary)?;
    let body = summary.join("\n\n");
    let (outcome, route) = validate_report_outcome(outcome, route)?;
    let (manifest, stem) = bound_operation_manifest(explicit_root, cwd)?;

    let (load, warning) = load_with_warning(&manifest.root, &stem);
    let mut surface = match load {
        OperatorLoad::Ready(surface) => surface,
        OperatorLoad::Missing => empty_surface(),
        OperatorLoad::Error(message) => {
            return Err(BackendError(format!(
                "operator state for '{stem}' is malformed: {message}"
            )))
        }
    };
    let now = now_stamp();
    let active = surface.active_iteration.as_mut().ok_or_else(|| {
        BackendError("automation_report requires an active agent iteration".into())
    })?;
    surface.headline = Some(headline);
    surface.body = Some(body);
    surface.outcome = outcome.clone();
    surface.route = route.clone();
    surface.updated_at = Some(now.clone());
    surface.basis_revision = current_definition_revision(&manifest, &stem);
    active.reported_at = Some(now);
    finish_write(&manifest.root, &stem, &surface)?;
    Ok(json!({
        "unit": stem,
        "operator": surface.to_json(),
        "definition_revision": surface.basis_revision,
        "warning": warning,
        "reported": true,
    }))
}

fn validate_report_outcome(
    outcome: Option<&str>,
    route: Option<&str>,
) -> Result<(Option<String>, Option<String>), BackendError> {
    let outcome = outcome
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BackendError("report requires outcome=ready|blocked".into()))?;
    let outcome = strict_single_line("outcome", outcome, 16)?;
    match outcome.as_str() {
        "ready" => {
            if route.is_some() {
                return Err(BackendError(
                    "READY reports must not include a route".into(),
                ));
            }
            Ok((Some(outcome), None))
        }
        "blocked" => {
            let route = route.ok_or_else(|| {
                BackendError("BLOCKED reports require route=self|parent|lead".into())
            })?;
            let route = strict_single_line("route", route, 16)?;
            if !matches!(route.as_str(), "self" | "parent" | "lead") {
                return Err(BackendError(
                    "unknown blocker route (known: self, parent, lead)".into(),
                ));
            }
            Ok((Some(outcome), Some(route)))
        }
        "failed" => Err(BackendError(
            "FAILED is a wrapper outcome, not an automation_report outcome".into(),
        )),
        other => Err(BackendError(format!(
            "unknown report outcome '{other}' (known: ready, blocked)"
        ))),
    }
}

pub fn automation_activity(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    text: &str,
) -> Result<Value, BackendError> {
    let text = strict_single_line("activity text", text, MAX_AUTOMATION_ACTIVITY)?;
    let (manifest, stem) = bound_operation_manifest(explicit_root, cwd)?;

    let (load, warning) = load_with_warning(&manifest.root, &stem);
    let mut surface = match load {
        OperatorLoad::Ready(surface) => surface,
        OperatorLoad::Missing => empty_surface(),
        OperatorLoad::Error(message) => {
            return Err(BackendError(format!(
                "operator state for '{stem}' is malformed: {message}"
            )))
        }
    };
    if surface.active_iteration.is_none() {
        return Err(BackendError(
            "automation_activity requires an active agent iteration".into(),
        ));
    }
    surface.activity.push(OperatorActivity {
        at: now_stamp(),
        text,
    });
    if surface.activity.len() > MAX_ACTIVITY {
        let drop = surface.activity.len() - MAX_ACTIVITY;
        surface.activity.drain(0..drop);
    }
    finish_write(&manifest.root, &stem, &surface)?;
    Ok(json!({
        "unit": stem,
        "operator": surface.to_json(),
        "warning": warning,
        "appended": true,
    }))
}

fn finish_write(root: &Path, stem: &str, surface: &OperatorSurface) -> Result<(), BackendError> {
    ensure_dirs(root, stem)?;
    let canonical = operator_path(root, stem);
    let legacy = legacy_operator_path(root, stem);
    refuse_symlink(&canonical)?;
    refuse_symlink(&legacy)?;
    atomic_write(&canonical, surface)?;
    if legacy.exists() {
        fs::remove_file(&legacy)
            .map_err(|e| BackendError(format!("remove migrated {}: {e}", legacy.display())))?;
    }
    Ok(())
}

fn push_iteration(surface: &mut OperatorSurface, iteration: OperatorIteration) {
    surface.iterations.insert(0, iteration);
    surface.iterations.truncate(MAX_ITERATIONS);
}

fn start_iteration(surface: &mut OperatorSurface, id: String, started_at: String) {
    if let Some(abandoned) = surface.active_iteration.take() {
        push_iteration(
            surface,
            OperatorIteration {
                id: abandoned.id,
                started_at: abandoned.started_at,
                finished_at: None,
                exit_code: None,
                reconsolidated: false,
                headline: None,
                summary: None,
                ..Default::default()
            },
        );
    }
    surface.active_iteration = Some(ActiveIteration {
        id,
        started_at,
        observed_updated_at: surface.updated_at.clone(),
        reported_at: None,
    });
}

fn finish_iteration(
    surface: &mut OperatorSurface,
    iteration_id: &str,
    exit_code: i32,
    finished_at: String,
) -> Result<bool, BackendError> {
    let active = surface
        .active_iteration
        .take()
        .ok_or_else(|| BackendError("no active iteration to finish".into()))?;
    if active.id != iteration_id {
        let actual = active.id.clone();
        surface.active_iteration = Some(active);
        return Err(BackendError(format!(
            "active iteration is '{actual}', not '{iteration_id}'"
        )));
    }
    let reconsolidated = exit_code == 0 && active.reported_at.is_some();
    let headline = reconsolidated.then(|| surface.headline.clone()).flatten();
    let summary = reconsolidated.then(|| surface.body.clone()).flatten();
    let outcome = reconsolidated.then(|| surface.outcome.clone()).flatten();
    let route = reconsolidated.then(|| surface.route.clone()).flatten();
    push_iteration(
        surface,
        OperatorIteration {
            id: active.id,
            started_at: active.started_at,
            finished_at: Some(finished_at),
            exit_code: Some(exit_code),
            reconsolidated,
            headline,
            summary,
            outcome,
            route,
        },
    );
    Ok(reconsolidated)
}
pub fn show(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    stem: &str,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    require_owned(&manifest, stem)?;
    let definition_revision = current_definition_revision(&manifest, stem);
    let (load, warning) = load_with_warning(&manifest.root, stem);
    let state = derive_state(&load, definition_revision.as_deref());
    match load {
        OperatorLoad::Missing => Ok(json!({
            "unit": stem,
            "operator": Value::Null,
            "operator_state": state.as_str(),
            "definition_revision": definition_revision,
            "warning": warning,
        })),
        OperatorLoad::Ready(surface) => Ok(json!({
            "unit": stem,
            "operator": surface.to_json(),
            "operator_state": state.as_str(),
            "definition_revision": definition_revision,
            "warning": warning,
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
    explicit_root: Option<&str>,
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
    let manifest = resolved_manifest(explicit_root, cwd)?;
    require_owned(&manifest, stem)?;
    let (load, warning) = load_with_warning(&manifest.root, stem);
    let mut surface = match load {
        OperatorLoad::Ready(s) => s,
        OperatorLoad::Missing | OperatorLoad::Error(_) => empty_surface(),
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
    finish_write(&manifest.root, stem, &surface)?;
    Ok(json!({
        "unit": stem,
        "operator": surface.to_json(),
        "operator_state": derive_state(
            &OperatorLoad::Ready(surface.clone()),
            surface.basis_revision.as_deref()
        ).as_str(),
        "definition_revision": surface.basis_revision,
        "warning": warning,
        "written": true,
    }))
}

pub fn append(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    stem: &str,
    text: &str,
) -> Result<Value, BackendError> {
    let text = bound("activity.text", text, MAX_ACTIVITY_TEXT)?;
    if text.is_empty() {
        return Err(BackendError("activity text must not be empty".into()));
    }
    let manifest = resolved_manifest(explicit_root, cwd)?;
    require_owned(&manifest, stem)?;
    let (load, warning) = load_with_warning(&manifest.root, stem);
    let mut surface = match load {
        OperatorLoad::Ready(s) => s,
        OperatorLoad::Missing => empty_surface(),
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
    finish_write(&manifest.root, stem, &surface)?;
    let definition_revision = current_definition_revision(&manifest, stem);
    Ok(json!({
        "unit": stem,
        "operator": surface.to_json(),
        "operator_state": derive_state(
            &OperatorLoad::Ready(surface.clone()),
            definition_revision.as_deref()
        ).as_str(),
        "definition_revision": definition_revision,
        "warning": warning,
        "appended": true,
    }))
}

pub fn iteration_start(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    stem: &str,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    require_owned(&manifest, stem)?;
    let (load, warning) = load_with_warning(&manifest.root, stem);
    let mut surface = match load {
        OperatorLoad::Ready(s) => s,
        OperatorLoad::Missing => empty_surface(),
        OperatorLoad::Error(msg) => {
            return Err(BackendError(format!(
                "operator state for '{stem}' is malformed: {msg}"
            )))
        }
    };
    let iteration_id = new_iteration_id();
    start_iteration(&mut surface, iteration_id.clone(), now_stamp());
    finish_write(&manifest.root, stem, &surface)?;
    Ok(json!({
        "unit": stem,
        "iteration_id": iteration_id,
        "operator": surface.to_json(),
        "warning": warning,
        "started": true,
    }))
}

pub fn iteration_finish(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    stem: &str,
    iteration_id: &str,
    exit_code: i32,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    require_owned(&manifest, stem)?;
    let (load, warning) = load_with_warning(&manifest.root, stem);
    let mut surface = match load {
        OperatorLoad::Ready(s) => s,
        OperatorLoad::Missing => return Err(BackendError("no active iteration to finish".into())),
        OperatorLoad::Error(msg) => {
            return Err(BackendError(format!(
                "operator state for '{stem}' is malformed: {msg}"
            )))
        }
    };
    let reconsolidated = finish_iteration(&mut surface, iteration_id, exit_code, now_stamp())?;
    finish_write(&manifest.root, stem, &surface)?;
    Ok(json!({
        "unit": stem,
        "iteration_id": iteration_id,
        "exit_code": exit_code,
        "reconsolidated": reconsolidated,
        "operator": surface.to_json(),
        "warning": warning,
        "finished": true,
    }))
}

pub fn clear(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    stem: &str,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    require_owned(&manifest, stem)?;
    let canonical = operator_path(&manifest.root, stem);
    let legacy = legacy_operator_path(&manifest.root, stem);
    refuse_symlink(&canonical)?;
    refuse_symlink(&legacy)?;
    let mut removed = false;
    for path in [&canonical, &legacy] {
        if path.exists() {
            fs::remove_file(path)
                .map_err(|e| BackendError(format!("remove {}: {e}", path.display())))?;
            removed = true;
        }
    }
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
    let (load, warning) = load_with_warning(root, stem);
    let state = derive_state(&load, definition_revision);
    match load {
        OperatorLoad::Missing => (Value::Null, state, warning),
        OperatorLoad::Ready(surface) => (surface.to_json(), state, warning),
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
            automation_agent_root: None,
            coordination_lead: None,
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
        ensure_dirs(&m.root, stem).unwrap();
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
            active_iteration: None,
            iterations: Vec::new(),
            ..Default::default()
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
        ensure_dirs(&m.root, stem).unwrap();
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
        ensure_dirs(&m.root, stem).unwrap();
        let mut surface = OperatorSurface {
            version: VERSION,
            about: Some("a".into()),
            headline: Some("h".into()),
            body: Some("b".into()),
            updated_at: Some("frozen".into()),
            basis_revision: Some("sha256:keep".into()),
            activity: Vec::new(),
            active_iteration: None,
            iterations: Vec::new(),
            ..Default::default()
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
        ensure_dirs(&m.root, "managed-personal-a").unwrap();
        ensure_dirs(&m.root, "managed-personal-b").unwrap();
        let a = OperatorSurface {
            version: VERSION,
            about: Some("a".into()),
            headline: None,
            body: None,
            updated_at: None,
            basis_revision: None,
            activity: Vec::new(),
            active_iteration: None,
            iterations: Vec::new(),
            ..Default::default()
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
                active_iteration: None,
                iterations: Vec::new(),
                ..Default::default()
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
    fn canonical_path_migration_and_coexistence_warning() {
        let root = tmp_root();
        let stem = "managed-personal-migrate";
        let legacy = legacy_operator_path(&root, stem);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let mut surface = empty_surface();
        surface.headline = Some("legacy".into());
        atomic_write(&legacy, &surface).unwrap();
        assert!(matches!(load(&root, stem), OperatorLoad::Ready(_)));
        finish_write(&root, stem, &surface).unwrap();
        assert!(operator_path(&root, stem).is_file());
        assert!(!legacy.exists());

        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, serde_json::to_vec(&surface.to_json()).unwrap()).unwrap();
        let (_, warning) = load_with_warning(&root, stem);
        assert!(warning.unwrap().contains("canonical operator state"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn owned_gate_precedes_directory_creation() {
        let root = tmp_root();
        let m = manifest(&root);
        assert!(require_owned(&m, "managed-other-nope").is_err());
        assert!(!operator_path(&root, "managed-other-nope").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn iteration_success_nonzero_and_no_reconsolidation() {
        let mut surface = empty_surface();
        surface.updated_at = Some("brief-0".into());
        surface.headline = Some("old headline".into());
        surface.body = Some("old summary".into());
        start_iteration(&mut surface, "it-a".into(), "start".into());
        assert!(!finish_iteration(&mut surface, "it-a", 0, "finish".into()).unwrap());
        assert_eq!(surface.iterations[0].exit_code, Some(0));
        assert!(!surface.iterations[0].reconsolidated);
        assert!(surface.iterations[0].headline.is_none());
        assert!(surface.iterations[0].summary.is_none());

        start_iteration(&mut surface, "it-b".into(), "start-2".into());
        surface.active_iteration.as_mut().unwrap().reported_at = Some("brief-1".into());
        assert!(!finish_iteration(&mut surface, "it-b", 17, "finish-2".into()).unwrap());
        assert_eq!(surface.iterations[0].exit_code, Some(17));
        assert!(!surface.iterations[0].reconsolidated);
        assert!(surface.iterations[0].headline.is_none());
        assert!(surface.iterations[0].summary.is_none());

        start_iteration(&mut surface, "it-c".into(), "start-3".into());
        surface.active_iteration.as_mut().unwrap().reported_at = Some("brief-2".into());
        assert!(finish_iteration(&mut surface, "it-c", 0, "finish-3".into()).unwrap());
        assert!(surface.iterations[0].reconsolidated);
        assert_eq!(
            surface.iterations[0].headline.as_deref(),
            Some("old headline")
        );

        start_iteration(&mut surface, "it-d".into(), "start-4".into());
        assert!(finish_iteration(&mut surface, "wrong", 0, "finish-4".into()).is_err());
        assert_eq!(surface.active_iteration.as_ref().unwrap().id, "it-d");
    }

    #[test]
    fn automation_report_schema_is_strict() {
        assert_eq!(
            strict_single_line("headline", " waiting ", MAX_AUTOMATION_HEADLINE).unwrap(),
            "waiting"
        );
        assert!(strict_single_line("headline", "", MAX_AUTOMATION_HEADLINE).is_err());
        assert!(strict_single_line("headline", "a\nb", MAX_AUTOMATION_HEADLINE).is_err());
        assert!(strict_single_line(
            "headline",
            &"x".repeat(MAX_AUTOMATION_HEADLINE + 1),
            MAX_AUTOMATION_HEADLINE
        )
        .is_err());

        assert_eq!(strict_summary(&[" one ".into()]).unwrap(), ["one"]);
        assert!(strict_summary(&[]).is_err());
        assert!(strict_summary(&vec!["x".into(); MAX_AUTOMATION_SUMMARY_ITEMS + 1]).is_err());
        assert!(strict_summary(&[" \t ".into()]).is_err());
        assert!(strict_summary(&["one\ntwo".into()]).is_err());
        assert!(strict_summary(&["x".repeat(MAX_AUTOMATION_SUMMARY_ITEM + 1)]).is_err());
    }

    #[test]
    fn automation_activity_schema_is_strict() {
        assert_eq!(
            strict_single_line("activity text", " milestone ", MAX_AUTOMATION_ACTIVITY).unwrap(),
            "milestone"
        );
        assert!(strict_single_line("activity text", "", MAX_AUTOMATION_ACTIVITY).is_err());
        assert!(strict_single_line("activity text", "a\rb", MAX_AUTOMATION_ACTIVITY).is_err());
        assert!(strict_single_line(
            "activity text",
            &"x".repeat(MAX_AUTOMATION_ACTIVITY + 1),
            MAX_AUTOMATION_ACTIVITY
        )
        .is_err());
    }

    #[test]
    fn report_outcome_requires_ready_or_blocked() {
        assert!(validate_report_outcome(None, None).is_err());
        assert!(validate_report_outcome(Some(""), None).is_err());
        assert!(validate_report_outcome(Some("ready"), Some("self")).is_err());
        assert!(validate_report_outcome(Some("blocked"), None).is_err());
        assert!(validate_report_outcome(Some("blocked"), Some("operator")).is_err());
        assert!(validate_report_outcome(Some("failed"), None).is_err());
        assert_eq!(
            validate_report_outcome(Some("ready"), None).unwrap(),
            (Some("ready".into()), None)
        );
        assert_eq!(
            validate_report_outcome(Some("blocked"), Some("parent")).unwrap(),
            (Some("blocked".into()), Some("parent".into()))
        );
        assert_eq!(
            validate_report_outcome(Some("blocked"), Some("lead")).unwrap(),
            (Some("blocked".into()), Some("lead".into()))
        );
    }

    #[test]
    fn iteration_abandonment_and_trim_newest_first() {
        let mut surface = empty_surface();
        start_iteration(&mut surface, "abandoned".into(), "start".into());
        start_iteration(&mut surface, "replacement".into(), "start-2".into());
        assert_eq!(surface.active_iteration.as_ref().unwrap().id, "replacement");
        assert_eq!(surface.iterations[0].id, "abandoned");
        assert!(surface.iterations[0].finished_at.is_none());
        assert!(surface.iterations[0].exit_code.is_none());

        for i in 0..(MAX_ITERATIONS + 5) {
            push_iteration(
                &mut surface,
                OperatorIteration {
                    id: format!("it-{i}"),
                    started_at: format!("s-{i}"),
                    finished_at: Some(format!("f-{i}")),
                    exit_code: Some(0),
                    reconsolidated: false,
                    headline: None,
                    summary: None,
                    ..Default::default()
                },
            );
        }
        assert_eq!(surface.iterations.len(), MAX_ITERATIONS);
        assert_eq!(
            surface.iterations[0].id,
            format!("it-{}", MAX_ITERATIONS + 4)
        );
        assert_eq!(surface.iterations.last().unwrap().id, "it-5");
    }

    #[test]
    fn refuses_symlink_target() {
        let root = tmp_root();
        let stem = "managed-personal-link";
        ensure_dirs(&root, stem).unwrap();
        let real = root.join("real.json");
        fs::write(&real, b"{}").unwrap();
        let link = operator_path(&root, stem);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let surface = OperatorSurface {
            version: VERSION,
            about: Some("x".into()),
            headline: None,
            body: None,
            updated_at: None,
            basis_revision: None,
            activity: Vec::new(),
            active_iteration: None,
            iterations: Vec::new(),
            ..Default::default()
        };
        assert!(atomic_write(&link, &surface).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
