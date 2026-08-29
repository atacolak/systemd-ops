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
const REQUESTS_DIR: &str = "requests";
const MAX_BRAIN_PATHS: usize = 32;
const MAX_REASON: usize = 500;
const MAX_REQUEST_SUMMARY: usize = 120;
const MAX_REQUEST_REASON: usize = 2000;

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
        "agent": operation
            .get("automation")
            .and_then(|value| value.get("agent"))
            .cloned()
            .unwrap_or(Value::Null),
        "health": operation.get("health").cloned().unwrap_or(Value::Null),
        "lifecycle": lifecycle,
        "running": state == "active" && sub == "running",
        "headline": operator.get("headline").cloned().unwrap_or(Value::Null),
        "checkpoint": checkpoint_summary(operation),
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
            match systemd::ensure_unit_known(&timer) {
                Ok(()) => {
                    changes.extend(systemd::apply_verb("stop", &timer, None)?);
                    changes.extend(systemd::apply_verb("disable", &timer, None)?);
                }
                Err(error) if error.0.starts_with("no such unit:") => {}
                Err(error) => return Err(error),
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

pub fn inspect_plan(token: &str) -> Result<Value, BackendError> {
    let cfg = crate::config::current_or_load()?;
    let plan = token::parse(&cfg, token)?;
    let action = plan
        .payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let agent = plan
        .payload
        .get("metadata")
        .and_then(Value::as_str)
        .and_then(|text| parse_metadata(text).ok())
        .map(|metadata| metadata.agent);
    let parent = plan
        .payload
        .get("metadata")
        .and_then(Value::as_str)
        .and_then(|text| parse_metadata(text).ok())
        .and_then(|metadata| metadata.parent);
    Ok(json!({
        "class": plan.class.as_str(),
        "unit": plan.unit,
        "action": action,
        "agent": agent,
        "parent": parent,
        "scope_root": plan.payload.get("scope_root").cloned().unwrap_or(Value::Null),
        "issued_at": crate::config::unix_to_rfc3339(plan.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(plan.expires_at),
        "origin_cwd": plan.origin_cwd,
    }))
}

fn requests_dir(root: &Path) -> PathBuf {
    root.join(DIR_NAME).join(REQUESTS_DIR)
}

fn request_path(root: &Path, id: &str) -> PathBuf {
    requests_dir(root).join(format!("{id}.json"))
}

fn validate_request_id(id: &str) -> Result<(), BackendError> {
    let rest = id
        .strip_prefix("req-")
        .ok_or_else(|| BackendError("request id must match req-YYYYMMDD-xxxxxx".into()))?;
    let valid = rest.len() == 15
        && rest.as_bytes().get(8) == Some(&b'-')
        && rest.bytes().enumerate().all(|(index, byte)| {
            if index == 8 {
                byte == b'-'
            } else {
                byte.is_ascii_alphanumeric()
            }
        });
    if valid {
        Ok(())
    } else {
        Err(BackendError(
            "request id must match req-YYYYMMDD-xxxxxx".into(),
        ))
    }
}

fn new_request_id() -> Result<String, BackendError> {
    let now = crate::config::now_unix();
    let date = crate::config::unix_to_rfc3339(now);
    let ymd = date.get(..10).unwrap_or("19700101").replace('-', "");
    let mut bytes = [0u8; 3];
    let mut random = std::fs::File::open("/dev/urandom")
        .map_err(|error| BackendError(format!("cannot open /dev/urandom: {error}")))?;
    use std::io::Read;
    random
        .read_exact(&mut bytes)
        .map_err(|error| BackendError(format!("cannot read /dev/urandom: {error}")))?;
    Ok(format!(
        "req-{ymd}-{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2]
    ))
}

fn write_new_request(path: &Path, bytes: &[u8]) -> Result<bool, BackendError> {
    refuse_symlink(path)?;
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(BackendError(format!(
                "cannot create {}: {error}",
                path.display()
            )))
        }
    };
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(BackendError(format!(
            "cannot write {}: {error}",
            path.display()
        )));
    }
    if let Err(error) =
        fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
    {
        let _ = fs::remove_file(path);
        return Err(BackendError(format!(
            "cannot chmod {}: {error}",
            path.display()
        )));
    }
    Ok(true)
}

