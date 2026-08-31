//! Agent-backed automation metadata, relationships, revisions, and lifecycle.
//!
//! Systemd unit files remain the definition truth. `automation.toml` stores only
//! agent-layer metadata which systemd cannot represent.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::scope::{self, ScopeManifest};
use crate::sha256::sha256_hex;
use crate::systemd::{self, BackendError};
use crate::token::{self, PlanClass};

pub const AUTOMATION_VERSION: u32 = 1;

const DIR_NAME: &str = ".systemd-ops";
const REQUESTS_DIR: &str = "requests";
const OPERATIONS_DIR: &str = "operations";

const MAX_BRAIN_PATHS: usize = 32;
const MAX_REASON: usize = 500;
const MAX_REQUEST_SUMMARY: usize = 120;
const MAX_REQUEST_REASON: usize = 2000;
const MAX_OPAQUE: usize = 128;
const MAX_BLOCKER_KIND: usize = 64;
const MAX_BLOCKER_SUMMARY: usize = 200;
const MAX_ITERATION_ID: usize = 80;
const MAX_OBSERVER_ARGS: usize = 32;
const MAX_OBSERVER_VALUE: usize = 512;

const BLOCKER_KINDS: &[&str] = &[
    "iteration-failed",
    "worktree-dirty",
    "worktree-diverged",
    "worktree-detached",
    "worktree-missing",
    "contract-failure",
    "stale-generation",
    "postcondition-failed",
    "semantic-blocked",
];

