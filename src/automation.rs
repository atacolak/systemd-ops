//! Agent-backed automation metadata, relationships, revisions, and lifecycle.
//!
//! Systemd unit files remain the definition truth. `automation.toml` stores only
//! agent-layer metadata which systemd cannot represent.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::scope::{self, ScopeManifest};
use crate::sha256::sha256_hex;
use crate::systemd::{self, BackendError};
use crate::token::{self, PlanClass};

pub const AUTOMATION_VERSION: u32 = 1;
const DIR_NAME: &str = ".systemd-ops";
const MAX_BRAIN_PATHS: usize = 32;
const MAX_REASON: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationMetadata {
    pub version: u32,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub brain_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lifecycle {
    pub status: String,
    pub completed_at: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
struct FileSnapshot {
    path: String,
    sha256: Option<String>,
}

pub fn operation_home(root: &Path, stem: &str) -> PathBuf {
    root.join(DIR_NAME).join(stem)
}

pub fn metadata_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("automation.toml")
}

pub fn lifecycle_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/lifecycle.json")
}

fn validate_agent_name(name: &str) -> Result<(), BackendError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(BackendError(
            "agent must be 1..64 lowercase letters, digits, or non-leading hyphens".into(),
        ))
    }
}

fn validate_relative_path(label: &str, path: &str) -> Result<(), BackendError> {
    if path.is_empty() || path.len() > 512 {
        return Err(BackendError(format!("{label} must be 1..512 characters")));
    }
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(BackendError(format!(
            "{label} must be a relative path without traversal"
        )));
    }
    Ok(())
}

fn normalize_metadata(
    mut metadata: AutomationMetadata,
) -> Result<AutomationMetadata, BackendError> {
    if metadata.version != AUTOMATION_VERSION {
        return Err(BackendError(format!(
            "automation.toml version must be {AUTOMATION_VERSION}"
        )));
    }
    validate_agent_name(&metadata.agent)?;
    if let Some(parent) = &metadata.parent {
        crate::operator::validate_stem(parent)?;
    }
    if metadata.brain_paths.len() > MAX_BRAIN_PATHS {
        return Err(BackendError(format!(
            "brain_paths is limited to {MAX_BRAIN_PATHS} entries"
        )));
    }
    for path in &metadata.brain_paths {
        validate_relative_path("brain_paths entry", path)?;
    }
    metadata.brain_paths.sort();
    metadata.brain_paths.dedup();
    Ok(metadata)
}

pub fn canonical_metadata(metadata: &AutomationMetadata) -> Result<String, BackendError> {
    let metadata = normalize_metadata(metadata.clone())?;
    toml::to_string(&metadata)
        .map_err(|error| BackendError(format!("cannot serialize automation metadata: {error}")))
}

pub fn parse_metadata(text: &str) -> Result<AutomationMetadata, BackendError> {
    let metadata: AutomationMetadata = toml::from_str(text)
        .map_err(|error| BackendError(format!("malformed automation.toml: {error}")))?;
    normalize_metadata(metadata)
}

fn refuse_symlink(path: &Path) -> Result<(), BackendError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(BackendError(format!(
            "refusing writable symlink {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BackendError(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

fn snapshot(path: &Path) -> FileSnapshot {
    FileSnapshot {
        path: path.to_string_lossy().into_owned(),
        sha256: fs::read(path).ok().map(|bytes| sha256_hex(&bytes)),
    }
}

fn snapshot_json(snapshot: &FileSnapshot) -> Value {
    json!({ "path": snapshot.path, "sha256": snapshot.sha256 })
}

fn snapshot_from_json(value: &Value) -> Result<FileSnapshot, BackendError> {
    Ok(FileSnapshot {
        path: value
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError("invalid automation plan snapshot".into()))?
            .to_string(),
        sha256: value
            .get("sha256")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn require_snapshot_fresh(snapshot: &FileSnapshot) -> Result<(), BackendError> {
    let current = fs::read(&snapshot.path)
        .ok()
        .map(|bytes| sha256_hex(&bytes));
    if current == snapshot.sha256 {
        Ok(())
    } else {
        Err(BackendError(format!(
            "plan is stale: '{}' changed; re-plan",
            snapshot.path
        )))
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), BackendError> {
    refuse_symlink(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| BackendError(format!("{} has no parent", path.display())))?;
    fs::create_dir_all(parent)
        .map_err(|error| BackendError(format!("cannot create {}: {error}", parent.display())))?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id()
    ));
    refuse_symlink(&tmp)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|error| BackendError(format!("cannot create {}: {error}", tmp.display())))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| BackendError(format!("cannot write {}: {error}", tmp.display())))?;
    fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|error| BackendError(format!("cannot chmod {}: {error}", tmp.display())))?;
    fs::rename(&tmp, path)
        .map_err(|error| BackendError(format!("cannot replace {}: {error}", path.display())))?;
    Ok(())
}

pub fn load_metadata(root: &Path, stem: &str) -> Result<Option<AutomationMetadata>, BackendError> {
    let path = metadata_path(root, stem);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BackendError(format!(
                "cannot read {}: {error}",
                path.display()
            )))
        }
    };
    parse_metadata(&text).map(Some)
}