fn load_request(root: &Path, id: &str) -> Result<Value, BackendError> {
    validate_request_id(id)?;
    let path = request_path(root, id);
    let bytes = fs::read(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BackendError(format!("request '{id}' does not exist"))
        } else {
            BackendError(format!("cannot read {}: {error}", path.display()))
        }
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|error| BackendError(format!("malformed {}: {error}", path.display())))
}

pub fn request_capability(
    summary: &str,
    reason: &str,
    target_agent: Option<&str>,
    suggested_agent: Option<&str>,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let summary = crate::operator::strict_line("summary", summary, MAX_REQUEST_SUMMARY)?;
    let reason = crate::operator::strict_line("reason", reason, MAX_REQUEST_REASON)?;
    if let Some(agent) = target_agent {
        validate_agent_name(agent)?;
    }
    if let Some(agent) = suggested_agent {
        validate_agent_name(agent)?;
    }
    if target_agent.is_some() == suggested_agent.is_some() {
        return Err(BackendError(
            "exactly one of target_agent or suggested_agent is required".into(),
        ));
    }
    let (manifest, stem) = crate::operator::bound_operation_manifest(explicit_root, cwd)?;
    let requester_agent = load_metadata(&manifest.root, &stem)?
        .map(|metadata| metadata.agent)
        .unwrap_or_default();
    let directory = requests_dir(&manifest.root);
    fs::create_dir_all(&directory)
        .map_err(|error| BackendError(format!("cannot create {}: {error}", directory.display())))?;
    for _ in 0..16 {
        let id = new_request_id()?;
        let record = json!({
            "version": 1,
            "id": id,
            "kind": "automation-capability",
            "status": "open",
            "created_at": crate::config::unix_to_rfc3339(crate::config::now_unix()),
            "requester_unit": stem,
            "requester_agent": requester_agent,
            "summary": summary,
            "reason": reason,
            "target_agent": target_agent,
            "suggested_agent": suggested_agent,
            "resolved_at": Value::Null,
            "resolution": Value::Null,
        });
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|error| BackendError(format!("cannot serialize request: {error}")))?;
        if write_new_request(&request_path(&manifest.root, &id), &bytes)? {
            return Ok(record);
        }
    }
    Err(BackendError(
        "cannot allocate a unique request id after 16 attempts".into(),
    ))
}

pub fn list_requests(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    let directory = requests_dir(&manifest.root);
    let mut requests = Vec::new();
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({ "requests": [] }));
        }
        Err(error) => {
            return Err(BackendError(format!(
                "cannot read {}: {error}",
                directory.display()
            )))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            BackendError(format!("cannot read {}: {error}", directory.display()))
        })?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        if validate_request_id(id).is_ok() {
            requests.push(load_request(&manifest.root, id)?);
        }
    }
    requests.sort_by(|a, b| {
        a.get("id")
            .and_then(Value::as_str)
            .cmp(&b.get("id").and_then(Value::as_str))
    });
    Ok(json!({ "requests": requests }))
}

pub fn inspect_request(
    id: &str,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let manifest = resolved_manifest(explicit_root, cwd)?;
    load_request(&manifest.root, id)
}

pub fn resolve_request(
    id: &str,
    resolution: &str,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let resolution = crate::operator::strict_line("resolution", resolution, MAX_REQUEST_REASON)?;
    let manifest = resolved_manifest(explicit_root, cwd)?;
    let mut record = load_request(&manifest.root, id)?;
    if record.get("status").and_then(Value::as_str) != Some("open") {
        return Err(BackendError(format!("request '{id}' is not open")));
    }
    record["status"] = json!("resolved");
    record["resolved_at"] = json!(crate::config::unix_to_rfc3339(crate::config::now_unix()));
    record["resolution"] = json!(resolution);
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|error| BackendError(format!("cannot serialize request: {error}")))?;
    atomic_write(&request_path(&manifest.root, id), &bytes)?;
    Ok(record)
}