const BLOCKER_ROUTES: &[&str] = &["self", "parent", "lead"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverPayload {
    pub version: u32,
    pub world_fingerprint: String,
    #[serde(default)]
    pub generation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub version: u32,
    pub world_fingerprint: String,
    pub brain_revision: String,
    pub input_fingerprint: String,
    #[serde(default)]
    pub generation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Processed {
    pub version: u32,
    pub input_fingerprint: String,
    pub outcome: String,
    pub processed_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub version: u32,
    #[serde(alias = "fingerprint")]
    pub input_fingerprint: String,
    #[serde(default)]
    pub generation: Option<String>,
    #[serde(default)]
    pub output_revision: Option<String>,
    pub checkpointed_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocker {
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub input_fingerprint: Option<String>,
    pub kind: String,
    #[serde(
        default = "default_blocker_route",
        skip_serializing_if = "is_self_route"
    )]
    pub route: String,
    pub at: String,
    #[serde(default)]
    pub iteration_id: Option<String>,
    #[serde(default)]
    pub code: Option<i32>,
    pub summary: String,
}

fn default_blocker_route() -> String {
    "self".into()
}

fn is_self_route(value: &str) -> bool {
    value == "self"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotifyEvent {
    Checkpoint,
    Blocked,
    Completed,
}

impl NotifyEvent {
    fn parse(raw: Option<&str>) -> Result<Self, BackendError> {
        match raw.unwrap_or("checkpoint") {
            "checkpoint" => Ok(Self::Checkpoint),
            "blocked" => Ok(Self::Blocked),
            "completed" => Ok(Self::Completed),
            other => Err(BackendError(format!(
                "unknown notify event '{other}' (known: checkpoint, blocked, completed)"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Checkpoint => "checkpoint",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationConfig {
    pub exec: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationMetadata {
    pub version: u32,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub brain_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<ObservationConfig>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub output_revision_required: bool,
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

pub fn preferred_operation_home(root: &Path, stem: &str) -> PathBuf {
    root.join(DIR_NAME).join(OPERATIONS_DIR).join(stem)
}

pub fn legacy_operation_home(root: &Path, stem: &str) -> PathBuf {
    root.join(DIR_NAME).join(stem)
}

pub fn operation_home_checked(root: &Path, stem: &str) -> Result<PathBuf, BackendError> {
    let preferred = preferred_operation_home(root, stem);
    let legacy = legacy_operation_home(root, stem);
    if preferred.exists() && legacy.exists() {
        return Err(BackendError(format!(
            "ambiguous operation home for '{stem}': both {} and {} exist",
            preferred.display(),
            legacy.display()
        )));
    }
    Ok(if preferred.exists() {
        preferred
    } else if legacy.exists() {
        legacy
    } else {
        preferred
    })
}

pub fn operation_home(root: &Path, stem: &str) -> PathBuf {
    operation_home_checked(root, stem).unwrap_or_else(|_| preferred_operation_home(root, stem))
}

pub fn metadata_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("automation.toml")
}

pub fn lifecycle_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/lifecycle.json")
}

pub fn observation_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/observation.json")
}

pub fn processed_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/processed.json")
}

pub fn checkpoint_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/checkpoint.json")
}

pub fn blocker_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/blocker.json")
}

pub fn fingerprint_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/fingerprint")
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
fn validate_observer_value(label: &str, value: &str) -> Result<String, BackendError> {
    if value.is_empty()
        || value.chars().count() > MAX_OBSERVER_VALUE
        || value.contains(['\n', '\r', '\0'])
    {
        return Err(BackendError(format!(
            "{label} must be one non-empty line of at most {MAX_OBSERVER_VALUE} characters"
        )));
    }
    Ok(value.to_string())
}

fn validate_observation_config(config: &ObservationConfig) -> Result<(), BackendError> {
    validate_relative_path("observation.exec", &config.exec)?;
    if config.args.len() > MAX_OBSERVER_ARGS {
        return Err(BackendError(format!(
            "observation.args is limited to {MAX_OBSERVER_ARGS} entries"
        )));
    }
    for arg in &config.args {
        if arg.chars().count() > MAX_OBSERVER_VALUE || arg.contains(['\n', '\r', '\0']) {
            return Err(BackendError(format!(
                "observation.args entries must be one line of at most {MAX_OBSERVER_VALUE} characters"
            )));
        }
    }
    Ok(())
}

pub fn parse_observer_payload(bytes: &[u8]) -> Result<ObserverPayload, BackendError> {
    let payload: ObserverPayload = serde_json::from_slice(bytes).map_err(|error| {
        BackendError(format!(
            "observer output is not one valid JSON object: {error}"
        ))
    })?;
    if payload.version != 1 {
        return Err(BackendError("observer version must be 1".into()));
    }
    let world_fingerprint =
        validate_observer_value("world_fingerprint", &payload.world_fingerprint)?;
    let generation = payload
        .generation
        .as_deref()
        .map(|value| validate_observer_value("generation", value))
        .transpose()?;
    Ok(ObserverPayload {
        version: 1,
        world_fingerprint,
        generation,
    })
}

pub fn effective_input_fingerprint(world_fingerprint: &str, brain_revision: &str) -> String {
    sha256_hex(format!("world={world_fingerprint}\nbrain={brain_revision}\n").as_bytes())
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
    if let Some(observation) = &metadata.observation {
        validate_observation_config(observation)?;
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

fn read_optional_bytes(path: &Path) -> Result<Option<Vec<u8>>, BackendError> {
    refuse_symlink(path)?;
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(BackendError(format!(
            "cannot read {}: {error}",
            path.display()
        ))),
    }
}

fn validate_opaque(label: &str, value: &str) -> Result<String, BackendError> {
    crate::operator::strict_line(label, value, MAX_OPAQUE)
}

fn optional_opaque(label: &str, value: Option<&str>) -> Result<Option<String>, BackendError> {
    match value {
        None => Ok(None),
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) => Ok(Some(validate_opaque(label, value)?)),
    }
}

fn validate_checkpoint(checkpoint: Checkpoint) -> Result<Checkpoint, BackendError> {
    if checkpoint.version != 1 && checkpoint.version != 2 {
        return Err(BackendError("checkpoint version must be 1 or 2".into()));
    }
    let input_fingerprint = validate_opaque(
        "checkpoint input_fingerprint",
        &checkpoint.input_fingerprint,
    )?;
    let generation = match checkpoint.generation {
        Some(value) => optional_opaque("checkpoint generation", Some(&value))?,
        None => None,
    };
    let output_revision = match checkpoint.output_revision {
        Some(value) => optional_opaque("checkpoint output_revision", Some(&value))?,
        None => None,
    };
    let checkpointed_at =
        crate::operator::strict_line("checkpointed_at", &checkpoint.checkpointed_at, MAX_OPAQUE)?;
    Ok(Checkpoint {
        version: checkpoint.version,
        input_fingerprint,
        generation,
        output_revision,
        checkpointed_at,
    })
}

fn validate_blocker(blocker: Blocker) -> Result<Blocker, BackendError> {
    if blocker.version != 1 && blocker.version != 2 {
        return Err(BackendError("blocker version must be 1 or 2".into()));
    }
    let id = validate_opaque("blocker id", &blocker.id)?;
    let input_fingerprint = blocker
        .input_fingerprint
        .as_deref()
        .map(|value| validate_opaque("blocker input_fingerprint", value))
        .transpose()?;
    if blocker.version == 2 && input_fingerprint.is_none() {
        return Err(BackendError(
            "blocker version 2 requires input_fingerprint".into(),
        ));
    }
    let kind = crate::operator::strict_line("blocker kind", &blocker.kind, MAX_BLOCKER_KIND)?;
    if !BLOCKER_KINDS.contains(&kind.as_str()) {
        return Err(BackendError(format!(
            "unknown blocker kind '{kind}' (known: {})",
            BLOCKER_KINDS.join(", ")
        )));
    }
    let route = if blocker.route.trim().is_empty() {
        "self".to_string()
    } else {
        crate::operator::strict_line("blocker route", &blocker.route, 16)?
    };
    if !BLOCKER_ROUTES.contains(&route.as_str()) {
        return Err(BackendError(format!(
            "unknown blocker route '{route}' (known: self, parent, lead)"
        )));
    }
    let at = crate::operator::strict_line("blocker at", &blocker.at, MAX_OPAQUE)?;
    let iteration_id = match blocker.iteration_id {
        Some(value) if value.trim().is_empty() => None,
        Some(value) => Some(crate::operator::strict_line(
            "blocker iteration_id",
            &value,
            MAX_ITERATION_ID,
        )?),
        None => None,
    };
    let summary =
        crate::operator::strict_line("blocker summary", &blocker.summary, MAX_BLOCKER_SUMMARY)?;
    Ok(Blocker {
        version: blocker.version,
        id,
        input_fingerprint,
        kind,
        route,
        at,
        iteration_id,
        code: blocker.code,
        summary,
    })
}

pub fn load_observation(root: &Path, stem: &str) -> Result<Option<Observation>, BackendError> {
    let path = observation_path(root, stem);
    let Some(bytes) = read_optional_bytes(&path)? else {
        return Ok(None);
    };
    let observation: Observation = serde_json::from_slice(&bytes)
        .map_err(|error| BackendError(format!("malformed {}: {error}", path.display())))?;
    if observation.version != 1 {
        return Err(BackendError("observation version must be 1".into()));
    }
    validate_observer_value("world_fingerprint", &observation.world_fingerprint)?;
    validate_observer_value("brain_revision", &observation.brain_revision)?;
    validate_opaque("input_fingerprint", &observation.input_fingerprint)?;
    if let Some(generation) = &observation.generation {
        validate_observer_value("generation", generation)?;
    }
    Ok(Some(observation))
}

pub fn load_processed(root: &Path, stem: &str) -> Result<Option<Processed>, BackendError> {
    let path = processed_path(root, stem);
    let Some(bytes) = read_optional_bytes(&path)? else {
        return Ok(None);
    };
    let processed: Processed = serde_json::from_slice(&bytes)
        .map_err(|error| BackendError(format!("malformed {}: {error}", path.display())))?;
    if processed.version != 1 || !matches!(processed.outcome.as_str(), "ready" | "blocked") {
        return Err(BackendError(
            "processed state requires version 1 and outcome ready|blocked".into(),
        ));
    }
    validate_opaque("processed input_fingerprint", &processed.input_fingerprint)?;
    crate::operator::strict_line("processed_at", &processed.processed_at, MAX_OPAQUE)?;
    Ok(Some(processed))
}

pub fn load_checkpoint(root: &Path, stem: &str) -> Result<Option<Checkpoint>, BackendError> {
    let path = checkpoint_path(root, stem);
    let Some(bytes) = read_optional_bytes(&path)? else {
        return Ok(None);
    };
    let checkpoint: Checkpoint = serde_json::from_slice(&bytes)
        .map_err(|error| BackendError(format!("malformed {}: {error}", path.display())))?;
    validate_checkpoint(checkpoint).map(Some)
}

pub fn load_blocker(root: &Path, stem: &str) -> Result<Option<Blocker>, BackendError> {
    let path = blocker_path(root, stem);
    let Some(bytes) = read_optional_bytes(&path)? else {
        return Ok(None);
    };
    let blocker: Blocker = serde_json::from_slice(&bytes)
        .map_err(|error| BackendError(format!("malformed {}: {error}", path.display())))?;
    validate_blocker(blocker).map(Some)
}

pub fn load_processed_fingerprint(root: &Path, stem: &str) -> Result<Option<String>, BackendError> {
    if let Some(processed) = load_processed(root, stem)? {
        return Ok(Some(processed.input_fingerprint));
    }
    let path = fingerprint_path(root, stem);
    let Some(bytes) = read_optional_bytes(&path)? else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes)
        .map_err(|error| BackendError(format!("malformed {}: {error}", path.display())))?;
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(validate_opaque("processed fingerprint", text)?))
}
fn compact_event_id(
    kind: &str,
    route: &str,
    input_fingerprint: Option<&str>,
    iteration_id: Option<&str>,
    code: Option<i32>,
) -> String {
    let mut input = format!("{kind}\0{route}\0");
    if let Some(input_fingerprint) = input_fingerprint {
        input.push_str(input_fingerprint);
    }
    input.push('\0');
    if let Some(iteration_id) = iteration_id {
        input.push_str(iteration_id);
    }
    input.push('\0');
    if let Some(code) = code {
        input.push_str(&code.to_string());
    }
    let digest = sha256_hex(input.as_bytes());
    format!("blk-{}", &digest[..12])
}

fn serialize_pretty(value: &impl Serialize) -> Result<Vec<u8>, BackendError> {
    serde_json::to_vec_pretty(value)
        .map_err(|error| BackendError(format!("cannot serialize automation state: {error}")))
}

pub fn write_observation(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    world_fingerprint: &str,
    brain_revision: &str,
    generation: Option<&str>,
) -> Result<Value, BackendError> {
    let (manifest, stem) = crate::operator::bound_operation_manifest(explicit_root, cwd)?;
    let observation = Observation {
        version: 1,
        world_fingerprint: validate_observer_value("world_fingerprint", world_fingerprint)?,
        brain_revision: validate_observer_value("brain_revision", brain_revision)?,
        input_fingerprint: effective_input_fingerprint(world_fingerprint, brain_revision),
        generation: generation
            .map(|value| validate_observer_value("generation", value))
            .transpose()?,
    };
    let path = observation_path(&manifest.root, &stem);
    let bytes = serialize_pretty(&observation)?;
    if read_optional_bytes(&path)?.as_deref() != Some(bytes.as_slice()) {
        atomic_write(&path, &bytes)?;
    }
    Ok(json!({ "unit": stem, "observation": observation }))
}

pub fn write_processed(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    input_fingerprint: &str,
    outcome: &str,
) -> Result<Value, BackendError> {
    let (manifest, stem) = crate::operator::bound_operation_manifest(explicit_root, cwd)?;
    if !matches!(outcome, "ready" | "blocked") {
        return Err(BackendError(
            "processed outcome must be ready or blocked".into(),
        ));
    }
    let processed = Processed {
        version: 1,
        input_fingerprint: validate_opaque("processed input_fingerprint", input_fingerprint)?,
        outcome: outcome.to_string(),
        processed_at: crate::config::unix_to_rfc3339(crate::config::now_unix()),
    };
    atomic_write(
        &processed_path(&manifest.root, &stem),
        &serialize_pretty(&processed)?,
    )?;
    let legacy = fingerprint_path(&manifest.root, &stem);
    refuse_symlink(&legacy)?;
    let _ = fs::remove_file(legacy);
    Ok(json!({ "unit": stem, "processed": processed }))
}

pub fn write_checkpoint(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    input_fingerprint: &str,
    generation: Option<&str>,
    output_revision: Option<&str>,
) -> Result<Value, BackendError> {
    let (manifest, stem) = crate::operator::bound_operation_manifest(explicit_root, cwd)?;
    let checkpoint = validate_checkpoint(Checkpoint {
        version: 2,
        input_fingerprint: input_fingerprint.to_string(),
        generation: generation.map(str::to_string),
        output_revision: output_revision.map(str::to_string),
        checkpointed_at: crate::config::unix_to_rfc3339(crate::config::now_unix()),
    })?;
    atomic_write(
        &checkpoint_path(&manifest.root, &stem),
        &serialize_pretty(&checkpoint)?,
    )?;
    let _ = fs::remove_file(blocker_path(&manifest.root, &stem));
    Ok(json!({
        "unit": stem,
        "checkpoint": checkpoint,
        "blocker": Value::Null,
    }))
}

#[allow(clippy::too_many_arguments)]
pub fn write_blocker(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    kind: &str,
    route: Option<&str>,
    input_fingerprint: Option<&str>,
    iteration_id: Option<&str>,
    code: Option<i32>,
    summary: &str,
) -> Result<Value, BackendError> {
    let (manifest, stem) = crate::operator::bound_operation_manifest(explicit_root, cwd)?;
    let route = route.unwrap_or("self");
    let input_fingerprint = match input_fingerprint {
        Some(value) => Some(validate_opaque("blocker input_fingerprint", value)?),
        None => load_observation(&manifest.root, &stem)?.map(|value| value.input_fingerprint),
    };
    let existing = load_blocker(&manifest.root, &stem)?;
    let id = compact_event_id(
        kind,
        route,
        input_fingerprint.as_deref(),
        iteration_id,
        code,
    );
    if let Some(existing) = existing.as_ref() {
        if existing.id == id {
            return Ok(json!({
                "unit": stem,
                "changed": false,
                "blocker": existing,
            }));
        }
    }
    let version = if input_fingerprint.is_some() { 2 } else { 1 };
    let blocker = validate_blocker(Blocker {
        version,
        id,
        input_fingerprint,
        kind: kind.to_string(),
        route: route.to_string(),
        at: crate::config::unix_to_rfc3339(crate::config::now_unix()),
        iteration_id: iteration_id.map(str::to_string),
        code,
        summary: summary.to_string(),
    })?;
    atomic_write(
        &blocker_path(&manifest.root, &stem),
        &serialize_pretty(&blocker)?,
    )?;
    Ok(json!({
        "unit": stem,
        "changed": true,
        "blocker": blocker,
    }))
}

pub fn clear_blocker(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let (manifest, stem) = crate::operator::bound_operation_manifest(explicit_root, cwd)?;
    let path = blocker_path(&manifest.root, &stem);
    refuse_symlink(&path)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(json!({ "unit": stem, "cleared": true })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(json!({ "unit": stem, "cleared": false }))
        }
        Err(error) => Err(BackendError(format!(
            "cannot clear {}: {error}",
            path.display()
        ))),
    }
}

fn checkpoint_json(checkpoint: Option<&Checkpoint>) -> Value {
    match checkpoint {
        Some(checkpoint) => json!({
            "present": true,
            "kind": "structured",
            "version": checkpoint.version,
            "input_fingerprint": checkpoint.input_fingerprint,
            "fingerprint": checkpoint.input_fingerprint,
            "generation": checkpoint.generation,
            "output_revision": checkpoint.output_revision,
            "checkpointed_at": checkpoint.checkpointed_at,
        }),
        None => json!({
            "present": false,
            "kind": Value::Null,
            "version": Value::Null,
            "input_fingerprint": Value::Null,
            "fingerprint": Value::Null,
            "generation": Value::Null,
            "output_revision": Value::Null,
            "checkpointed_at": Value::Null,
        }),
    }
}

fn blocker_json(blocker: Option<&Blocker>) -> Value {
    match blocker {
        Some(blocker) => json!({
            "version": blocker.version,
            "id": blocker.id,
            "input_fingerprint": blocker.input_fingerprint,
            "kind": blocker.kind,
            "route": blocker.route,
            "at": blocker.at,
            "iteration_id": blocker.iteration_id,
            "code": blocker.code,
            "summary": blocker.summary,
        }),
        None => Value::Null,
    }
}

fn processed_json(processed: Option<&Processed>, legacy: Option<&str>) -> Value {
    match processed {
        Some(processed) => json!({
            "version": processed.version,
            "input_fingerprint": processed.input_fingerprint,
            "fingerprint": processed.input_fingerprint,
            "outcome": processed.outcome,
            "processed_at": processed.processed_at,
            "legacy": false,
        }),
        None => json!({
            "version": Value::Null,
            "input_fingerprint": legacy,
            "fingerprint": legacy,
            "outcome": Value::Null,
            "processed_at": Value::Null,
            "legacy": legacy.is_some(),
        }),
    }
}

fn latest_iteration_json(operator: &Value) -> Value {
    let latest = operator
        .get("iterations")
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    match latest {
        Some(item) => json!({
            "id": item.get("id").cloned().unwrap_or(Value::Null),
            "exit_code": item.get("exit_code").cloned().unwrap_or(Value::Null),
            "reconsolidated": item.get("reconsolidated").cloned().unwrap_or(Value::Null),
            "finished_at": item.get("finished_at").cloned().unwrap_or(Value::Null),
        }),
        None => Value::Null,
    }
}

struct ChildRevisionInput<'a> {
    unit: &'a str,
    agent: Option<&'a str>,
    lifecycle: &'a str,
    processed: Option<&'a str>,
    checkpoint: Option<&'a Checkpoint>,
    blocker: Option<&'a Blocker>,
}