pub fn load_lifecycle(root: &Path, stem: &str) -> Result<Option<Lifecycle>, BackendError> {
    let path = lifecycle_path(root, stem);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BackendError(format!(
                "cannot read {}: {error}",
                path.display()
            )))
        }
    };
    let lifecycle: Lifecycle = serde_json::from_slice(&bytes)
        .map_err(|error| BackendError(format!("malformed {}: {error}", path.display())))?;
    if lifecycle.status != "completed" || lifecycle.reason.trim().is_empty() {
        return Err(BackendError(format!(
            "{} must contain completed lifecycle state and a reason",
            path.display()
        )));
    }
    Ok(Some(lifecycle))
}

fn role_path(manifest: &ScopeManifest, agent: &str) -> Result<PathBuf, BackendError> {
    let root = manifest
        .automation_agent_root
        .as_ref()
        .ok_or_else(|| BackendError("scope has no [automation].agent_root configuration".into()))?;
    Ok(root.join(".omp/agents").join(format!("{agent}.md")))
}

fn readable_regular_file(path: &Path, label: &str) -> Result<Vec<u8>, BackendError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| BackendError(format!("missing {label} {}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BackendError(format!(
            "{label} must be a regular non-symlink file: {}",
            path.display()
        )));
    }
    fs::read(path).map_err(|error| BackendError(format!("cannot read {}: {error}", path.display())))
}

pub fn validate_agent_role(manifest: &ScopeManifest, agent: &str) -> Result<PathBuf, BackendError> {
    validate_agent_name(agent)?;
    let path = role_path(manifest, agent)?;
    readable_regular_file(&path, "agent role")?;
    Ok(path)
}

pub fn brain_revision(manifest: &ScopeManifest, stem: &str) -> Result<String, BackendError> {
    crate::operator::require_owned(manifest, stem)?;
    let metadata = load_metadata(&manifest.root, stem)?
        .ok_or_else(|| BackendError(format!("automation '{stem}' has no automation.toml")))?;
    let canonical = canonical_metadata(&metadata)?;
    let role = role_path(manifest, &metadata.agent)?;
    let role_bytes = readable_regular_file(&role, "agent role")?;
    let mut input = Vec::new();
    input.extend_from_slice(b"automation.toml\0");
    input.extend_from_slice(canonical.as_bytes());
    input.push(0);
    input.extend_from_slice(role.to_string_lossy().as_bytes());
    input.push(0);
    input.extend_from_slice(&role_bytes);
    input.push(0);
    for relative in &metadata.brain_paths {
        let path = manifest.root.join(relative);
        let bytes = readable_regular_file(&path, "brain path")?;
        input.extend_from_slice(relative.as_bytes());
        input.push(0);
        input.extend_from_slice(&bytes);
        input.push(0);
    }
    Ok(format!("sha256:{}", sha256_hex(&input)))
}

fn lifecycle_json(lifecycle: Option<&Lifecycle>) -> Value {
    match lifecycle {
        Some(lifecycle) => json!({
            "status": lifecycle.status,
            "completed_at": lifecycle.completed_at,
            "reason": lifecycle.reason,
        }),
        None => json!({ "status": "active" }),
    }
}