fn checkpoint_summary(operation: &Value) -> Value {
    let operator = operation.get("operator").unwrap_or(&Value::Null);
    let latest = operator
        .get("iterations")
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    let exit_zero = latest
        .and_then(|item| item.get("exit_code"))
        .and_then(Value::as_i64)
        == Some(0);
    let reconsolidated = latest
        .and_then(|item| item.get("reconsolidated"))
        .and_then(Value::as_bool)
        == Some(true);
    let fingerprint = operation_home_from_unit(operation)
        .and_then(|home| fs::read_to_string(home.join("state/fingerprint")).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    json!({
        "exit_zero": exit_zero,
        "reconsolidated": reconsolidated,
        "fingerprint": fingerprint,
        "stable": exit_zero && reconsolidated && fingerprint.is_some(),
    })
}

fn operation_home_from_unit(operation: &Value) -> Option<PathBuf> {
    operation
        .get("operation_home")
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

pub fn notify_parent(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let (manifest, stem) = crate::operator::bound_operation_manifest(explicit_root, cwd)?;
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
    let parent = operation
        .get("automation")
        .and_then(|value| value.get("parent"))
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError(format!("'{stem}' has no parent to notify")))?;
    let checkpoint = checkpoint_summary(operation);
    if checkpoint.get("stable").and_then(Value::as_bool) != Some(true) {
        return Err(BackendError(format!(
            "parent notification requires a stable successful checkpoint for '{stem}'"
        )));
    }
    let timer = format!("{parent}.timer");
    let planned = crate::write::plan(crate::write::Action::Start, &timer, None)?;
    let token = planned
        .get("plan_token")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("parent notification plan did not return a token".into()))?;
    let applied = crate::write::apply_with_context(token, cwd)?;
    Ok(json!({
        "notified": true,
        "child": stem,
        "parent": parent,
        "checkpoint": checkpoint,
        "systemd": applied,
    }))
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
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    static PATH_LOCK: Mutex<()> = Mutex::new(());

    fn tmp_root(name: &str) -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "systemd-ops-automation-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn test_manifest(root: &Path, agent_root: &Path) -> ScopeManifest {
        ScopeManifest {
            id: "proof".into(),
            root: root.to_path_buf(),
            owned: vec!["managed-*".into()],
            critical: Vec::new(),
            watch: Vec::new(),
            automation_agent_root: Some(agent_root.to_path_buf()),
        }
    }

    fn write_role(agent_root: &Path, bytes: &[u8]) {
        let path = agent_root.join(".omp/agents/pr-maintainer.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn write_metadata(root: &Path, stem: &str, brain_paths: Vec<&str>) {
        let metadata = AutomationMetadata {
            version: AUTOMATION_VERSION,
            agent: "pr-maintainer".into(),
            parent: None,
            brain_paths: brain_paths.into_iter().map(str::to_string).collect(),
        };
        let path = metadata_path(root, stem);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, canonical_metadata(&metadata).unwrap()).unwrap();
    }

    fn write_parent_fixture(root: &Path, stem: &str, parent: Option<&str>) {
        let metadata = AutomationMetadata {
            version: AUTOMATION_VERSION,
            agent: "pr-maintainer".into(),
            parent: parent.map(str::to_string),
            brain_paths: Vec::new(),
        };
        let path = metadata_path(root, stem);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, canonical_metadata(&metadata).unwrap()).unwrap();
    }

    fn fake_inventory_systemctl(root: &Path, stems: &[&str]) -> PathBuf {
        let bin = root.join("inventory-bin");
        fs::create_dir_all(&bin).unwrap();
        let rows: Vec<Value> = stems
            .iter()
            .map(|stem| {
                json!({
                    "unit_file": format!("{stem}.service"),
                    "state": "static",
                    "preset": "enabled"
                })
            })
            .collect();
        fs::write(
            bin.join("systemctl"),
            format!(
                "#!/bin/sh\ncase \" $* \" in\n  *\" list-unit-files \"*|*\" list-unit-files\"*) cat <<'EOF'\n{}\nEOF\n    ;;\n  *) printf '[]\\n' ;;\nesac\n",
                serde_json::to_string(&rows).unwrap()
            ),
        )
        .unwrap();
        fs::set_permissions(bin.join("systemctl"), fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn with_user_unit_dir<T>(root: &Path, stems: &[&str], run: impl FnOnce() -> T) -> T {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        systemd::set_manager(systemd::Manager::User);
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let previous_path = std::env::var_os("PATH");
        std::env::set_var("XDG_CONFIG_HOME", root.join("xdg"));
        let unit_dir = systemd::unit_file_dir();
        fs::create_dir_all(&unit_dir).unwrap();
        for stem in stems {
            fs::write(
                unit_dir.join(format!("{stem}.service")),
                format!(
                    "# Managed by systemd-ops\n[Unit]\nDescription={stem}\n[Service]\nType=oneshot\nExecStart=/bin/true\n"
                ),
            )
            .unwrap();
        }
        let bin = fake_inventory_systemctl(root, stems);
        let mut paths = vec![bin];
        if let Some(previous) = previous_path.as_ref() {
            paths.extend(std::env::split_paths(previous));
        }
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let result = run();
        match previous_path {
            Some(previous) => std::env::set_var("PATH", previous),
            None => std::env::remove_var("PATH"),
        }
        match previous_xdg {
            Some(previous) => std::env::set_var("XDG_CONFIG_HOME", previous),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        result
    }

    #[test]
    fn parent_accepts_existing_same_scope_automation() {
        let root = tmp_root("parent-success");
        let manifest = test_manifest(&root, &root.join("agents"));
        with_user_unit_dir(&root, &["managed-parent"], || {
            write_parent_fixture(&root, "managed-parent", None);
            let result = validate_parent(&manifest, "managed-child", Some("managed-parent"));
            assert!(result.is_ok(), "{result:?}");
        });
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parent_rejects_missing_automation() {
        let root = tmp_root("parent-missing");
        let manifest = test_manifest(&root, &root.join("agents"));
        let error = with_user_unit_dir(&root, &[], || {
            validate_parent(&manifest, "managed-child", Some("managed-missing")).unwrap_err()
        });
        assert!(error.0.contains("does not exist in this scope"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parent_rejects_self_relation() {
        let root = tmp_root("parent-self");
        let manifest = test_manifest(&root, &root.join("agents"));
        let error = validate_parent(&manifest, "managed-child", Some("managed-child")).unwrap_err();
        assert!(error.0.contains("own parent"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parent_rejects_cycles() {
        let root = tmp_root("parent-cycle");
        let manifest = test_manifest(&root, &root.join("agents"));
        let error = with_user_unit_dir(&root, &["managed-parent"], || {
            write_parent_fixture(&root, "managed-parent", Some("managed-child"));
            validate_parent(&manifest, "managed-child", Some("managed-parent")).unwrap_err()
        });
        assert!(error.0.contains("create a cycle"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parent_rejects_operation_outside_owned_scope() {
        let root = tmp_root("parent-unowned");
        let manifest = test_manifest(&root, &root.join("agents"));
        let error =
            validate_parent(&manifest, "managed-child", Some("outside-parent")).unwrap_err();
        assert!(error.0.contains("restricted to owned stems"), "{}", error.0);
        let _ = fs::remove_dir_all(root);
    }

    fn completion_plan(path: &Path, stem: &str) -> token::SealedPlan {
        token::SealedPlan {
            class: PlanClass::Automation,
            manager: systemd::Manager::User,
            unit: stem.into(),
            issued_at: 0,
            expires_at: u64::MAX,
            origin_cwd: None,
            payload: json!({
                "action": "complete",
                "lifecycle_snapshot": snapshot_json(&snapshot(path)),
                "lifecycle": {
                    "status": "completed",
                    "completed_at": "2026-08-29T00:00:00Z",
                    "reason": "proof complete"
                }
            }),
        }
    }

    fn fake_systemctl(root: &Path, fail_verb: Option<&str>) -> (PathBuf, PathBuf) {
        let bin = root.join("bin");
        let log = root.join("systemctl.log");
        fs::create_dir_all(&bin).unwrap();
        let script = bin.join("systemctl");
        let fail = fail_verb.unwrap_or("");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >>'{}'\nargs=\" $* \"\ncase \"$args\" in\n  *' show '*) printf 'LoadState=loaded\\n';;\n  *' {} '*) exit 23;;\nesac\n",
                log.display(),
                fail
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (bin, log)
    }

    fn with_path<T>(bin: &Path, run: impl FnOnce() -> T) -> T {
        let _guard = PATH_LOCK.lock().unwrap();
        let previous = std::env::var_os("PATH");
        let mut paths = vec![bin.to_path_buf()];
        if let Some(previous) = previous.as_ref() {
            paths.extend(std::env::split_paths(previous));
        }
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let result = run();
        match previous {
            Some(previous) => std::env::set_var("PATH", previous),
            None => std::env::remove_var("PATH"),
        }
        result
    }

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
    fn inspect_plan_exposes_action_unit_and_agent() {
        let dir = std::env::temp_dir().join(format!(
            "systemd-ops-inspect-plan-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::create_dir_all(&dir);
        let cfg = crate::config::OpsConfig {
            manager: systemd::Manager::User,
            write_prefix: Some("managed-*".into()),
            plan_ttl_secs: 600,
            state_dir: dir.clone(),
        };
        crate::config::set_current(cfg.clone());
        let metadata = canonical_metadata(&AutomationMetadata {
            version: 1,
            agent: "pr-maintainer".into(),
            parent: Some("managed-runtime".into()),
            brain_paths: Vec::new(),
        })
        .unwrap();
        let (token, _) = token::mint(
            &cfg,
            PlanClass::Automation,
            "managed-child",
            None,
            json!({
                "action": "create",
                "metadata": metadata,
                "scope_root": "/tmp/scope",
            }),
        )
        .unwrap();
        let inspected = inspect_plan(&token).unwrap();
        assert_eq!(inspected["class"], "automation");
        assert_eq!(inspected["unit"], "managed-child");
        assert_eq!(inspected["action"], "create");
        assert_eq!(inspected["agent"], "pr-maintainer");
        assert_eq!(inspected["parent"], "managed-runtime");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn request_lifecycle_is_durable_and_compact() {
        let root = tmp_root("requests");
        let previous_root = std::env::var_os("SYSTEMD_OPS_SCOPE_ROOT");
        let previous_op = std::env::var_os("SYSTEMD_OPS_OPERATION");
        std::env::set_var("SYSTEMD_OPS_SCOPE_ROOT", &root);
        std::env::set_var("SYSTEMD_OPS_OPERATION", "managed-proof");
        fs::create_dir_all(root.join("agents")).unwrap();

        fs::create_dir_all(root.join(".systemd-ops")).unwrap();
        fs::write(
            root.join(".systemd-ops/scope.toml"),
            format!(
                "[scope]\nid = \"proof\"\nowned = [\"managed-*\"]\n[automation]\nagent_root = \"{}\"\n",
                root.join("agents").display()
            ),
        )
        .unwrap();

        write_metadata(&root, "managed-proof", vec![]);
        let created = request_capability(
            "need capability maintainer",
            "durable local source has no class",
            None,
            Some("capability-maintainer"),
            Some(root.to_str().unwrap()),
            Some(root.to_str().unwrap()),
        )
        .unwrap();
        let id = created["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("req-"));
        assert_eq!(id.len(), 19);
        assert_eq!(created["status"], "open");
        let listed =
            list_requests(Some(root.to_str().unwrap()), Some(root.to_str().unwrap())).unwrap();
        assert_eq!(listed["requests"][0]["id"], id);
        let resolved = resolve_request(
            &id,
            "created capability-maintainer",
            Some(root.to_str().unwrap()),
            Some(root.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(resolved["status"], "resolved");
        match previous_root {
            Some(value) => std::env::set_var("SYSTEMD_OPS_SCOPE_ROOT", value),
            None => std::env::remove_var("SYSTEMD_OPS_SCOPE_ROOT"),
        }
        match previous_op {
            Some(value) => std::env::set_var("SYSTEMD_OPS_OPERATION", value),
            None => std::env::remove_var("SYSTEMD_OPS_OPERATION"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn request_create_never_overwrites_an_existing_record() {
        let root = tmp_root("request-collision");
        let directory = requests_dir(&root);
        fs::create_dir_all(&directory).unwrap();
        let id = new_request_id().unwrap();
        let path = request_path(&root, &id);
        fs::write(&path, b"existing\n").unwrap();
        assert!(!write_new_request(&path, b"replacement\n").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"existing\n");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_request_record_is_reported() {
        let root = tmp_root("request-malformed");
        let id = "req-20260829-abcdef";
        let path = request_path(&root, id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{not-json").unwrap();
        let error = load_request(&root, id).unwrap_err();
        assert!(error.0.contains("malformed"), "{}", error.0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn notify_parent_requires_stable_checkpoint() {
        let root = tmp_root("notify");
        let previous_root = std::env::var_os("SYSTEMD_OPS_SCOPE_ROOT");
        let previous_op = std::env::var_os("SYSTEMD_OPS_OPERATION");
        std::env::set_var("SYSTEMD_OPS_SCOPE_ROOT", &root);
        std::env::set_var("SYSTEMD_OPS_OPERATION", "managed-child");
        fs::create_dir_all(root.join("agents")).unwrap();
        fs::create_dir_all(root.join(".systemd-ops")).unwrap();
        fs::write(
            root.join(".systemd-ops/scope.toml"),
            format!(
                "[scope]\nid = \"proof\"\nowned = [\"managed-*\"]\n[automation]\nagent_root = \"{}\"\n",
                root.join("agents").display()
            ),
        )
        .unwrap();
        let error = with_user_unit_dir(&root, &["managed-child", "managed-parent"], || {
            write_parent_fixture(&root, "managed-parent", None);
            write_parent_fixture(&root, "managed-child", Some("managed-parent"));
            notify_parent(Some(root.to_str().unwrap()), Some(root.to_str().unwrap())).unwrap_err()
        });
        assert!(
            error.0.contains("stable successful checkpoint") || error.0.contains("no parent"),
            "{}",
            error.0
        );
        match previous_root {
            Some(value) => std::env::set_var("SYSTEMD_OPS_SCOPE_ROOT", value),
            None => std::env::remove_var("SYSTEMD_OPS_SCOPE_ROOT"),
        }
        match previous_op {
            Some(value) => std::env::set_var("SYSTEMD_OPS_OPERATION", value),
            None => std::env::remove_var("SYSTEMD_OPS_OPERATION"),
        }
        let _ = fs::remove_dir_all(root);
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

    #[test]
    fn brain_revision_is_stable_and_byte_sensitive() {
        let root = tmp_root("revision");
        let agent_root = root.join("agents");
        let manifest = test_manifest(&root, &agent_root);
        let stem = "managed-proof";
        write_role(&agent_root, b"role bytes\n");
        fs::create_dir_all(root.join("brain")).unwrap();
        fs::write(root.join("brain/a"), b"alpha\n").unwrap();
        fs::write(root.join("brain/z"), b"zeta\n").unwrap();
        write_metadata(&root, stem, vec!["brain/z", "brain/a", "brain/z"]);

        let first = brain_revision(&manifest, stem).unwrap();
        let second = brain_revision(&manifest, stem).unwrap();
        assert_eq!(first, second);
        fs::write(root.join("brain/a"), b"alpha changed\n").unwrap();
        assert_ne!(first, brain_revision(&manifest, stem).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn brain_revision_requires_role_and_listed_paths() {
        let root = tmp_root("missing-brain");
        let agent_root = root.join("agents");
        let manifest = test_manifest(&root, &agent_root);
        let stem = "managed-proof";
        write_metadata(&root, stem, vec!["brain/missing"]);
        assert!(brain_revision(&manifest, stem)
            .unwrap_err()
            .0
            .contains("agent role"));
        write_role(&agent_root, b"role bytes\n");
        assert!(brain_revision(&manifest, stem)
            .unwrap_err()
            .0
            .contains("brain path"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn completion_persists_only_after_stop_and_disable() {
        systemd::set_manager(systemd::Manager::User);
        let root = tmp_root("complete-success");
        let lifecycle = lifecycle_path(&root, "managed-proof");
        let (bin, log) = fake_systemctl(&root, None);
        let result = with_path(&bin, || {
            apply(&completion_plan(&lifecycle, "managed-proof"), None)
        });
        assert!(result.is_ok());
        assert!(lifecycle.exists());
        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains(" stop "));
        assert!(calls.contains(" disable "));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn completion_failure_preserves_active_lifecycle() {
        systemd::set_manager(systemd::Manager::User);
        let root = tmp_root("complete-failure");
        let lifecycle = lifecycle_path(&root, "managed-proof");
        let (bin, _) = fake_systemctl(&root, Some("disable"));
        let error = with_path(&bin, || {
            apply(&completion_plan(&lifecycle, "managed-proof"), None)
        })
        .unwrap_err();
        assert!(error.0.contains("systemctl exited"));
        assert!(!lifecycle.exists());
        let _ = fs::remove_dir_all(root);
    }
}