fn child_revision(input: ChildRevisionInput<'_>) -> String {
    let mut buf = Vec::new();
    buf.extend_from_slice(input.unit.as_bytes());
    buf.push(0);
    if let Some(agent) = input.agent {
        buf.extend_from_slice(agent.as_bytes());
    }
    buf.push(0);
    buf.extend_from_slice(input.lifecycle.as_bytes());
    buf.push(0);
    if let Some(processed) = input.processed {
        buf.extend_from_slice(processed.as_bytes());
    }
    buf.push(0);
    if let Some(checkpoint) = input.checkpoint {
        buf.extend_from_slice(checkpoint.input_fingerprint.as_bytes());

        buf.push(0);
        if let Some(generation) = &checkpoint.generation {
            buf.extend_from_slice(generation.as_bytes());
        }
        buf.push(0);
        if let Some(output_revision) = &checkpoint.output_revision {
            buf.extend_from_slice(output_revision.as_bytes());
        }
    }
    buf.push(0);
    if let Some(blocker) = input.blocker {
        buf.extend_from_slice(blocker.id.as_bytes());
        buf.push(0);
        buf.extend_from_slice(blocker.kind.as_bytes());
    }
    format!("sha256:{}", sha256_hex(&buf))
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
pub fn run_observer(explicit_root: Option<&str>, cwd: Option<&str>) -> Result<Value, BackendError> {
    let (manifest, stem) = crate::operator::bound_operation_manifest(explicit_root, cwd)?;
    let metadata = load_metadata(&manifest.root, &stem)?
        .ok_or_else(|| BackendError(format!("automation '{stem}' has no automation.toml")))?;
    let observation = metadata
        .observation
        .as_ref()
        .ok_or_else(|| BackendError(format!("automation '{stem}' has no [observation] config")))?;
    let scope_dir = manifest.root.join(DIR_NAME);
    let executable = scope_dir.join(&observation.exec);
    let executable_meta = fs::symlink_metadata(&executable).map_err(|error| {
        BackendError(format!(
            "cannot inspect observer {}: {error}",
            executable.display()
        ))
    })?;
    if executable_meta.file_type().is_symlink() || !executable_meta.is_file() {
        return Err(BackendError(format!(
            "observer must be a regular non-symlink file: {}",
            executable.display()
        )));
    }
    let operation = crate::operations::get_operation_any(&stem)?;
    let operation_cwd = operation
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| manifest.root.to_str().unwrap_or("."));
    let home = operation_home_checked(&manifest.root, &stem)?;
    let output = Command::new(&executable)
        .args(&observation.args)
        .env("SYSTEMD_OPS_SCOPE_ROOT", &manifest.root)
        .env("SYSTEMD_OPS_OPERATION", &stem)
        .env("SYSTEMD_OPS_OPERATION_HOME", &home)
        .env("SYSTEMD_OPS_CWD", operation_cwd)
        .output()
        .map_err(|error| {
            BackendError(format!(
                "cannot execute observer {}: {error}",
                executable.display()
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(BackendError(format!(
            "observer {} exited {}{}",
            executable.display(),
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        )));
    }
    let payload = parse_observer_payload(&output.stdout)?;
    let brain_revision = brain_revision(&manifest, &stem)?;
    write_observation(
        explicit_root,
        cwd,
        &payload.world_fingerprint,
        &brain_revision,
        payload.generation.as_deref(),
    )
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
    let observation = match load_observation(&manifest.root, stem) {
        Ok(observation) => observation,
        Err(error) => return (Value::Null, Some(error.0)),
    };
    let processed = match load_processed(&manifest.root, stem) {
        Ok(processed) => processed,
        Err(error) => return (Value::Null, Some(error.0)),
    };
    let legacy_processed = if processed.is_none() {
        match load_processed_fingerprint(&manifest.root, stem) {
            Ok(processed) => processed,
            Err(error) => return (Value::Null, Some(error.0)),
        }
    } else {
        None
    };
    let checkpoint = match load_checkpoint(&manifest.root, stem) {
        Ok(checkpoint) => checkpoint,
        Err(error) => return (Value::Null, Some(error.0)),
    };
    let blocker = match load_blocker(&manifest.root, stem) {
        Ok(blocker) => blocker,
        Err(error) => return (Value::Null, Some(error.0)),
    };
    let mut warning = None;
    let Some(metadata) = metadata else {
        return (
            json!({
                "agent": Value::Null,
                "agent_root": manifest.automation_agent_root.as_ref().map(|path| path.to_string_lossy()),
                "parent": Value::Null,
                "brain_paths": [],
                "brain_revision": Value::Null,
                "observation_config": Value::Null,
                "observation": observation,
                "output_revision_required": false,
                "lifecycle": lifecycle_json(lifecycle.as_ref()),
                "checkpoint": checkpoint_json(checkpoint.as_ref()),
                "processed": processed_json(processed.as_ref(), legacy_processed.as_deref()),
                "blocker": blocker_json(blocker.as_ref()),
                "semantic_state": Value::Null,
            }),
            None,
        );
    };
    let revision = brain_revision(manifest, stem);
    if let Err(error) = &revision {
        warning = Some(error.0.clone());
    }
    (
        json!({
            "agent": metadata.agent,
            "agent_root": manifest.automation_agent_root.as_ref().map(|path| path.to_string_lossy()),
            "parent": metadata.parent,
            "brain_paths": metadata.brain_paths,
            "brain_revision": revision.ok(),
            "observation_config": metadata.observation,
            "observation": observation,
            "output_revision_required": metadata.output_revision_required,
            "lifecycle": lifecycle_json(lifecycle.as_ref()),
            "checkpoint": checkpoint_json(checkpoint.as_ref()),
            "processed": processed_json(processed.as_ref(), legacy_processed.as_deref()),
            "blocker": blocker_json(blocker.as_ref()),
            "semantic_state": "stale",
        }),
        warning,
    )
}

fn relation_summary(operation: &Value) -> Value {
    let operator = operation.get("operator").unwrap_or(&Value::Null);
    let automation = operation.get("automation").unwrap_or(&Value::Null);
    let lifecycle = automation
        .get("lifecycle")
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("active");
    let state = operation.get("state").and_then(Value::as_str).unwrap_or("");
    let sub = operation.get("sub").and_then(Value::as_str).unwrap_or("");
    let running = state == "active" && sub == "running";
    let active_iteration = operator.get("active_iteration").is_some_and(|value| {
        !value.is_null()
            && value
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
    });
    let agent = automation.get("agent").and_then(Value::as_str);
    let checkpoint = automation.get("checkpoint").cloned().unwrap_or_else(|| {
        json!({
            "present": false,
            "kind": Value::Null,
            "fingerprint": Value::Null,
            "generation": Value::Null,
            "output_revision": Value::Null,
            "checkpointed_at": Value::Null,
        })
    });
    let processed = automation.get("processed").cloned().unwrap_or_else(|| {
        json!({
            "input_fingerprint": Value::Null,
            "fingerprint": Value::Null,
            "outcome": Value::Null,
            "legacy": false,
        })
    });

    let blocker = automation.get("blocker").cloned().unwrap_or(Value::Null);
    let structured = checkpoint.get("present").and_then(Value::as_bool) == Some(true)
        && checkpoint.get("kind").and_then(Value::as_str) == Some("structured");
    let unit = operation.get("unit").and_then(Value::as_str).unwrap_or("");
    let processed_fingerprint = processed
        .get("input_fingerprint")
        .or_else(|| processed.get("fingerprint"))
        .and_then(Value::as_str);

    let checkpoint_record = if structured {
        Some(Checkpoint {
            version: checkpoint
                .get("version")
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32,
            input_fingerprint: checkpoint
                .get("input_fingerprint")
                .or_else(|| checkpoint.get("fingerprint"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            generation: checkpoint
                .get("generation")
                .and_then(Value::as_str)
                .map(str::to_string),
            output_revision: checkpoint
                .get("output_revision")
                .and_then(Value::as_str)
                .map(str::to_string),
            checkpointed_at: checkpoint
                .get("checkpointed_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    } else {
        None
    };
    let blocker_record = blocker.as_object().map(|object| Blocker {
        version: object.get("version").and_then(Value::as_u64).unwrap_or(1) as u32,
        id: object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        input_fingerprint: object
            .get("input_fingerprint")
            .and_then(Value::as_str)
            .map(str::to_string),
        kind: object
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        route: object
            .get("route")
            .and_then(Value::as_str)
            .unwrap_or("self")
            .to_string(),
        at: object
            .get("at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        iteration_id: object
            .get("iteration_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        code: object.get("code").and_then(Value::as_i64).map(|n| n as i32),
        summary: object
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    });
    json!({
        "unit": operation.get("unit").cloned().unwrap_or(Value::Null),
        "title": operation.get("title").cloned().unwrap_or(Value::Null),
        "agent": agent.map(Value::from).unwrap_or(Value::Null),
        "health": operation.get("health").cloned().unwrap_or(Value::Null),
        "semantic_state": automation.get("semantic_state").cloned().unwrap_or(Value::Null),
        "lifecycle": lifecycle,
        "running": running,
        "active_iteration": active_iteration,
        "headline": operator.get("headline").cloned().unwrap_or(Value::Null),
        "observation": automation.get("observation").cloned().unwrap_or(Value::Null),
        "processed": processed,
        "checkpoint": checkpoint,
        "blocker": blocker,
        "latest_iteration": latest_iteration_json(operator),
        "child_revision": child_revision(ChildRevisionInput {
            unit,
            agent,
            lifecycle,
            processed: processed_fingerprint,
            checkpoint: checkpoint_record.as_ref(),
            blocker: blocker_record.as_ref(),
        }),

    })
}

fn active_iteration(operation: &Value) -> bool {
    operation
        .get("operator")
        .and_then(|value| value.get("active_iteration"))
        .is_some_and(|value| !value.is_null())
}

fn processed_is_current_blocked(operation: &Value) -> bool {
    let automation = operation.get("automation").unwrap_or(&Value::Null);
    let observation_input = automation
        .get("observation")
        .and_then(|value| value.get("input_fingerprint"))
        .and_then(Value::as_str);
    let processed = automation.get("processed").unwrap_or(&Value::Null);
    processed.get("legacy").and_then(Value::as_bool) != Some(true)
        && processed.get("outcome").and_then(Value::as_str) == Some("blocked")
        && observation_input.is_some()
        && processed.get("input_fingerprint").and_then(Value::as_str) == observation_input
}

fn blocker_is_current(operation: &Value) -> bool {
    let automation = operation.get("automation").unwrap_or(&Value::Null);
    let blocker = automation.get("blocker").unwrap_or(&Value::Null);
    if blocker.is_null() {
        return processed_is_current_blocked(operation);
    }
    let observation_input = automation
        .get("observation")
        .and_then(|value| value.get("input_fingerprint"))
        .and_then(Value::as_str);
    match blocker.get("input_fingerprint").and_then(Value::as_str) {
        Some(input) => observation_input == Some(input),
        None => {
            let structured_processed = automation
                .get("processed")
                .and_then(|value| value.get("legacy"))
                .and_then(Value::as_bool)
                == Some(false)
                && automation
                    .get("processed")
                    .and_then(|value| value.get("input_fingerprint"))
                    .and_then(Value::as_str)
                    .is_some();
            !structured_processed
        }
    }
}

fn checkpoint_is_current(operation: &Value) -> bool {
    let automation = operation.get("automation").unwrap_or(&Value::Null);
    let observation = automation.get("observation").unwrap_or(&Value::Null);
    let checkpoint = automation.get("checkpoint").unwrap_or(&Value::Null);
    let input = observation.get("input_fingerprint").and_then(Value::as_str);
    let checkpoint_input = checkpoint
        .get("input_fingerprint")
        .or_else(|| checkpoint.get("fingerprint"))
        .and_then(Value::as_str);
    if checkpoint.get("present").and_then(Value::as_bool) != Some(true)
        || input.is_none()
        || checkpoint_input != input
    {
        return false;
    }
    if let Some(generation) = observation.get("generation").and_then(Value::as_str) {
        if checkpoint.get("generation").and_then(Value::as_str) != Some(generation) {
            return false;
        }
    }
    let output_required = automation
        .get("output_revision_required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    !output_required
        || checkpoint
            .get("output_revision")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
}

pub fn derive_semantic_states(operations: &mut [Value]) {
    let indexes: BTreeMap<String, usize> = operations
        .iter()
        .enumerate()
        .filter_map(|(index, operation)| {
            operation
                .get("unit")
                .and_then(Value::as_str)
                .map(|unit| (unit.to_string(), index))
        })
        .collect();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for operation in operations.iter() {
        let Some(unit) = operation.get("unit").and_then(Value::as_str) else {
            continue;
        };
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
            .push(unit.to_string());
    }
    fn derive(
        unit: &str,
        operations: &[Value],
        indexes: &BTreeMap<String, usize>,
        children: &BTreeMap<String, Vec<String>>,
        memo: &mut BTreeMap<String, &'static str>,
        visiting: &mut BTreeSet<String>,
    ) -> &'static str {
        if let Some(state) = memo.get(unit) {
            return state;
        }
        if !visiting.insert(unit.to_string()) {
            return "stale";
        }
        let Some(operation) = indexes.get(unit).and_then(|index| operations.get(*index)) else {
            visiting.remove(unit);
            return "stale";
        };
        let agent_backed = operation
            .get("automation")
            .and_then(|value| value.get("agent"))
            .and_then(Value::as_str)
            .is_some();
        let state = if !agent_backed {
            "neutral"
        } else if active_iteration(operation) {
            "running"
        } else if blocker_is_current(operation) || processed_is_current_blocked(operation) {
            "blocked"
        } else {
            let active_children: Vec<&str> = children
                .get(unit)
                .into_iter()
                .flatten()
                .filter_map(|child| {
                    let child_operation = indexes
                        .get(child)
                        .and_then(|index| operations.get(*index))?;
                    let completed = child_operation
                        .get("automation")
                        .and_then(|value| value.get("lifecycle"))
                        .and_then(|value| value.get("status"))
                        .and_then(Value::as_str)
                        == Some("completed");
                    (!completed).then_some(child.as_str())
                })
                .collect();
            let waiting = !active_children.is_empty()
                && active_children.iter().any(|child| {
                    derive(child, operations, indexes, children, memo, visiting) != "ready"
                });
            if waiting {
                "waiting"
            } else if checkpoint_is_current(operation) {
                "ready"
            } else {
                "stale"
            }
        };
        visiting.remove(unit);
        memo.insert(unit.to_string(), state);
        state
    }
    let units: Vec<String> = indexes.keys().cloned().collect();
    let mut memo = BTreeMap::new();
    for unit in &units {
        let mut visiting = BTreeSet::new();
        derive(
            unit,
            operations,
            &indexes,
            &children,
            &mut memo,
            &mut visiting,
        );
    }
    for operation in operations {
        let Some(unit) = operation.get("unit").and_then(Value::as_str) else {
            continue;
        };
        if let Some(state) = memo.get(unit) {
            let current_blocker = blocker_is_current(operation);
            operation["automation"]["semantic_state"] = json!(state);
            if let Some(blocker) = operation["automation"].get_mut("blocker") {
                if !blocker.is_null() {
                    blocker["current"] = json!(current_blocker);
                }
            }
        }
    }
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
    let observation = match spec.get("observation") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<ObservationConfig>(value.clone())
                .map_err(|error| BackendError(format!("invalid observation: {error}")))?,
        ),
    };
    let output_revision_required = spec
        .get("output_revision_required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(agent) = agent else {
        if parent.is_some()
            || !brain_paths.is_empty()
            || observation.is_some()
            || output_revision_required
        {
            return Err(BackendError(
                "parent, brain_paths, observation, and output_revision_required require an agent-backed automation".into(),
            ));
        }
        return Ok(None);
    };
    normalize_metadata(AutomationMetadata {
        version: AUTOMATION_VERSION,
        agent: agent.to_string(),
        parent,
        brain_paths,
        observation,
        output_revision_required,
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
    object.remove("observation");
    object.remove("output_revision_required");
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

fn structured_checkpoint(operation: &Value) -> Option<&Value> {
    let checkpoint = operation
        .get("automation")
        .and_then(|value| value.get("checkpoint"))?;
    if checkpoint.get("present").and_then(Value::as_bool) == Some(true)
        && checkpoint.get("kind").and_then(Value::as_str) == Some("structured")
    {
        Some(checkpoint)
    } else {
        None
    }
}

fn current_blocker(operation: &Value) -> Option<&Value> {
    let blocker = operation
        .get("automation")
        .and_then(|value| value.get("blocker"))?;
    if blocker.is_null() {
        None
    } else {
        Some(blocker)
    }
}

fn lifecycle_status(operation: &Value) -> &str {
    operation
        .get("automation")
        .and_then(|value| value.get("lifecycle"))
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("active")
}

pub fn notify_parent(
    explicit_root: Option<&str>,
    cwd: Option<&str>,
    event: Option<&str>,
) -> Result<Value, BackendError> {
    let event = NotifyEvent::parse(event)?;
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
    match event {
        NotifyEvent::Checkpoint => {
            if structured_checkpoint(operation).is_none() {
                return Err(BackendError(format!(
                    "parent notification requires a structured output checkpoint for '{stem}'"
                )));
            }
        }
        NotifyEvent::Blocked => {
            if current_blocker(operation).is_none() {
                return Err(BackendError(format!(
                    "blocked parent notification requires a current blocker for '{stem}'"
                )));
            }
        }
        NotifyEvent::Completed => {
            if lifecycle_status(operation) != "completed" {
                return Err(BackendError(format!(
                    "completed parent notification requires completed lifecycle for '{stem}'"
                )));
            }
        }
    }
    let service = format!("{parent}.service");
    let changes = systemd::start_noblock(&service)?;
    Ok(json!({
        "notified": true,
        "event": event.as_str(),
        "child": stem,
        "parent": parent,
        "unit": service,
        "blocking": false,
        "checkpoint": structured_checkpoint(operation).cloned().unwrap_or(Value::Null),
        "blocker": current_blocker(operation).cloned().unwrap_or(Value::Null),
        "systemd": {
            "action": "start",
            "unit": service,
            "no_block": true,
            "changes": changes,
        },
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
            coordination_lead: None,
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
            observation: None,
            output_revision_required: false,
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
            observation: None,
            output_revision_required: false,
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
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >>'{log}'\ncase \" $* \" in\n  *\" list-unit-files \"*|*\" list-unit-files\"*) cat <<'EOF'\n{rows}\nEOF\n    ;;\n  *' show '*) printf 'LoadState=loaded\\nActiveState=inactive\\nSubState=dead\\n';;\n  *) : ;;\nesac\n",
                log = root.join("systemctl.log").display(),
                rows = serde_json::to_string(&rows).unwrap()
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
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            observation: None,
            output_revision_required: false,
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
            observation: None,
            output_revision_required: false,
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
            observation: None,
            output_revision_required: false,
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
        with_bound_operation(&root, "managed-proof", || {
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
        });
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

    fn bind_operation(root: &Path, stem: &str) {
        std::env::set_var("SYSTEMD_OPS_SCOPE_ROOT", root);
        std::env::set_var("SYSTEMD_OPS_OPERATION", stem);
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
        crate::config::set_current(crate::config::OpsConfig {
            manager: systemd::Manager::User,
            write_prefix: Some("managed-*".into()),
            plan_ttl_secs: 600,
            state_dir: root.join("state-dir"),
        });
        systemd::set_write_prefix(Some("managed-*".into()));
    }

    fn restore_bindings(
        previous_root: Option<std::ffi::OsString>,
        previous_op: Option<std::ffi::OsString>,
    ) {
        match previous_root {
            Some(value) => std::env::set_var("SYSTEMD_OPS_SCOPE_ROOT", value),
            None => std::env::remove_var("SYSTEMD_OPS_SCOPE_ROOT"),
        }
        match previous_op {
            Some(value) => std::env::set_var("SYSTEMD_OPS_OPERATION", value),
            None => std::env::remove_var("SYSTEMD_OPS_OPERATION"),
        }
        systemd::set_write_prefix(None);
    }

    fn with_bound_operation<T>(root: &Path, stem: &str, run: impl FnOnce() -> T) -> T {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_root = std::env::var_os("SYSTEMD_OPS_SCOPE_ROOT");
        let previous_op = std::env::var_os("SYSTEMD_OPS_OPERATION");
        bind_operation(root, stem);
        let result = run();
        restore_bindings(previous_root, previous_op);
        result
    }

    #[test]
    fn structured_checkpoint_is_atomic_and_bounded() {
        let root = tmp_root("checkpoint");
        with_bound_operation(&root, "managed-child", || {
            write_parent_fixture(&root, "managed-child", None);
            let written = write_checkpoint(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "fp-a",
                Some("gen-a"),
                Some("out-a"),
            )
            .unwrap();
            assert_eq!(written["checkpoint"]["generation"], "gen-a");
            assert_eq!(written["checkpoint"]["output_revision"], "out-a");
            let loaded = load_checkpoint(&root, "managed-child").unwrap().unwrap();
            assert_eq!(loaded.input_fingerprint, "fp-a");

            assert_eq!(loaded.generation.as_deref(), Some("gen-a"));
            assert_eq!(loaded.output_revision.as_deref(), Some("out-a"));
            let path = checkpoint_path(&root, "managed-child");
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        });
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_checkpoint_is_rejected() {
        let root = tmp_root("checkpoint-malformed");
        let path = checkpoint_path(&root, "managed-child");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{not-json").unwrap();
        let error = load_checkpoint(&root, "managed-child").unwrap_err();
        assert!(error.0.contains("malformed"), "{}", error.0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checkpoint_symlink_is_refused() {
        let root = tmp_root("checkpoint-symlink");
        with_bound_operation(&root, "managed-child", || {
            write_parent_fixture(&root, "managed-child", None);
            let path = checkpoint_path(&root, "managed-child");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let target = root.join("outside.json");
            fs::write(&target, b"{}\n").unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            let error = write_checkpoint(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "fp-a",
                Some("gen-a"),
                Some("out-a"),
            )
            .unwrap_err();
            assert!(error.0.contains("symlink"), "{}", error.0);
        });
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fingerprint_only_operation_is_legacy_not_structured() {
        let root = tmp_root("legacy-fingerprint");
        let path = fingerprint_path(&root, "managed-child");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"old-fingerprint\n").unwrap();
        assert!(load_checkpoint(&root, "managed-child").unwrap().is_none());
        let processed = load_processed_fingerprint(&root, "managed-child")
            .unwrap()
            .unwrap();
        assert_eq!(processed, "old-fingerprint");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blocker_write_does_not_overwrite_checkpoint() {
        let root = tmp_root("blocker-preserves-checkpoint");
        with_bound_operation(&root, "managed-child", || {
            write_parent_fixture(&root, "managed-child", None);
            write_checkpoint(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "fp-a",
                Some("gen-a"),
                Some("out-a"),
            )
            .unwrap();
            let before = fs::read(checkpoint_path(&root, "managed-child")).unwrap();
            let first = write_blocker(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "iteration-failed",
                None,
                None,
                Some("iter-1"),
                Some(7),
                "model failed",
            )
            .unwrap();
            assert_eq!(first["changed"], true);
            let second = write_blocker(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "iteration-failed",
                None,
                None,
                Some("iter-1"),
                Some(7),
                "model failed again",
            )
            .unwrap();
            assert_eq!(second["changed"], false);
            let after = fs::read(checkpoint_path(&root, "managed-child")).unwrap();
            assert_eq!(before, after);
            write_checkpoint(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "fp-b",
                Some("gen-b"),
                Some("out-b"),
            )
            .unwrap();
            assert!(load_blocker(&root, "managed-child").unwrap().is_none());
        });
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn notify_parent_requires_structured_checkpoint() {
        let root = tmp_root("notify");
        let error = with_user_unit_dir(&root, &["managed-child", "managed-parent"], || {
            bind_operation(&root, "managed-child");
            write_parent_fixture(&root, "managed-parent", None);
            write_parent_fixture(&root, "managed-child", Some("managed-parent"));
            notify_parent(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                None,
            )
            .unwrap_err()
        });
        assert!(
            error.0.contains("structured output checkpoint") || error.0.contains("no parent"),
            "{}",
            error.0
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn notify_parent_starts_parent_service_noblock() {
        let root = tmp_root("notify-start");
        let notified = with_user_unit_dir(&root, &["managed-child", "managed-parent"], || {
            bind_operation(&root, "managed-child");
            write_parent_fixture(&root, "managed-parent", None);
            write_parent_fixture(&root, "managed-child", Some("managed-parent"));
            write_checkpoint(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "fp-a",
                Some("gen-a"),
                Some("out-a"),
            )
            .unwrap();
            notify_parent(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                Some("checkpoint"),
            )
            .unwrap()
        });
        assert_eq!(notified["unit"], "managed-parent.service");
        assert_eq!(notified["blocking"], false);
        let calls = fs::read_to_string(root.join("systemctl.log")).unwrap();
        assert!(calls.contains("start --no-block"), "{calls}");
        assert!(calls.contains("managed-parent.service"), "{calls}");
        assert!(!calls.contains(".timer"), "{calls}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blocked_notify_does_not_require_checkpoint() {
        let root = tmp_root("notify-blocked");
        let notified = with_user_unit_dir(&root, &["managed-child", "managed-parent"], || {
            bind_operation(&root, "managed-child");
            write_parent_fixture(&root, "managed-parent", None);
            write_parent_fixture(&root, "managed-child", Some("managed-parent"));
            write_blocker(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "worktree-dirty",
                None,
                None,
                None,
                Some(4),
                "dedicated worktree is dirty",
            )
            .unwrap();
            notify_parent(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                Some("blocked"),
            )
            .unwrap()
        });
        assert_eq!(notified["event"], "blocked");
        assert_eq!(notified["blocking"], false);
        let calls = fs::read_to_string(root.join("systemctl.log")).unwrap();
        assert!(calls.contains("start --no-block"), "{calls}");
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
        assert!(child.get("child_revision").is_some());
        assert_eq!(child["checkpoint"]["present"], false);
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

    #[test]
    fn preferred_and_legacy_operation_homes_are_discovered_without_ambiguity() {
        let root = tmp_root("homes");
        let stem = "managed-proof";
        let preferred = preferred_operation_home(&root, stem);
        let legacy = legacy_operation_home(&root, stem);
        assert_eq!(operation_home_checked(&root, stem).unwrap(), preferred);
        fs::create_dir_all(&legacy).unwrap();
        assert_eq!(operation_home_checked(&root, stem).unwrap(), legacy);
        fs::create_dir_all(&preferred).unwrap();
        let error = operation_home_checked(&root, stem).unwrap_err();
        assert!(error.0.contains("ambiguous operation home"), "{}", error.0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn observer_payload_and_effective_input_are_strict_and_stable() {
        let payload = parse_observer_payload(
            br#"{"version":1,"world_fingerprint":"world-a","generation":null}"#,
        )
        .unwrap();
        assert_eq!(payload.world_fingerprint, "world-a");
        assert_eq!(payload.generation, None);
        assert!(
            parse_observer_payload(br#"{"version":2,"world_fingerprint":"x"}"#)
                .unwrap_err()
                .0
                .contains("version")
        );
        assert!(parse_observer_payload(br#"{"version":1,"world_fingerprint":""}"#).is_err());
        assert!(
            parse_observer_payload(b"{\"version\":1,\"world_fingerprint\":\"a\\nb\"}").is_err()
        );
        let first = effective_input_fingerprint("world-a", "brain-a");
        assert_eq!(first, effective_input_fingerprint("world-a", "brain-a"));
        assert_ne!(first, effective_input_fingerprint("world-a", "brain-b"));
    }

    #[test]
    fn structured_state_migrates_legacy_fingerprint_only_after_processing() {
        let root = tmp_root("processed-migration");
        with_bound_operation(&root, "managed-proof", || {
            write_parent_fixture(&root, "managed-proof", None);
            let legacy = fingerprint_path(&root, "managed-proof");
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, b"legacy-input\n").unwrap();
            assert_eq!(
                load_processed_fingerprint(&root, "managed-proof")
                    .unwrap()
                    .as_deref(),
                Some("legacy-input")
            );
            write_processed(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "current-input",
                "blocked",
            )
            .unwrap();
            assert!(!legacy.exists());
            let processed = load_processed(&root, "managed-proof").unwrap().unwrap();
            assert_eq!(processed.input_fingerprint, "current-input");
            assert_eq!(processed.outcome, "blocked");
        });
        let _ = fs::remove_dir_all(root);
    }

    #[allow(clippy::too_many_arguments)]
    fn semantic_operation(
        unit: &str,
        parent: Option<&str>,
        input: &str,
        generation: &str,
        checkpoint_input: Option<&str>,
        checkpoint_generation: Option<&str>,
        blocker_input: Option<&str>,
        active_iteration: bool,
        health: &str,
    ) -> Value {
        json!({
            "unit": unit,
            "health": health,
            "state": "inactive",
            "sub": "dead",
            "operator": {
                "active_iteration": active_iteration.then(|| json!({"id": "it-a"}))
            },
            "automation": {
                "agent": "proof-agent",
                "parent": parent,
                "lifecycle": {"status": "active"},
                "observation": {
                    "version": 1,
                    "world_fingerprint": "world",
                    "brain_revision": "brain",
                    "input_fingerprint": input,
                    "generation": generation
                },
                "output_revision_required": true,
                "processed": {"legacy": false, "input_fingerprint": input},
                "checkpoint": checkpoint_input.map(|checkpoint_input| json!({
                    "present": true,
                    "kind": "structured",
                    "input_fingerprint": checkpoint_input,
                    "generation": checkpoint_generation,
                    "output_revision": "out"
                })).unwrap_or_else(|| json!({"present": false})),
                "blocker": blocker_input.map(|blocker_input| json!({
                    "version": 2,
                    "id": "blk-a",
                    "input_fingerprint": blocker_input,
                    "kind": "semantic-blocked",
                    "route": "self"
                }))
            }
        })
    }

    #[test]
    fn semantic_state_precedence_keeps_health_separate() {
        let cases = [
            ("running", true, None, None, None, "failed"),
            ("blocked", false, Some("in"), None, None, "healthy"),
            ("ready", false, None, Some("in"), Some("gen"), "healthy"),
            ("stale", false, None, Some("old"), Some("gen"), "failed"),
            ("stale", false, None, Some("in"), Some("old"), "healthy"),
        ];
        for (expected, running, blocker, checkpoint, checkpoint_generation, health) in cases {
            let mut operations = vec![semantic_operation(
                "managed-proof",
                None,
                "in",
                "gen",
                checkpoint,
                checkpoint_generation,
                blocker,
                running,
                health,
            )];
            derive_semantic_states(&mut operations);
            assert_eq!(operations[0]["automation"]["semantic_state"], expected);
            assert_eq!(operations[0]["health"], health);
        }
    }

    #[test]
    fn parent_waits_for_required_child_and_zero_child_is_ready() {
        let parent = semantic_operation(
            "managed-parent",
            None,
            "parent-in",
            "gen",
            Some("parent-in"),
            Some("gen"),
            None,
            false,
            "healthy",
        );
        let stale_child = semantic_operation(
            "managed-child",
            Some("managed-parent"),
            "child-in",
            "gen",
            Some("old"),
            Some("gen"),
            None,
            false,
            "healthy",
        );
        let mut operations = vec![parent.clone(), stale_child];
        derive_semantic_states(&mut operations);
        assert_eq!(operations[0]["automation"]["semantic_state"], "waiting");

        let mut zero_child = vec![parent];
        derive_semantic_states(&mut zero_child);
        assert_eq!(zero_child[0]["automation"]["semantic_state"], "ready");
    }

    #[test]
    fn old_input_blocker_expires_without_deleting_history() {
        let mut operations = vec![semantic_operation(
            "managed-proof",
            None,
            "new-input",
            "gen",
            None,
            None,
            Some("old-input"),
            false,
            "healthy",
        )];
        derive_semantic_states(&mut operations);
        assert_eq!(operations[0]["automation"]["semantic_state"], "stale");
        assert_eq!(operations[0]["automation"]["blocker"]["current"], false);
        assert_eq!(operations[0]["automation"]["blocker"]["id"], "blk-a");
    }

    #[test]
    fn current_blocked_processed_is_blocked_without_blocker_file() {
        let mut operation = semantic_operation(
            "managed-proof",
            None,
            "in",
            "gen",
            None,
            None,
            None,
            false,
            "healthy",
        );
        operation["automation"]["processed"] = json!({
            "version": 1,
            "input_fingerprint": "in",
            "outcome": "blocked",
            "legacy": false
        });
        let mut operations = vec![operation];
        derive_semantic_states(&mut operations);
        assert_eq!(operations[0]["automation"]["semantic_state"], "blocked");
        assert_eq!(operations[0]["health"], "healthy");
    }
}