pub fn join_operation(manifest: &ScopeManifest, stem: &str) -> (Value, Option<String>) {
    let metadata = match load_metadata(&manifest.root, stem) {
        Ok(metadata) => metadata,
        Err(error) => return (Value::Null, Some(error.0)),
    };
    let lifecycle = match load_lifecycle(&manifest.root, stem) {
        Ok(lifecycle) => lifecycle,
        Err(error) => return (Value::Null, Some(error.0)),
    };
    let Some(metadata) = metadata else {
        return (
            json!({
                "agent": Value::Null,
                "agent_root": manifest.automation_agent_root.as_ref().map(|path| path.to_string_lossy()),
                "parent": Value::Null,
                "brain_paths": [],
                "brain_revision": Value::Null,
                "lifecycle": lifecycle_json(lifecycle.as_ref()),
            }),
            None,
        );
    };
    let revision = brain_revision(manifest, stem);
    let warning = revision.as_ref().err().map(|error| error.0.clone());
    (
        json!({
            "agent": metadata.agent,
            "agent_root": manifest.automation_agent_root.as_ref().map(|path| path.to_string_lossy()),
            "parent": metadata.parent,
            "brain_paths": metadata.brain_paths,
            "brain_revision": revision.ok(),
            "lifecycle": lifecycle_json(lifecycle.as_ref()),
        }),
        warning,
    )
}

fn relation_summary(operation: &Value) -> Value {
    let operator = operation.get("operator").unwrap_or(&Value::Null);
    let lifecycle = operation
        .get("automation")
        .and_then(|value| value.get("lifecycle"))
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("active");
    let state = operation.get("state").and_then(Value::as_str).unwrap_or("");
    let sub = operation.get("sub").and_then(Value::as_str).unwrap_or("");
    json!({
        "unit": operation.get("unit").cloned().unwrap_or(Value::Null),
        "title": operation.get("title").cloned().unwrap_or(Value::Null),
        "health": operation.get("health").cloned().unwrap_or(Value::Null),
        "lifecycle": lifecycle,
        "running": state == "active" && sub == "running",
        "headline": operator.get("headline").cloned().unwrap_or(Value::Null),
    })
}

pub fn attach_relations(operations: &mut [Value]) {
    let summaries: BTreeMap<String, Value> = operations
        .iter()
        .filter_map(|operation| {
            let unit = operation.get("unit")?.as_str()?.to_string();
            Some((unit, relation_summary(operation)))
        })
        .collect();
    let mut children: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for operation in operations.iter() {
        let Some(parent) = operation
            .get("automation")
            .and_then(|value| value.get("parent"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        children
            .entry(parent.to_string())
            .or_default()
            .push(relation_summary(operation));
    }
    for values in children.values_mut() {
        values.sort_by(|a, b| {
            a.get("unit")
                .and_then(Value::as_str)
                .cmp(&b.get("unit").and_then(Value::as_str))
        });
    }
    for operation in operations.iter_mut() {
        let unit = operation
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let parent = operation
            .get("automation")
            .and_then(|value| value.get("parent"))
            .and_then(Value::as_str)
            .and_then(|parent| summaries.get(parent))
            .cloned()
            .unwrap_or(Value::Null);
        operation["relations"] = json!({
            "parent": parent,
            "children": children.remove(&unit).unwrap_or_default(),
        });
    }
}

fn metadata_from_spec(spec: &Value) -> Result<Option<AutomationMetadata>, BackendError> {
    let agent = spec.get("agent").and_then(Value::as_str);
    let parent = spec
        .get("parent")
        .and_then(Value::as_str)
        .map(str::to_string);
    let brain_paths = match spec.get("brain_paths") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| BackendError("brain_paths must be an array of strings".into()))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| BackendError("brain_paths must contain strings".into()))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let Some(agent) = agent else {
        if parent.is_some() || !brain_paths.is_empty() {
            return Err(BackendError(
                "parent and brain_paths require an agent-backed automation".into(),
            ));
        }
        return Ok(None);
    };
    normalize_metadata(AutomationMetadata {
        version: AUTOMATION_VERSION,
        agent: agent.to_string(),
        parent,
        brain_paths,
    })
    .map(Some)
}

fn operation_spec(spec: &Value) -> Result<Value, BackendError> {
    let mut object = spec
        .as_object()
        .cloned()
        .ok_or_else(|| BackendError("spec must be an object".into()))?;
    object.remove("agent");
    object.remove("parent");
    object.remove("brain_paths");
    Ok(Value::Object(object))
}

fn resolved_manifest(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<ScopeManifest, BackendError> {
    let env_root = std::env::var("SYSTEMD_OPS_SCOPE_ROOT").ok();
    scope::resolve(explicit_root, env_root.as_deref(), cwd)
}

fn validate_parent(
    manifest: &ScopeManifest,
    stem: &str,
    parent: Option<&str>,
) -> Result<(), BackendError> {
    let Some(parent) = parent else {
        return Ok(());
    };
    if parent == stem {
        return Err(BackendError("automation cannot be its own parent".into()));
    }
    crate::operator::require_owned(manifest, parent)?;
    crate::operations::get_operation_any(parent).map_err(|_| {
        BackendError(format!(
            "parent automation '{parent}' does not exist in this scope"
        ))
    })?;
    if load_metadata(&manifest.root, parent)?.is_none() {
        return Err(BackendError(format!(
            "parent automation '{parent}' has no automation.toml"
        )));
    }
    let mut seen = BTreeSet::new();
    seen.insert(stem.to_string());
    let mut cursor = Some(parent.to_string());
    while let Some(current) = cursor {
        if !seen.insert(current.clone()) {
            return Err(BackendError(
                "automation parent relation would create a cycle".into(),
            ));
        }
        cursor = load_metadata(&manifest.root, &current)?.and_then(|metadata| metadata.parent);
    }
    Ok(())
}

fn plan_instance(
    action: &str,
    args: &Value,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    let spec = args
        .get("spec")
        .ok_or_else(|| BackendError("missing required argument: spec".into()))?;
    let stem = spec
        .get("unit")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("spec.unit is required".into()))?;
    crate::operator::require_owned(&manifest, stem)?;
    let metadata = metadata_from_spec(spec)?;
    if let Some(metadata) = &metadata {
        validate_agent_role(&manifest, &metadata.agent)?;
        validate_parent(&manifest, stem, metadata.parent.as_deref())?;
    }
    let op_spec = operation_spec(spec)?;
    let nested = match action {
        "create" => {
            crate::operations::plan_create(&json!({ "spec": op_spec, "context": { "cwd": cwd } }))?
        }
        "update" => {
            crate::operations::plan_update(&json!({ "spec": op_spec, "context": { "cwd": cwd } }))?
        }
        _ => {
            return Err(BackendError(format!(
                "unknown automation author action '{action}'"
            )))
        }
    };
    let author_token = nested
        .get("plan_token")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("systemd author plan did not return a plan token".into()))?;
    let metadata_file = metadata_path(&manifest.root, stem);
    let metadata_snapshot = snapshot(&metadata_file);
    if action == "create" && metadata_snapshot.sha256.is_some() {
        return Err(BackendError(format!(
            "create refused: '{}' already exists",
            metadata_file.display()
        )));
    }
    let metadata_text = metadata.as_ref().map(canonical_metadata).transpose()?;
    let cfg = crate::config::current_or_load()?;
    let payload = json!({
        "action": action,
        "author_plan_token": author_token,
        "metadata": metadata_text,
        "metadata_snapshot": snapshot_json(&metadata_snapshot),
        "scope_root": manifest.root.to_string_lossy(),
    });
    let (plan_token, sealed) = token::mint(
        &cfg,
        PlanClass::Automation,
        stem,
        cwd.map(str::to_string),
        payload,
    )?;
    Ok(json!({
        "plan_token": plan_token,
        "class": "automation",
        "unit": stem,
        "action": action,
        "issued_at": crate::config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(sealed.expires_at),
        "metadata": metadata,
        "systemd": nested,
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

pub fn plan_create(
    args: &Value,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    plan_instance("create", args, explicit_root, cwd)
}

pub fn plan_update(
    args: &Value,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    plan_instance("update", args, explicit_root, cwd)
}

pub fn plan_retire(
    stem: &str,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    crate::operator::require_owned(&manifest, stem)?;
    let nested =
        crate::operations::plan_retire(&json!({ "unit": stem, "context": { "cwd": cwd } }))?;
    let author_token = nested
        .get("plan_token")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("systemd author plan did not return a plan token".into()))?;
    let cfg = crate::config::current_or_load()?;
    let (plan_token, sealed) = token::mint(
        &cfg,
        PlanClass::Automation,
        stem,
        cwd.map(str::to_string),
        json!({
            "action": "retire",
            "author_plan_token": author_token,
            "scope_root": manifest.root.to_string_lossy(),
        }),
    )?;
    Ok(json!({
        "plan_token": plan_token,
        "class": "automation",
        "unit": stem,
        "action": "retire",
        "issued_at": crate::config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(sealed.expires_at),
        "systemd": nested,
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

pub fn plan_complete(
    stem: &str,
    reason: &str,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_REASON || reason.contains(['\n', '\r']) {
        return Err(BackendError(format!(
            "completion reason must be one line of 1..{MAX_REASON} characters"
        )));
    }
    let manifest = resolved_manifest(explicit_root, cwd)?;
    crate::operator::require_owned(&manifest, stem)?;
    crate::operations::get_operation_any(stem)?;
    let path = lifecycle_path(&manifest.root, stem);
    let lifecycle_snapshot = snapshot(&path);
    if let Some(existing) = load_lifecycle(&manifest.root, stem)? {
        return Err(BackendError(format!(
            "automation '{stem}' is already completed: {}",
            existing.reason
        )));
    }
    let completed_at = crate::config::unix_to_rfc3339(crate::config::now_unix());
    let cfg = crate::config::current_or_load()?;
    let (plan_token, sealed) = token::mint(
        &cfg,
        PlanClass::Automation,
        stem,
        cwd.map(str::to_string),
        json!({
            "action": "complete",
            "scope_root": manifest.root.to_string_lossy(),
            "lifecycle_snapshot": snapshot_json(&lifecycle_snapshot),
            "lifecycle": {
                "status": "completed",
                "completed_at": completed_at,
                "reason": reason,
            },
        }),
    )?;
    Ok(json!({
        "plan_token": plan_token,
        "class": "automation",
        "unit": stem,
        "action": "complete",
        "issued_at": crate::config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(sealed.expires_at),
        "lifecycle": {
            "status": "completed",
            "completed_at": completed_at,
            "reason": reason,
        },
        "systemd": {
            "stop": format!("{stem}.timer"),
            "disable": format!("{stem}.timer"),
            "preserve_definition": true,
        },
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

pub fn apply(plan: &token::SealedPlan, cwd: Option<&str>) -> Result<Value, BackendError> {
    token::require_class(plan, PlanClass::Automation)?;
    let action = plan
        .payload
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    match action {
        "create" | "update" => {
            let snapshot = snapshot_from_json(
                plan.payload
                    .get("metadata_snapshot")
                    .ok_or_else(|| BackendError("invalid automation plan token".into()))?,
            )?;
            require_snapshot_fresh(&snapshot)?;
            let author_token = plan
                .payload
                .get("author_plan_token")
                .and_then(Value::as_str)
                .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
            let systemd_result = crate::write::apply_with_context(author_token, cwd)?;
            let metadata = plan.payload.get("metadata").and_then(Value::as_str);
            if let Some(metadata) = metadata {
                atomic_write(Path::new(&snapshot.path), metadata.as_bytes())?;
            }
            Ok(json!({
                "class": "automation",
                "unit": plan.unit,
                "action": action,
                "applied": true,
                "metadata_file": metadata.map(|_| snapshot.path),
                "systemd": systemd_result,
            }))
        }
        "retire" => {
            let author_token = plan
                .payload
                .get("author_plan_token")
                .and_then(Value::as_str)
                .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
            let systemd_result = crate::write::apply_with_context(author_token, cwd)?;
            Ok(json!({
                "class": "automation",
                "unit": plan.unit,
                "action": "retire",
                "applied": true,
                "systemd": systemd_result,
                "history_preserved": true,
            }))
        }
        "complete" => {
            let snapshot = snapshot_from_json(
                plan.payload
                    .get("lifecycle_snapshot")
                    .ok_or_else(|| BackendError("invalid automation plan token".into()))?,
            )?;
            require_snapshot_fresh(&snapshot)?;
            let lifecycle: Lifecycle = serde_json::from_value(
                plan.payload
                    .get("lifecycle")
                    .cloned()
                    .ok_or_else(|| BackendError("invalid automation plan token".into()))?,
            )
            .map_err(|_| BackendError("invalid automation plan token".into()))?;
            let timer = format!("{}.timer", plan.unit);
            let mut changes = Vec::new();
            if systemd::ensure_unit_known(&timer).is_ok() {
                if let Ok(lines) = systemd::apply_verb("stop", &timer, None) {
                    changes.extend(lines);
                }
                if let Ok(lines) = systemd::try_disable(&timer) {
                    changes.extend(lines);
                }
            }
            let bytes = serde_json::to_vec_pretty(&lifecycle)
                .map_err(|error| BackendError(format!("cannot serialize lifecycle: {error}")))?;
            atomic_write(Path::new(&snapshot.path), &bytes)?;
            Ok(json!({
                "class": "automation",
                "unit": plan.unit,
                "action": "complete",
                "applied": true,
                "lifecycle": lifecycle,
                "changes": changes,
                "definition_preserved": true,
                "history_preserved": true,
            }))
        }
        _ => Err(BackendError("invalid automation plan token".into())),
    }
}

pub fn inspect(
    stem: &str,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    crate::operator::require_owned(&manifest, stem)?;
    let view = scope::show_manifest(&manifest)?;
    let operation = view
        .owned
        .into_iter()
        .find(|operation| operation.get("unit").and_then(Value::as_str) == Some(stem))
        .unwrap_or(Value::Null);
    Ok(json!({
        "unit": stem,
        "metadata_file": metadata_path(&manifest.root, stem),
        "automation": operation.get("automation").cloned().unwrap_or(Value::Null),
        "relations": operation.get("relations").cloned().unwrap_or(Value::Null),
        "operation": operation,
    }))
}

pub fn complete_now(
    stem: &str,
    reason: &str,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let planned = plan_complete(stem, reason, explicit_root, cwd)?;
    let token = planned
        .get("plan_token")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("completion plan did not return a token".into()))?;
    crate::write::apply_with_context(token, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_metadata_sorts_brain_paths() {
        let metadata = AutomationMetadata {
            version: 1,
            agent: "pr-maintainer".into(),
            parent: None,
            brain_paths: vec!["z".into(), "a".into(), "z".into()],
        };
        let text = canonical_metadata(&metadata).unwrap();
        assert!(text.find("\"a\"").unwrap() < text.find("\"z\"").unwrap());
        let parsed = parse_metadata(&text).unwrap();
        assert_eq!(parsed.brain_paths, vec!["a", "z"]);
    }

    #[test]
    fn metadata_rejects_traversal() {
        let metadata = AutomationMetadata {
            version: 1,
            agent: "pr-maintainer".into(),
            parent: None,
            brain_paths: vec!["../outside".into()],
        };
        assert!(canonical_metadata(&metadata)
            .unwrap_err()
            .0
            .contains("traversal"));
    }

    #[test]
    fn relation_summaries_do_not_leak_history() {
        let mut operations = vec![
            json!({
                "unit": "managed-parent", "title": "Parent", "health": "healthy",
                "state": "inactive", "sub": "dead",
                "automation": {"parent": null, "lifecycle": {"status": "active"}},
                "operator": {"headline": "ready", "iterations": [{"summary": "secret"}]}
            }),
            json!({
                "unit": "managed-child", "title": "Child", "health": "healthy",
                "state": "active", "sub": "running",
                "automation": {"parent": "managed-parent", "lifecycle": {"status": "completed"}},
                "operator": {"headline": "merged", "iterations": [{"summary": "secret"}]}
            }),
        ];
        attach_relations(&mut operations);
        let child = &operations[0]["relations"]["children"][0];
        assert_eq!(child["unit"], "managed-child");
        assert_eq!(child["lifecycle"], "completed");
        assert!(child.get("iterations").is_none());
        assert!(child.to_string().find("secret").is_none());
    }
}
