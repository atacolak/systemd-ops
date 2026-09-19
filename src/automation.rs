//! Agent-backed automation metadata, relationships, revisions, and lifecycle.
//!
//! Systemd unit files remain the definition truth. `automation.toml` stores only
//! agent-layer metadata which systemd cannot represent.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

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
    "native-provisioning",
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

pub fn desired_inputs_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/desired-inputs.json")
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

pub fn effective_input_fingerprint(
    world_fingerprint: &str,
    brain_revision: &str,
    desired_inputs: Option<&[u8]>,
) -> String {
    let mut material = format!("world={world_fingerprint}\nbrain={brain_revision}\n");
    if let Some(bytes) = desired_inputs {
        material.push_str(&format!("desired={}\n", sha256_hex(bytes)));
    }
    sha256_hex(material.as_bytes())
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
    let desired_bytes = read_optional_bytes(&desired_inputs_path(&manifest.root, &stem))?;
    let observation = Observation {
        version: 1,
        world_fingerprint: validate_observer_value("world_fingerprint", world_fingerprint)?,
        brain_revision: validate_observer_value("brain_revision", brain_revision)?,
        input_fingerprint: effective_input_fingerprint(
            world_fingerprint,
            brain_revision,
            desired_bytes.as_deref(),
        ),
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
    if processed_is_current_blocked(operation) {
        return true;
    }
    let automation = operation.get("automation").unwrap_or(&Value::Null);
    let blocker = automation.get("blocker").unwrap_or(&Value::Null);
    if blocker.is_null() {
        return false;
    }
    let observation_input = automation
        .get("observation")
        .and_then(|value| value.get("input_fingerprint"))
        .and_then(Value::as_str);
    match blocker.get("input_fingerprint").and_then(Value::as_str) {
        Some(input) => observation_input == Some(input),
        None => true,
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
        } else if blocker_is_current(operation) {
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredInputsFile {
    version: u32,
    inputs: Vec<DesiredInput>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredInput {
    originating_work: String,
    artifact_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<Vec<DeltaFile>>,
    artifact_identity: String,
    admitted_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeltaFile {
    path: String,
    sha256: String,
}

struct AdmitSpec {
    originating_work: String,
    capability: String,
    artifact_kind: String,
    artifact_identity: String,
    artifact_revision: Option<String>,
    base: Option<String>,
    delta: Option<Vec<DeltaFile>>,
    supersedes: Option<String>,
}

fn parse_supersedes(spec: &Value, artifact_identity: &str) -> Result<Option<String>, BackendError> {
    match spec.get("supersedes") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let valid = matches!(value.len(), 40 | 64)
                && value
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
            if !valid {
                return Err(BackendError(
                    "supersedes must be 40 or 64 lowercase hex".into(),
                ));
            }
            if value == artifact_identity {
                return Err(BackendError(
                    "supersedes must not equal the new artifact_identity".into(),
                ));
            }
            Ok(Some(value.clone()))
        }
        Some(_) => Err(BackendError("supersedes must be a string".into())),
    }
}

fn lowercase_hex(label: &str, value: &str, len: usize) -> Result<String, BackendError> {
    let valid = value.len() == len
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(value.to_string())
    } else {
        Err(BackendError(format!("{label} must be {len} lowercase hex")))
    }
}

fn validate_originating_work(value: &str) -> Result<String, BackendError> {
    if value == "overseer" {
        return Err(BackendError(
            "overseer is refused as originating_work".into(),
        ));
    }
    let valid = (1..=80).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-'));
    if valid {
        Ok(value.to_string())
    } else {
        Err(BackendError(
            "originating_work must be 1..80 characters matching [A-Za-z0-9_.:-]".into(),
        ))
    }
}

fn validate_capability_id(value: &str) -> Result<String, BackendError> {
    if value == "overseer" {
        return Err(BackendError("overseer is refused as capability".into()));
    }
    let valid = (1..=50).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(value.to_string())
    } else {
        Err(BackendError(
            "capability must be 1..50 characters matching [A-Za-z0-9_-]".into(),
        ))
    }
}

fn validate_delta_path(path: &str) -> Result<(), BackendError> {
    if path.is_empty() || path.len() > 200 {
        return Err(BackendError("delta path must be 1..200 characters".into()));
    }
    if !path.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err(BackendError(
            "delta path must be printable ASCII without NUL".into(),
        ));
    }
    if path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(BackendError(
            "delta path must be relative without a .. component".into(),
        ));
    }
    Ok(())
}

fn worktree_delta_identity(base: &str, delta: &[DeltaFile]) -> String {
    let mut files = delta.to_vec();
    files.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let mut material = format!("worktree-delta\nbase={base}\n");
    for file in files {
        material.push_str(&file.path);
        material.push('=');
        material.push_str(&file.sha256);
        material.push('\n');
    }
    sha256_hex(material.as_bytes())
}

fn parse_admit_spec(spec: &Value) -> Result<AdmitSpec, BackendError> {
    if !spec.is_object() {
        return Err(BackendError("admit spec must be an object".into()));
    }
    if spec.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(BackendError("admit spec version must be 1".into()));
    }
    let originating_work = spec
        .get("originating_work")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: originating_work".into()))?;
    let originating_work = validate_originating_work(originating_work)?;
    let capability = spec
        .get("capability")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: capability".into()))?;
    let capability = validate_capability_id(capability)?;
    let kind = spec
        .get("artifact_kind")
        .and_then(Value::as_str)
        .unwrap_or("commit");
    if !matches!(kind, "commit" | "worktree-delta") {
        return Err(BackendError(
            "artifact_kind must be commit or worktree-delta".into(),
        ));
    }
    let has_revision = spec
        .get("artifact_revision")
        .is_some_and(|value| !value.is_null());
    let has_base = spec.get("base").is_some_and(|value| !value.is_null());
    let has_delta = spec.get("delta").is_some_and(|value| !value.is_null());
    if kind == "worktree-delta" && has_revision || kind == "commit" && (has_base || has_delta) {
        return Err(BackendError(
            "admit spec refused: mixed artifact kinds".into(),
        ));
    }
    if kind == "commit" {
        let revision = spec
            .get("artifact_revision")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError("missing required argument: artifact_revision".into()))?;
        let artifact_revision = lowercase_hex("artifact_revision", revision, 40)?;
        let artifact_identity = artifact_revision.clone();
        let supersedes = parse_supersedes(spec, &artifact_identity)?;
        return Ok(AdmitSpec {
            originating_work,
            capability,
            artifact_kind: "commit".into(),
            artifact_identity,
            artifact_revision: Some(artifact_revision),
            base: None,
            delta: None,
            supersedes,
        });
    }
    let base = spec
        .get("base")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: base".into()))?;
    let base = lowercase_hex("base", base, 40)?;
    let raw_delta = spec.get("delta").and_then(Value::as_array).ok_or_else(|| {
        BackendError("delta must be an array of 1..32 path/sha256 objects".into())
    })?;
    if raw_delta.is_empty() || raw_delta.len() > 32 {
        return Err(BackendError("delta must have 1..32 entries".into()));
    }
    let mut delta = Vec::with_capacity(raw_delta.len());
    let mut seen = BTreeSet::new();
    for item in raw_delta {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError("delta entry path is required".into()))?;
        validate_delta_path(path)?;
        if !seen.insert(path.to_string()) {
            return Err(BackendError("delta paths must be unique".into()));
        }
        let sha = item
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError("delta entry sha256 is required".into()))?;
        let sha256 = lowercase_hex("delta sha256", sha, 64)?;
        delta.push(DeltaFile {
            path: path.to_string(),
            sha256,
        });
    }
    let artifact_identity = worktree_delta_identity(&base, &delta);
    let supersedes = parse_supersedes(spec, &artifact_identity)?;
    Ok(AdmitSpec {
        originating_work,
        capability,
        artifact_kind: "worktree-delta".into(),
        artifact_identity,
        artifact_revision: None,
        base: Some(base),
        delta: Some(delta),
        supersedes,
    })
}

fn new_capability_create_args(capability: &str, scope_root: &Path) -> Value {
    let stem = format!("managed-omp-cap-{capability}");
    json!({
        "spec": {
            "unit": stem,
            "kind": "oneshot",
            "title": format!("{capability} capability"),
            "tags": [
                "omp",
                "capability-maintainer",
                format!("capability:{capability}"),
                format!("branch:cap/{capability}"),
            ],
            "exec": { "path": "/bin/true", "argv": [] },
            "cwd": scope_root.to_string_lossy(),
            "schedule": {
                "type": "interval",
                "on_boot_sec": "5min",
                "on_unit_inactive_sec": "3min",
                "persistent": false,
                "accuracy_sec": "30s",
            },
            "enabled": true,
            "start_now": true,
            "agent": "capability-maintainer",
            "parent": "managed-omp-runtime-integrator",
        }
    })
}

fn refuse_capability_tag_collision(stem: &str, capability: &str) -> Result<(), BackendError> {
    let cap_tag = format!("capability:{capability}");
    let branch_tag = format!("branch:cap/{capability}");
    let dir = systemd::unit_file_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(BackendError(format!(
                "cannot read unit directory {}: {error}",
                dir.display()
            )))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            BackendError(format!(
                "cannot read unit directory {}: {error}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) if name.ends_with(".service") => name,
            _ => continue,
        };
        let other = name.trim_end_matches(".service");
        if other == stem {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let tags = unit_file_capability_tags(&text);
        if tags.iter().any(|tag| tag == &cap_tag || tag == &branch_tag) {
            return Err(BackendError(format!(
                "admit refused: capability '{capability}' collides on tag with '{other}'"
            )));
        }
    }
    Ok(())
}

fn unit_file_capability_tags(text: &str) -> Vec<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        let rest = trimmed
            .strip_prefix("# systemd-ops-tags:")
            .or_else(|| trimmed.strip_prefix("# managed-tags:"));
        if let Some(rest) = rest {
            return rest
                .split(',')
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect();
        }
    }
    Vec::new()
}

pub fn plan_admit(
    args: &Value,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let spec = args
        .get("spec")
        .ok_or_else(|| BackendError("missing required argument: spec".into()))?;
    let admit = parse_admit_spec(spec)?;
    let stem = format!("managed-omp-cap-{}", admit.capability);
    let manifest = resolved_manifest(explicit_root, cwd)?;
    crate::operator::require_owned(&manifest, &stem)?;
    if load_metadata(&manifest.root, &stem)?.is_none() {
        refuse_capability_tag_collision(&stem, &admit.capability)?;
    }
    let desired_path = desired_inputs_path(&manifest.root, &stem);
    let desired_snapshot = snapshot(&desired_path);
    if let Some(supersedes) = admit.supersedes.as_deref() {
        let unit_path = systemd::unit_file_dir().join(format!("{stem}.service"));
        if !unit_path.is_file() || !desired_path.is_file() {
            return Err(BackendError(
                "supersedes is existing-capability only".into(),
            ));
        }
        let bytes = read_optional_bytes(&desired_path)?
            .ok_or_else(|| BackendError("supersedes is existing-capability only".into()))?;
        let desired = serde_json::from_slice::<DesiredInputsFile>(&bytes)
            .map_err(|error| BackendError(format!("malformed desired-inputs.json: {error}")))?;
        if !desired
            .inputs
            .iter()
            .any(|input| input.artifact_identity == supersedes)
        {
            return Err(BackendError(
                "supersedes identity is absent from desired-inputs".into(),
            ));
        }
        if desired.inputs.iter().any(|input| {
            input.originating_work == admit.originating_work
                && input.artifact_identity == admit.artifact_identity
        }) {
            return Err(BackendError(
                "supersedes refused: replacement already exists as another entry".into(),
            ));
        }
    }
    let cfg = crate::config::current_or_load()?;
    let payload = json!({
        "action": "admit",
        "scope_root": manifest.root.to_string_lossy(),
        "capability": admit.capability,
        "originating_work": admit.originating_work,
        "artifact_kind": admit.artifact_kind,
        "artifact_identity": admit.artifact_identity,
        "artifact_revision": admit.artifact_revision,
        "base": admit.base,
        "delta": admit.delta,
        "supersedes": admit.supersedes,
        "desired_inputs_snapshot": snapshot_json(&desired_snapshot),
    });
    let (plan_token, sealed) = token::mint(
        &cfg,
        PlanClass::Automation,
        &stem,
        cwd.map(str::to_string),
        payload,
    )?;
    Ok(json!({
        "plan_token": plan_token,
        "class": "automation",
        "unit": stem,
        "action": "admit",
        "issued_at": crate::config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(sealed.expires_at),
        "capability": admit.capability,
        "originating_work": admit.originating_work,
        "artifact_kind": admit.artifact_kind,
        "artifact_identity": admit.artifact_identity,
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

fn apply_admit(plan: &token::SealedPlan, cwd: Option<&str>) -> Result<Value, BackendError> {
    let snapshot = snapshot_from_json(
        plan.payload
            .get("desired_inputs_snapshot")
            .ok_or_else(|| BackendError("invalid automation plan token".into()))?,
    )?;
    require_snapshot_fresh(&snapshot)?;
    let scope_root = plan
        .payload
        .get("scope_root")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    if plan
        .payload
        .get("supersedes")
        .and_then(Value::as_str)
        .is_some()
    {
        let unit_path = systemd::unit_file_dir().join(format!("{}.service", plan.unit));
        let desired_path = Path::new(&snapshot.path);
        if !unit_path.is_file() || !desired_path.is_file() {
            return Err(BackendError(
                "supersedes is existing-capability only".into(),
            ));
        }
    }
    if load_metadata(Path::new(scope_root), &plan.unit)?.is_none() {
        let capability = plan
            .payload
            .get("capability")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
        refuse_capability_tag_collision(&plan.unit, capability)?;
        let created = plan_instance(
            "create",
            &new_capability_create_args(capability, Path::new(scope_root)),
            Some(scope_root),
            cwd,
        )?;
        let token = created
            .get("plan_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                BackendError("automation create plan did not return a plan token".into())
            })?;
        crate::write::apply_with_context(token, cwd)?;
    }
    let originating_work = plan
        .payload
        .get("originating_work")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    let artifact_identity = plan
        .payload
        .get("artifact_identity")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    let path = Path::new(&snapshot.path);
    let mut desired = match read_optional_bytes(path)? {
        Some(bytes) => serde_json::from_slice::<DesiredInputsFile>(&bytes)
            .map_err(|error| BackendError(format!("malformed desired-inputs.json: {error}")))?,
        None => DesiredInputsFile {
            version: 1,
            inputs: Vec::new(),
        },
    };
    if desired.version != 1 {
        return Err(BackendError("desired-inputs.json version must be 1".into()));
    }
    let supersedes = match plan.payload.get("supersedes") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err(BackendError("invalid automation plan token".into())),
    };
    let duplicate = desired.inputs.iter().any(|input| {
        input.originating_work == originating_work && input.artifact_identity == artifact_identity
    });
    let replace_at = if let Some(supersedes) = supersedes {
        Some(
            desired
                .inputs
                .iter()
                .position(|input| input.artifact_identity == supersedes)
                .ok_or_else(|| {
                    BackendError("supersedes identity is absent from desired-inputs".into())
                })?,
        )
    } else {
        None
    };
    if replace_at.is_some() || !duplicate {
        let artifact_kind = plan
            .payload
            .get("artifact_kind")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError("invalid automation plan token".into()))?
            .to_string();
        let artifact_revision = plan
            .payload
            .get("artifact_revision")
            .and_then(Value::as_str)
            .map(str::to_string);
        let base = plan
            .payload
            .get("base")
            .and_then(Value::as_str)
            .map(str::to_string);
        let delta = match plan.payload.get("delta") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                serde_json::from_value(value.clone())
                    .map_err(|_| BackendError("invalid automation plan token".into()))?,
            ),
        };
        let entry = DesiredInput {
            originating_work: originating_work.to_string(),
            artifact_kind,
            artifact_revision,
            base,
            delta,
            artifact_identity: artifact_identity.to_string(),
            admitted_at: crate::config::unix_to_rfc3339(crate::config::now_unix()),
        };
        if let Some(index) = replace_at {
            if desired.inputs.iter().enumerate().any(|(other, input)| {
                other != index
                    && input.originating_work == originating_work
                    && input.artifact_identity == artifact_identity
            }) {
                return Err(BackendError(
                    "supersedes refused: replacement already exists as another entry".into(),
                ));
            }
            desired.inputs[index] = entry;
        } else {
            desired.inputs.push(entry);
        }
        atomic_write(path, &serialize_pretty(&desired)?)?;
    }
    Ok(json!({
        "class": "automation",
        "unit": plan.unit,
        "action": "admit",
        "applied": true,
        "noop": replace_at.is_none() && duplicate,
        "desired_inputs": snapshot.path,
    }))
}

const RUNTIME_INTEGRATOR: &str = "managed-omp-runtime-integrator";

struct CanaryAckSpec {
    generation: String,
    actor: String,
    result: String,
    observed_generation: String,
    continuity_actor: String,
    session_id: String,
}

enum CanaryAckIntent {
    Fail { reason: String },
    Recovered,
    Pass,
}

fn deployment_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/deployment.json")
}

fn dogfood_lib(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("dogfood/lib")
        .join(name)
}

fn runtime_generation_label(sha: &str) -> String {
    let prefix = sha.get(..8).unwrap_or(sha);
    format!("omp@{prefix}")
}

fn parse_canary_ack_spec(spec: &Value) -> Result<CanaryAckSpec, BackendError> {
    if !spec.is_object() {
        return Err(BackendError("canary ack spec must be an object".into()));
    }
    if spec.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(BackendError("canary ack spec version must be 1".into()));
    }
    let generation = spec
        .get("generation")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: generation".into()))?;
    let generation = lowercase_hex("generation", generation, 40)?;
    let observed = spec
        .get("observed_generation")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: observed_generation".into()))?;
    let observed_generation = lowercase_hex("observed_generation", observed, 40)?;
    let actor = spec
        .get("actor")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: actor".into()))?
        .to_string();
    let result = spec
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: result".into()))?;
    if !matches!(result, "pass" | "fail" | "recovered") {
        return Err(BackendError(
            "result must be pass, fail, or recovered".into(),
        ));
    }
    let (continuity_actor, session_id) = match spec.get("continuity") {
        None | Some(Value::Null) => (String::new(), String::new()),
        Some(Value::Object(map)) => {
            let continuity_actor = map
                .get("actor")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let session_id = map
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if session_id.len() > 80 {
                return Err(BackendError(
                    "continuity.session_id must be at most 80 characters".into(),
                ));
            }
            (continuity_actor, session_id)
        }
        Some(_) => return Err(BackendError("continuity must be an object".into())),
    };
    Ok(CanaryAckSpec {
        generation,
        actor,
        result: result.to_string(),
        observed_generation,
        continuity_actor,
        session_id,
    })
}

fn canary_ack_from_plan(plan: &token::SealedPlan) -> Result<CanaryAckSpec, BackendError> {
    let generation = plan
        .payload
        .get("generation")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?
        .to_string();
    let actor = plan
        .payload
        .get("actor")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?
        .to_string();
    let result = plan
        .payload
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?
        .to_string();
    let observed_generation = plan
        .payload
        .get("observed_generation")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?
        .to_string();
    let continuity = plan.payload.get("continuity");
    let continuity_actor = continuity
        .and_then(|value| value.get("actor"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let session_id = continuity
        .and_then(|value| value.get("session_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(CanaryAckSpec {
        generation,
        actor,
        result,
        observed_generation,
        continuity_actor,
        session_id,
    })
}

fn load_json_object(path: &Path, label: &str) -> Result<Value, BackendError> {
    let bytes =
        read_optional_bytes(path)?.ok_or_else(|| BackendError(format!("{label} is missing")))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| BackendError(format!("malformed {label}: {error}")))?;
    if !value.is_object() {
        return Err(BackendError(format!("{label} must be an object")));
    }
    Ok(value)
}

fn candidate_output_revision(deployment: &Value) -> Option<&str> {
    deployment
        .get("candidate")
        .and_then(|value| value.get("output_revision"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn recovery_target_sha(deployment: &Value) -> Option<&str> {
    deployment
        .get("recovery")
        .and_then(|value| value.get("target"))
        .and_then(|value| value.get("sha"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn midi_alive_on(deployment: &Value, sha: &str) -> bool {
    if let Some(at) = deployment
        .get("recovery")
        .and_then(|value| value.get("midi_alive_at"))
        .and_then(Value::as_str)
    {
        if !at.is_empty() {
            return true;
        }
    }
    let Some(rows) = deployment.get("occupancy").and_then(Value::as_array) else {
        return false;
    };
    rows.iter().any(|row| {
        row.get("name").and_then(Value::as_str) == Some("midi")
            && row.get("state").and_then(Value::as_str) == Some("live")
            && row
                .get("runtime_generation")
                .and_then(|value| value.get("sha"))
                .and_then(Value::as_str)
                == Some(sha)
    })
}

fn recovery_hold_active(deployment: &Value) -> bool {
    match recovery_target_sha(deployment) {
        Some(sha) => !midi_alive_on(deployment, sha),
        None => false,
    }
}

fn fail_closed_reason(spec: &CanaryAckSpec, deployment: &Value) -> Option<String> {
    if candidate_output_revision(deployment).is_none() {
        return Some("there is no candidate".into());
    }
    if spec.actor != "midi" {
        return Some("actor is not midi".into());
    }
    if spec.result == "fail" {
        return Some("result=fail".into());
    }
    if spec.generation != candidate_output_revision(deployment).unwrap_or("") {
        return Some("generation is not the current candidate".into());
    }
    if spec.observed_generation != spec.generation {
        return Some("observed_generation mismatch".into());
    }
    if spec.continuity_actor != "midi" {
        return Some("continuity.actor is not midi".into());
    }
    if spec.session_id.is_empty() {
        return Some("empty continuity".into());
    }
    None
}

fn validate_recovered(spec: &CanaryAckSpec, deployment: &Value) -> Result<(), BackendError> {
    let Some(sha) = recovery_target_sha(deployment) else {
        return Err(BackendError(
            "result=recovered refused: no recovery.target".into(),
        ));
    };
    if spec.actor != "midi" {
        return Err(BackendError(
            "result=recovered refused: actor is not midi".into(),
        ));
    }
    if spec.continuity_actor != "midi" || spec.session_id.is_empty() {
        return Err(BackendError(
            "result=recovered refused: continuity is not midi".into(),
        ));
    }
    if spec.generation != sha || spec.observed_generation != sha {
        return Err(BackendError(
            "result=recovered refused: generation must equal recovery.target.sha".into(),
        ));
    }
    Ok(())
}

fn classify_canary_ack(
    spec: &CanaryAckSpec,
    deployment: &Value,
) -> Result<CanaryAckIntent, BackendError> {
    if spec.result == "recovered" {
        validate_recovered(spec, deployment)?;
        return Ok(CanaryAckIntent::Recovered);
    }
    if let Some(reason) = fail_closed_reason(spec, deployment) {
        return Ok(CanaryAckIntent::Fail { reason });
    }
    if recovery_hold_active(deployment) {
        return Err(BackendError(
            "canary pass refused until midi recovered on the previous promoted generation".into(),
        ));
    }
    Ok(CanaryAckIntent::Pass)
}

fn previous_promoted_sha(
    deployment: &Value,
    published_path: &Path,
    gens_root: &str,
) -> Result<String, BackendError> {
    if let Ok(published) = load_json_object(published_path, "published-generation.json") {
        if let Some(sha) = published
            .get("output_revision")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            return Ok(sha.to_string());
        }
    }
    if let Some(sha) = deployment
        .get("promoted")
        .and_then(|value| value.get("output_revision"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return Ok(sha.to_string());
    }
    let current = Path::new(gens_root).join("current");
    if let Ok(target) = fs::canonicalize(&current) {
        if let Some(name) = target.file_name().and_then(|name| name.to_str()) {
            return Ok(name.to_string());
        }
    }
    Err(BackendError("no previous promoted generation".into()))
}

fn already_promoted(deployment: &Value, published_path: &Path, gens_root: &str) -> bool {
    let Some(cand) = candidate_output_revision(deployment) else {
        return false;
    };
    let current_ok = match (
        fs::canonicalize(Path::new(gens_root).join("current")),
        fs::canonicalize(Path::new(gens_root).join(cand)),
    ) {
        (Ok(current), Ok(target)) => current == target,
        _ => false,
    };
    let published_ok = load_json_object(published_path, "published-generation.json")
        .ok()
        .and_then(|value| {
            value
                .get("output_revision")
                .and_then(Value::as_str)
                .map(|sha| sha == cand)
        })
        .unwrap_or(false);
    current_ok && published_ok
}

fn notify_canary_recovery(
    restored_label: &str,
    restored_sha: &str,
    candidate_label: &str,
    reason: &str,
    observed_generation: &str,
) {
    let message = format!(
        "canary failed; failed candidate {candidate_label}; restore {restored_label} ({restored_sha}); {reason}; observed_generation={observed_generation}"
    );
    if let Some(lib) = std::env::var("SYSTEM_NOTIFY_LIB")
        .ok()
        .filter(|value| !value.is_empty())
    {
        let _ = Command::new(lib)
            .args(["omp-runtime", "inform", &message])
            .status();
        return;
    }
    let lib = dogfood_lib("system-notify");
    let _ = Command::new("bash")
        .args([
            "-c",
            "source \"$1\"; system_notify omp-runtime inform \"$2\"",
            "system-notify",
            lib.to_str().unwrap_or(""),
            &message,
        ])
        .status();
}

fn promote_runtime_candidate(deployment: &Value, state_dir: &Path) -> Result<(), BackendError> {
    let compose = deployment
        .get("candidate")
        .and_then(|value| value.get("generation"))
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("candidate.generation is missing".into()))?;
    let output_revision = candidate_output_revision(deployment)
        .ok_or_else(|| BackendError("candidate.output_revision is missing".into()))?;
    let gens_root = deployment
        .get("generations_root")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("deployment.generations_root is missing".into()))?;
    let promote = std::env::var("RUNTIME_PROMOTE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| dogfood_lib("runtime-promote").display().to_string());
    let output = Command::new("bash")
        .args([
            "-c",
            "set -euo pipefail; source \"$RUNTIME_PROMOTE\"; promote_runtime_generation \"$1\" \"$2\"",
            "promote",
            compose,
            output_revision,
        ])
        .env("RUNTIME_PROMOTE", &promote)
        .env("GENERATIONS_ROOT", gens_root)
        .env("STATE_DIR", state_dir)
        .output()
        .map_err(|error| BackendError(format!("cannot invoke runtime-promote: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(BackendError(format!(
        "runtime-promote failed{}{}",
        if stderr.is_empty() { "" } else { ": " },
        stderr
    )))
}

pub fn plan_deploy_canary_ack(
    args: &Value,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let spec = args
        .get("spec")
        .ok_or_else(|| BackendError("missing required argument: spec".into()))?;
    let ack = parse_canary_ack_spec(spec)?;
    let stem = RUNTIME_INTEGRATOR;
    let manifest = resolved_manifest(explicit_root, cwd)?;
    crate::operator::require_owned(&manifest, stem)?;
    let deploy_path = deployment_path(&manifest.root, stem);
    let deploy_snapshot = snapshot(&deploy_path);
    let deployment = load_json_object(&deploy_path, "deployment.json")?;
    classify_canary_ack(&ack, &deployment)?;
    let cfg = crate::config::current_or_load()?;
    let payload = json!({
        "action": "deploy-canary-ack",
        "scope_root": manifest.root.to_string_lossy(),
        "generation": ack.generation,
        "actor": ack.actor,
        "result": ack.result,
        "observed_generation": ack.observed_generation,
        "continuity": {
            "actor": ack.continuity_actor,
            "session_id": ack.session_id,
        },
        "deployment_snapshot": snapshot_json(&deploy_snapshot),
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
        "action": "deploy-canary-ack",
        "issued_at": crate::config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(sealed.expires_at),
        "generation": ack.generation,
        "actor": ack.actor,
        "result": ack.result,
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

fn apply_deploy_canary_ack(
    plan: &token::SealedPlan,
    _cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let snapshot = snapshot_from_json(
        plan.payload
            .get("deployment_snapshot")
            .ok_or_else(|| BackendError("invalid automation plan token".into()))?,
    )?;
    require_snapshot_fresh(&snapshot)?;
    let ack = canary_ack_from_plan(plan)?;
    let path = Path::new(&snapshot.path);
    let mut deployment = load_json_object(path, "deployment.json")?;
    let state_dir = path
        .parent()
        .ok_or_else(|| BackendError("deployment.json has no parent".into()))?;
    let published_path = state_dir.join("published-generation.json");
    let gens_root = deployment
        .get("generations_root")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match classify_canary_ack(&ack, &deployment)? {
        CanaryAckIntent::Fail { reason } => {
            let prev = previous_promoted_sha(&deployment, &published_path, &gens_root)?;
            let label = runtime_generation_label(&prev);
            let now = crate::config::unix_to_rfc3339(crate::config::now_unix());
            deployment["canary"] = json!({
                "actor": ack.actor,
                "result": "fail",
                "observed_generation": ack.observed_generation,
                "continuity": {
                    "actor": ack.continuity_actor,
                    "session_id": ack.session_id,
                },
                "recorded_at": now,
                "reason": reason,
            });
            if let Some(candidate) = deployment.get_mut("candidate") {
                if candidate.is_object() {
                    candidate["phase"] = json!("candidate");
                }
            }
            deployment["recovery"] = json!({
                "target": {"sha": prev, "label": label},
                "midi_alive_at": "",
                "reason": reason,
            });
            atomic_write(path, &serialize_pretty(&deployment)?)?;
            let candidate_label = candidate_output_revision(&deployment)
                .map(runtime_generation_label)
                .unwrap_or_default();
            notify_canary_recovery(
                &label,
                &prev,
                &candidate_label,
                &reason,
                &ack.observed_generation,
            );
            Ok(json!({
                "class": "automation",
                "unit": plan.unit,
                "action": "deploy-canary-ack",
                "applied": true,
                "result": "fail",
                "recovery_target": {"sha": prev, "label": label},
            }))
        }
        CanaryAckIntent::Recovered => {
            let now = crate::config::unix_to_rfc3339(crate::config::now_unix());
            match deployment.get_mut("recovery") {
                Some(Value::Object(map)) => {
                    map.insert("midi_alive_at".into(), json!(now));
                }
                _ => {
                    return Err(BackendError(
                        "result=recovered refused: no recovery.target".into(),
                    ))
                }
            }
            atomic_write(path, &serialize_pretty(&deployment)?)?;
            Ok(json!({
                "class": "automation",
                "unit": plan.unit,
                "action": "deploy-canary-ack",
                "applied": true,
                "result": "recovered",
                "midi_alive_at": now,
            }))
        }
        CanaryAckIntent::Pass => {
            if already_promoted(&deployment, &published_path, &gens_root) {
                return Ok(json!({
                    "class": "automation",
                    "unit": plan.unit,
                    "action": "deploy-canary-ack",
                    "applied": true,
                    "result": "pass",
                    "noop": true,
                }));
            }
            promote_runtime_candidate(&deployment, state_dir)?;
            let now = crate::config::unix_to_rfc3339(crate::config::now_unix());
            let cand_sha = candidate_output_revision(&deployment)
                .ok_or_else(|| BackendError("candidate.output_revision is missing".into()))?
                .to_string();
            let cand_gen = deployment
                .get("candidate")
                .and_then(|value| value.get("generation"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            deployment["canary"] = json!({
                "actor": ack.actor,
                "result": "pass",
                "observed_generation": ack.observed_generation,
                "continuity": {
                    "actor": ack.continuity_actor,
                    "session_id": ack.session_id,
                },
                "recorded_at": now,
            });
            if let Some(candidate) = deployment.get_mut("candidate") {
                if candidate.is_object() {
                    candidate["phase"] = json!("rolling_out");
                }
            }
            deployment["promoted"] = json!({
                "generation": cand_gen,
                "output_revision": cand_sha,
                "promoted_at": now,
            });
            atomic_write(path, &serialize_pretty(&deployment)?)?;
            Ok(json!({
                "class": "automation",
                "unit": plan.unit,
                "action": "deploy-canary-ack",
                "applied": true,
                "result": "pass",
                "noop": false,
            }))
        }
    }
}

struct OccupancyRow {
    name: String,
    sha: String,
    label: String,
    upgrade_target: Option<(String, String)>,
    state: String,
    reason: String,
    acked_at: String,
}

fn published_generation_path(root: &Path, stem: &str) -> PathBuf {
    operation_home(root, stem).join("state/published-generation.json")
}

fn occupancy_row_value(row: &OccupancyRow) -> Value {
    json!({
        "name": row.name,
        "runtime_generation": {"sha": row.sha, "label": row.label},
        "upgrade_target": match &row.upgrade_target {
            None => Value::Null,
            Some((sha, label)) => json!({"sha": sha, "label": label}),
        },
        "state": row.state,
        "reason": row.reason,
        "acked_at": row.acked_at,
    })
}

fn parse_generation_record(value: &Value, field: &str) -> Result<(String, String), BackendError> {
    let object = value
        .as_object()
        .ok_or_else(|| BackendError(format!("{field} must be an object")))?;
    let sha = object
        .get("sha")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError(format!("{field}.sha is required")))?;
    let sha = lowercase_hex(&format!("{field}.sha"), sha, 40)?;
    let expected = runtime_generation_label(&sha);
    let label = match object.get("label") {
        None | Some(Value::Null) => expected.clone(),
        Some(Value::String(label)) if label == &expected => expected,
        Some(Value::String(_)) => {
            return Err(BackendError(format!("{field}.label must be {expected}")));
        }
        Some(_) => return Err(BackendError(format!("{field}.label must be a string"))),
    };
    Ok((sha, label))
}

fn validate_occupancy_name(name: &str) -> Result<String, BackendError> {
    if name.is_empty()
        || name.len() > 64
        || name
            .chars()
            .any(|ch| ch.is_whitespace() || ch == '/' || ch == ':')
    {
        return Err(BackendError("occupancy actor name is invalid".into()));
    }
    Ok(name.to_string())
}

fn parse_occupancy_row(value: &Value) -> Result<OccupancyRow, BackendError> {
    if !value.is_object() {
        return Err(BackendError("occupancy actor must be an object".into()));
    }
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("occupancy actor name is required".into()))?;
    let name = validate_occupancy_name(name)?;
    let runtime = value
        .get("runtime_generation")
        .ok_or_else(|| BackendError("occupancy runtime_generation is required".into()))?;
    let (sha, label) = parse_generation_record(runtime, "runtime_generation")?;
    let upgrade_target = match value.get("upgrade_target") {
        None | Some(Value::Null) => None,
        Some(target) => Some(parse_generation_record(target, "upgrade_target")?),
    };
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("occupancy state is required".into()))?;
    if !matches!(state, "live" | "stopped" | "retired") {
        return Err(BackendError(
            "occupancy state must be live, stopped, or retired".into(),
        ));
    }
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if reason.len() > MAX_REASON {
        return Err(BackendError("occupancy reason is too long".into()));
    }
    let acked_at = value
        .get("acked_at")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("occupancy acked_at is required".into()))?;
    if acked_at.is_empty() {
        return Err(BackendError("occupancy acked_at is required".into()));
    }
    Ok(OccupancyRow {
        name,
        sha,
        label,
        upgrade_target,
        state: state.to_string(),
        reason,
        acked_at: acked_at.to_string(),
    })
}

fn parse_occupancy_spec(spec: &Value) -> Result<(String, Vec<OccupancyRow>), BackendError> {
    if !spec.is_object() {
        return Err(BackendError("occupancy spec must be an object".into()));
    }
    if spec.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(BackendError("occupancy spec version must be 1".into()));
    }
    let generation = spec
        .get("generation")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: generation".into()))?;
    let generation = lowercase_hex("generation", generation, 40)?;
    let actors = spec
        .get("actors")
        .and_then(Value::as_array)
        .ok_or_else(|| BackendError("occupancy actors must be an array".into()))?;
    let mut rows = Vec::with_capacity(actors.len());
    let mut seen = BTreeSet::new();
    for actor in actors {
        let row = parse_occupancy_row(actor)?;
        if !seen.insert(row.name.clone()) {
            return Err(BackendError(format!(
                "duplicate occupancy actor {}",
                row.name
            )));
        }
        rows.push(row);
    }
    Ok((generation, rows))
}

fn occupancy_from_plan(plan: &token::SealedPlan) -> Result<Vec<OccupancyRow>, BackendError> {
    let actors = plan
        .payload
        .get("actors")
        .and_then(Value::as_array)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    actors.iter().map(parse_occupancy_row).collect()
}

fn occupancy_row_settled(row: &Value, promoted: &str) -> bool {
    match row.get("state").and_then(Value::as_str) {
        Some("stopped" | "retired") => true,
        _ => {
            row.get("runtime_generation")
                .and_then(|value| value.get("sha"))
                .and_then(Value::as_str)
                == Some(promoted)
        }
    }
}

fn maybe_set_converged(deployment: &mut Value) {
    let phase = deployment
        .get("candidate")
        .and_then(|value| value.get("phase"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if phase != "rolling_out" && phase != "converged" {
        return;
    }
    let Some(promoted) = deployment
        .get("promoted")
        .and_then(|value| value.get("output_revision"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
    else {
        return;
    };
    let occupancy = deployment
        .get("occupancy")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let settled = occupancy
        .iter()
        .all(|row| occupancy_row_settled(row, &promoted));
    if let Some(candidate) = deployment.get_mut("candidate") {
        if candidate.is_object() {
            candidate["phase"] = json!(if settled { "converged" } else { "rolling_out" });
        }
    }
}

fn generations_root_of(deployment: &Value) -> Option<PathBuf> {
    deployment
        .get("generations_root")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("GENERATIONS_ROOT")
                .ok()
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
}

fn generation_directories(gens_root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(gens_root) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "current" || lowercase_hex("generation", name, 40).is_err() {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            dirs.push((name.to_string(), path));
        }
    }
    dirs.sort_by(|left, right| left.0.cmp(&right.0));
    dirs
}

fn current_points_at(gens_root: &Path, sha: &str) -> bool {
    let current = fs::canonicalize(gens_root.join("current")).ok();
    let target = fs::canonicalize(gens_root.join(sha)).ok();
    match (current, target) {
        (Some(current), Some(target)) => current == target,
        _ => false,
    }
}

fn process_executable_resolves_into(gen_dir: &Path) -> bool {
    let Ok(root) = fs::canonicalize(gen_dir) else {
        return false;
    };
    let Ok(proc) = fs::read_dir("/proc") else {
        return false;
    };
    for entry in proc.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let exe = entry.path().join("exe");
        let target = fs::canonicalize(&exe).or_else(|_| fs::read_link(&exe));
        if let Ok(target) = target {
            if target.starts_with(&root) {
                return true;
            }
        }
    }
    false
}

fn live_occupancy_names(deployment: &Value, sha: &str) -> Vec<String> {
    let Some(rows) = deployment.get("occupancy").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            if row.get("state").and_then(Value::as_str) != Some("live") {
                return None;
            }
            let row_sha = row.get("runtime_generation")?.get("sha")?.as_str()?;
            if row_sha != sha {
                return None;
            }
            row.get("name")?.as_str().map(str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

fn promoted_output_revision<'a>(
    deployment: &'a Value,
    published: Option<&'a Value>,
) -> Option<&'a str> {
    published
        .and_then(|value| value.get("output_revision"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            deployment
                .get("promoted")
                .and_then(|value| value.get("output_revision"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
}

fn retained_reasons(
    sha: &str,
    gen_dir: &Path,
    deployment: &Value,
    published: Option<&Value>,
    gens_root: &Path,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if promoted_output_revision(deployment, published) == Some(sha) {
        reasons.push("promoted".into());
    }
    if candidate_output_revision(deployment) == Some(sha) {
        reasons.push("candidate".into());
        if deployment
            .get("canary")
            .and_then(|value| value.get("result"))
            .and_then(Value::as_str)
            == Some("fail")
        {
            reasons.push("failed-canary".into());
        }
    }
    for name in live_occupancy_names(deployment, sha) {
        reasons.push(format!("live-occupancy:{name}"));
    }
    if process_executable_resolves_into(gen_dir) {
        reasons.push("process-executable".into());
    }
    if current_points_at(gens_root, sha) {
        reasons.push("current-symlink".into());
    }
    reasons
}

fn gc_refuse_message(
    sha: &str,
    deployment: &Value,
    published: Option<&Value>,
    gens_root: &Path,
) -> Option<String> {
    let gen_dir = gens_root.join(sha);
    if !gen_dir.is_dir() {
        return Some(format!("GC refused: no such generation {sha}"));
    }
    let reasons = retained_reasons(sha, &gen_dir, deployment, published, gens_root);
    if reasons.is_empty() {
        None
    } else {
        Some(format!("GC refused: {}", reasons.join(", ")))
    }
}

fn runtime_generation_projection(root: &Path, stem: &str) -> Value {
    let deploy_path = deployment_path(root, stem);
    let published_path = published_generation_path(root, stem);
    let deployment = load_json_object(&deploy_path, "deployment.json").ok();
    let published = load_json_object(&published_path, "published-generation.json").ok();
    if deployment.is_none() && published.is_none() {
        return Value::Null;
    }
    let empty = json!({});
    let deployment_ref = deployment.as_ref().unwrap_or(&empty);
    let gens_root = generations_root_of(deployment_ref);
    let mut retained = Vec::new();
    if let Some(gens_root) = gens_root.as_ref() {
        for (sha, path) in generation_directories(gens_root) {
            let reasons =
                retained_reasons(&sha, &path, deployment_ref, published.as_ref(), gens_root);
            let resolved = fs::canonicalize(&path).unwrap_or(path);
            retained.push(json!({
                "sha": sha,
                "label": runtime_generation_label(&sha),
                "path": resolved.to_string_lossy(),
                "reasons": reasons,
            }));
        }
    }
    let promoted = promoted_output_revision(deployment_ref, published.as_ref()).map(|sha| {
        let compose = published
            .as_ref()
            .and_then(|value| value.get("generation"))
            .and_then(Value::as_str)
            .or_else(|| {
                deployment_ref
                    .get("promoted")
                    .and_then(|value| value.get("generation"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("");
        let promoted_at = published
            .as_ref()
            .and_then(|value| value.get("published_at"))
            .and_then(Value::as_str)
            .or_else(|| {
                deployment_ref
                    .get("promoted")
                    .and_then(|value| value.get("promoted_at"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("");
        let path = gens_root
            .as_ref()
            .and_then(|root| fs::canonicalize(root.join(sha)).ok())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        json!({
            "sha": sha,
            "label": runtime_generation_label(sha),
            "compose_generation": compose,
            "promoted_at": promoted_at,
            "path": path,
        })
    });
    let candidate = candidate_output_revision(deployment_ref).map(|sha| {
        let cand = deployment_ref
            .get("candidate")
            .cloned()
            .unwrap_or(Value::Null);
        json!({
            "sha": sha,
            "label": runtime_generation_label(sha),
            "compose_generation": cand.get("generation").and_then(Value::as_str).unwrap_or(""),
            "phase": cand.get("phase").and_then(Value::as_str).unwrap_or(""),
            "composed_at": cand.get("composed_at").and_then(Value::as_str).unwrap_or(""),
        })
    });
    json!({
        "promoted": promoted,
        "candidate": candidate,
        "cohort": occupancy_cohort(deployment_ref),
        "retained": retained,
    })
}

fn occupancy_cohort(deployment: &Value) -> Value {
    let promoted = promoted_output_revision(deployment, None);
    let mut migrated = Vec::new();
    let mut pending = Vec::new();
    let Some(rows) = deployment.get("occupancy").and_then(Value::as_array) else {
        return json!({ "migrated": migrated, "pending": pending });
    };
    for row in rows {
        let sha = row
            .get("runtime_generation")
            .and_then(|value| value.get("sha"))
            .and_then(Value::as_str);
        let upgrade = row.get("upgrade_target");
        let waiting = match upgrade {
            None | Some(Value::Null) => false,
            Some(_) => true,
        };
        let live_behind = row.get("state").and_then(Value::as_str) == Some("live")
            && sha.is_some()
            && sha != promoted;
        if waiting || live_behind {
            pending.push(row.clone());
        } else {
            migrated.push(row.clone());
        }
    }
    json!({ "migrated": migrated, "pending": pending })
}

fn operator_deployment_phase(deployment: Option<&Value>, service_active: bool) -> Option<String> {
    let empty = json!({});
    let value = deployment.unwrap_or(&empty);
    if candidate_output_revision(value).is_none() {
        return service_active.then(|| "composing".into());
    }
    let stored = value
        .get("candidate")
        .and_then(|candidate| candidate.get("phase"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(match stored {
        "composed" => "composing".into(),
        "candidate" | "canary" | "rolling_out" | "converged" => stored.into(),
        _ => "candidate".into(),
    })
}

fn deployment_inspect(root: &Path, stem: &str, service_active: bool) -> Value {
    let loaded = load_json_object(&deployment_path(root, stem), "deployment.json").ok();
    let phase = operator_deployment_phase(loaded.as_ref(), service_active);
    match (loaded, phase) {
        (Some(mut value), Some(phase)) => {
            value["phase"] = json!(phase);
            value
        }
        (Some(mut value), None) => {
            value["phase"] = Value::Null;
            value
        }
        (None, Some(phase)) => json!({
            "phase": phase,
            "stall": Value::Null,
            "occupancy": [],
            "recovery": Value::Null,
        }),
        (None, None) => Value::Null,
    }
}

fn git_is_ancestor(worktree: &Path, revision: &str) -> bool {
    let Some(dir) = worktree.to_str() else {
        return false;
    };
    Command::new("git")
        .args(["-C", dir, "merge-base", "--is-ancestor", revision, "HEAD"])
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn desired_input_unconsumed(input: &DesiredInput, worktree: Option<&Path>) -> bool {
    let Some(worktree) = worktree.filter(|path| path.exists()) else {
        return true;
    };
    match input.artifact_kind.as_str() {
        "commit" => match input.artifact_revision.as_deref() {
            Some(revision) if !revision.is_empty() => !git_is_ancestor(worktree, revision),
            _ => true,
        },
        "worktree-delta" => {
            let Some(delta) = input.delta.as_ref() else {
                return true;
            };
            delta
                .iter()
                .any(|file| match fs::read(worktree.join(&file.path)) {
                    Ok(bytes) => sha256_hex(&bytes) != file.sha256,
                    Err(_) => true,
                })
        }
        _ => true,
    }
}

fn capability_desired_inspect(root: &Path, stem: &str) -> (Value, bool) {
    let path = desired_inputs_path(root, stem);
    let Some(bytes) = read_optional_bytes(&path).ok().flatten() else {
        return (Value::Null, false);
    };
    let Ok(desired) = serde_json::from_slice::<DesiredInputsFile>(&bytes) else {
        return (Value::Null, true);
    };
    let worktree = bound_working_directory(stem).map(PathBuf::from);
    let unconsumed = desired
        .inputs
        .iter()
        .any(|input| desired_input_unconsumed(input, worktree.as_deref()));
    (
        serde_json::to_value(&desired).unwrap_or(Value::Null),
        unconsumed,
    )
}

pub(crate) fn attach_inspect_surfaces(
    root: &Path,
    stem: &str,
    service_active: bool,
    automation: &mut Value,
) {
    let Some(object) = automation.as_object_mut() else {
        return;
    };
    if stem == RUNTIME_INTEGRATOR {
        object.insert(
            "deployment".into(),
            deployment_inspect(root, stem, service_active),
        );
        object.insert(
            "runtime_generation".into(),
            runtime_generation_projection(root, stem),
        );
    }
    if stem.starts_with("managed-omp-cap-") {
        let (desired_inputs, desired_unconsumed) = capability_desired_inspect(root, stem);
        object.insert("desired_inputs".into(), desired_inputs);
        object.insert("desired_unconsumed".into(), json!(desired_unconsumed));
    }
}

pub fn plan_deploy_occupancy(
    args: &Value,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let spec = args
        .get("spec")
        .ok_or_else(|| BackendError("missing required argument: spec".into()))?;
    let (generation, rows) = parse_occupancy_spec(spec)?;
    let stem = RUNTIME_INTEGRATOR;
    let manifest = resolved_manifest(explicit_root, cwd)?;
    crate::operator::require_owned(&manifest, stem)?;
    let deploy_path = deployment_path(&manifest.root, stem);
    let deploy_snapshot = snapshot(&deploy_path);
    let _deployment = load_json_object(&deploy_path, "deployment.json")?;
    let cfg = crate::config::current_or_load()?;
    let actors: Vec<Value> = rows.iter().map(occupancy_row_value).collect();
    let payload = json!({
        "action": "deploy-occupancy",
        "scope_root": manifest.root.to_string_lossy(),
        "generation": generation,
        "actors": actors,
        "deployment_snapshot": snapshot_json(&deploy_snapshot),
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
        "action": "deploy-occupancy",
        "issued_at": crate::config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(sealed.expires_at),
        "generation": generation,
        "occupancy": actors.len(),
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

fn apply_deploy_occupancy(
    plan: &token::SealedPlan,
    _cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let snapshot = snapshot_from_json(
        plan.payload
            .get("deployment_snapshot")
            .ok_or_else(|| BackendError("invalid automation plan token".into()))?,
    )?;
    require_snapshot_fresh(&snapshot)?;
    let rows = occupancy_from_plan(plan)?;
    let path = Path::new(&snapshot.path);
    let mut deployment = load_json_object(path, "deployment.json")?;
    let occupancy: Vec<Value> = rows.iter().map(occupancy_row_value).collect();
    deployment["occupancy"] = json!(occupancy);
    maybe_set_converged(&mut deployment);
    atomic_write(path, &serialize_pretty(&deployment)?)?;
    Ok(json!({
        "class": "automation",
        "unit": plan.unit,
        "action": "deploy-occupancy",
        "applied": true,
        "phase": deployment
            .get("candidate")
            .and_then(|value| value.get("phase"))
            .cloned()
            .unwrap_or(Value::Null),
        "occupancy": occupancy,
    }))
}

pub fn plan_deploy_gc(
    args: &Value,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let generation = args
        .get("generation")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("missing required argument: generation".into()))?;
    let generation = lowercase_hex("generation", generation, 40)?;
    let stem = RUNTIME_INTEGRATOR;
    let manifest = resolved_manifest(explicit_root, cwd)?;
    crate::operator::require_owned(&manifest, stem)?;
    let deploy_path = deployment_path(&manifest.root, stem);
    let deploy_snapshot = snapshot(&deploy_path);
    let deployment = load_json_object(&deploy_path, "deployment.json")?;
    let published = load_json_object(
        &published_generation_path(&manifest.root, stem),
        "published-generation.json",
    )
    .ok();
    let gens_root = generations_root_of(&deployment)
        .ok_or_else(|| BackendError("deployment.generations_root is missing".into()))?;
    if let Some(reason) =
        gc_refuse_message(&generation, &deployment, published.as_ref(), &gens_root)
    {
        return Err(BackendError(reason));
    }
    let cfg = crate::config::current_or_load()?;
    let payload = json!({
        "action": "deploy-gc",
        "scope_root": manifest.root.to_string_lossy(),
        "generation": generation,
        "deployment_snapshot": snapshot_json(&deploy_snapshot),
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
        "action": "deploy-gc",
        "issued_at": crate::config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(sealed.expires_at),
        "generation": generation,
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

fn apply_deploy_gc(plan: &token::SealedPlan, _cwd: Option<&str>) -> Result<Value, BackendError> {
    let snapshot = snapshot_from_json(
        plan.payload
            .get("deployment_snapshot")
            .ok_or_else(|| BackendError("invalid automation plan token".into()))?,
    )?;
    require_snapshot_fresh(&snapshot)?;
    let generation = plan
        .payload
        .get("generation")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?
        .to_string();
    let path = Path::new(&snapshot.path);
    let mut deployment = load_json_object(path, "deployment.json")?;
    let state_dir = path
        .parent()
        .ok_or_else(|| BackendError("deployment.json has no parent".into()))?;
    let published = load_json_object(
        &state_dir.join("published-generation.json"),
        "published-generation.json",
    )
    .ok();
    let gens_root = generations_root_of(&deployment)
        .ok_or_else(|| BackendError("deployment.generations_root is missing".into()))?;
    if let Some(reason) =
        gc_refuse_message(&generation, &deployment, published.as_ref(), &gens_root)
    {
        return Err(BackendError(reason));
    }
    let gen_dir = gens_root.join(&generation);
    fs::remove_dir_all(&gen_dir).map_err(|error| {
        BackendError(format!(
            "cannot remove generation {}: {error}",
            gen_dir.display()
        ))
    })?;
    if let Some(Value::Array(rows)) = deployment.get_mut("occupancy") {
        rows.retain(|row| {
            let sha = row
                .get("runtime_generation")
                .and_then(|value| value.get("sha"))
                .and_then(Value::as_str);
            sha.map(|value| value != generation.as_str())
                .unwrap_or(true)
        });
    }
    maybe_set_converged(&mut deployment);
    atomic_write(path, &serialize_pretty(&deployment)?)?;
    Ok(json!({
        "class": "automation",
        "unit": plan.unit,
        "action": "deploy-gc",
        "applied": true,
        "generation": generation,
        "removed": true,
    }))
}

struct NormalizePrSpec {
    unit: String,
    pr: String,
    merge_sha: String,
    merged_at: String,
}

fn parse_normalize_pr_spec(spec: &Value) -> Result<NormalizePrSpec, BackendError> {
    if !spec.is_object() {
        return Err(BackendError("normalize-pr spec must be an object".into()));
    }
    if spec.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(BackendError("normalize-pr spec version must be 1".into()));
    }
    let unit = spec
        .get("unit")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("spec.unit is required".into()))?;
    let merge_sha = match spec.get("merge_sha") {
        None | Some(Value::Null) => {
            return Err(BackendError("missing required argument: merge_sha".into()))
        }
        Some(Value::String(value)) if value.is_empty() => {
            return Err(BackendError("missing required argument: merge_sha".into()))
        }
        Some(Value::String(value)) => lowercase_hex("merge_sha", value, 40)?,
        Some(_) => return Err(BackendError("merge_sha must be 40 lowercase hex".into())),
    };
    let pr = match spec.get("pr") {
        Some(Value::String(value)) if !value.is_empty() && !value.contains(['\n', '\r']) => {
            value.clone()
        }
        Some(Value::Number(value)) => value.to_string(),
        _ => return Err(BackendError("pr is required".into())),
    };
    let merged_at = spec
        .get("merged_at")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.contains(['\n', '\r']))
        .ok_or_else(|| BackendError("merged_at is required".into()))?
        .to_string();
    Ok(NormalizePrSpec {
        unit: unit.to_string(),
        pr,
        merge_sha,
        merged_at,
    })
}

fn bound_working_directory(stem: &str) -> Option<String> {
    let path = systemd::unit_file_dir().join(format!("{stem}.service"));
    let text = fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("WorkingDirectory=")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn worktree_is_registered(scope_root: &Path, cwd: &Path) -> bool {
    let Some(root) = scope_root.to_str() else {
        return false;
    };
    let Ok(output) = Command::new("git")
        .args(["-C", root, "worktree", "list", "--porcelain"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let wanted = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .any(|listed| listed.canonicalize().unwrap_or(listed) == wanted)
}

fn notify_normalize_pr(pr: &str, merge_sha: &str, unit: &str) {
    let message = format!("pr={pr} merge_sha={merge_sha} unit={unit}");
    if let Some(lib) = std::env::var("SYSTEM_NOTIFY_LIB")
        .ok()
        .filter(|value| !value.is_empty())
    {
        let _ = Command::new(lib)
            .args(["omp-runtime", "inform", &message])
            .status();
        return;
    }
    let lib = dogfood_lib("system-notify");
    let _ = Command::new("bash")
        .args([
            "-c",
            "source \"$1\"; system_notify omp-runtime inform \"$2\"",
            "system-notify",
            lib.to_str().unwrap_or(""),
            &message,
        ])
        .status();
}

fn remove_registered_worktree(scope_root: &Path, cwd: &Path) -> Result<bool, BackendError> {
    if !cwd.exists() || !worktree_is_registered(scope_root, cwd) {
        return Ok(false);
    }
    let Some(root) = scope_root.to_str() else {
        return Ok(false);
    };
    let _ = Command::new("git")
        .args(["-C", root, "worktree", "remove", "--force"])
        .arg(cwd)
        .status();
    if cwd.exists() {
        fs::remove_dir_all(cwd).map_err(|error| {
            BackendError(format!("cannot remove worktree {}: {error}", cwd.display()))
        })?;
    }
    Ok(!cwd.exists())
}

pub fn plan_normalize_pr(
    args: &Value,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let spec = args
        .get("spec")
        .ok_or_else(|| BackendError("missing required argument: spec".into()))?;
    let parsed = parse_normalize_pr_spec(spec)?;
    let manifest = resolved_manifest(explicit_root, cwd)?;
    crate::operator::require_owned(&manifest, &parsed.unit)?;
    let nested = plan_retire(&parsed.unit, explicit_root, cwd)?;
    let retire_plan_token = nested
        .get("plan_token")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("automation retire plan did not return a plan token".into()))?;
    let bound_cwd = bound_working_directory(&parsed.unit);
    let cfg = crate::config::current_or_load()?;
    let payload = json!({
        "action": "normalize-pr",
        "retire_plan_token": retire_plan_token,
        "scope_root": manifest.root.to_string_lossy(),
        "pr": parsed.pr,
        "merge_sha": parsed.merge_sha,
        "merged_at": parsed.merged_at,
        "bound_cwd": bound_cwd,
    });
    let (plan_token, sealed) = token::mint(
        &cfg,
        PlanClass::Automation,
        &parsed.unit,
        cwd.map(str::to_string),
        payload,
    )?;
    Ok(json!({
        "plan_token": plan_token,
        "class": "automation",
        "unit": parsed.unit,
        "action": "normalize-pr",
        "issued_at": crate::config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(sealed.expires_at),
        "pr": parsed.pr,
        "merge_sha": parsed.merge_sha,
        "systemd": nested,
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

fn apply_normalize_pr(plan: &token::SealedPlan, cwd: Option<&str>) -> Result<Value, BackendError> {
    let pr = plan
        .payload
        .get("pr")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    let merge_sha = plan
        .payload
        .get("merge_sha")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    let scope_root = plan
        .payload
        .get("scope_root")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    let retire_plan_token = plan
        .payload
        .get("retire_plan_token")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    notify_normalize_pr(pr, merge_sha, &plan.unit);
    let systemd_result = crate::write::apply_with_context(retire_plan_token, cwd)?;
    let worktree_removed = match plan.payload.get("bound_cwd").and_then(Value::as_str) {
        Some(bound) => remove_registered_worktree(Path::new(scope_root), Path::new(bound))?,
        None => false,
    };
    Ok(json!({
        "class": "automation",
        "unit": plan.unit,
        "action": "normalize-pr",
        "applied": true,
        "pr": pr,
        "merge_sha": merge_sha,
        "worktree_removed": worktree_removed,
        "systemd": systemd_result,
    }))
}

struct NormalizeCapabilitySpec {
    unit: String,
}

fn parse_normalize_capability_spec(spec: &Value) -> Result<NormalizeCapabilitySpec, BackendError> {
    if !spec.is_object() {
        return Err(BackendError(
            "normalize-capability spec must be an object".into(),
        ));
    }
    if spec.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(BackendError(
            "normalize-capability spec version must be 1".into(),
        ));
    }
    let unit = spec
        .get("unit")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.contains(['\n', '\r']))
        .ok_or_else(|| BackendError("spec.unit is required".into()))?;
    Ok(NormalizeCapabilitySpec {
        unit: unit.to_string(),
    })
}

fn capability_run_lib() -> PathBuf {
    if let Ok(value) = std::env::var("CAPABILITY_RUN_LIB") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dogfood/drivers/capability-run")
}

fn current_upstream_generation(root: &Path) -> Result<String, BackendError> {
    if let Ok(value) = std::env::var("UPSTREAM_GENERATION") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let path = operation_home(root, "managed-omp-release-watch").join("state/generation.json");
    let parsed = load_json_object(&path, "generation.json")?;
    parsed
        .get("target_commit")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| BackendError("missing current upstream generation".into()))
}

fn invoke_capability_pass_is_verified_noop(
    generation: &str,
    worktree: &Path,
) -> Result<bool, BackendError> {
    let lib = capability_run_lib();
    let lib = lib
        .to_str()
        .ok_or_else(|| BackendError("capability-run path is not utf-8".into()))?;
    let worktree = worktree
        .to_str()
        .ok_or_else(|| BackendError("worktree path is not utf-8".into()))?;
    // Source only the named predicate. Do not execute capability-run's driver body.
    let output = Command::new("bash")
        .args([
            "-c",
            r#"
set -euo pipefail
lib=$1
generation=$2
worktree=$3
source /dev/stdin <<<"$(sed -n '/^capability_pass_is_verified_noop()/,/^}/p' "$lib")"
capability_pass_is_verified_noop 0 "$generation" "$worktree"
"#,
            "capability-verified-noop",
            lib,
            generation,
            worktree,
        ])
        .output()
        .map_err(|error| {
            BackendError(format!(
                "cannot invoke capability_pass_is_verified_noop: {error}"
            ))
        })?;
    Ok(output.status.success())
}

fn pr_maintainer_children(
    manifest: &ScopeManifest,
    parent: &str,
) -> Result<Vec<String>, BackendError> {
    let dir = systemd::unit_file_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(BackendError(format!(
                "cannot read unit directory {}: {error}",
                dir.display()
            )))
        }
    };
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            BackendError(format!(
                "cannot read unit directory {}: {error}",
                dir.display()
            ))
        })?;
        let name = match entry.file_name().to_str() {
            Some(name) if name.ends_with(".service") => name.to_string(),
            _ => continue,
        };
        let stem = name.trim_end_matches(".service");
        if stem == parent {
            continue;
        }
        if !manifest
            .owned
            .iter()
            .any(|glob| crate::scope::glob_matches_stem(glob, stem))
        {
            continue;
        }
        let Some(metadata) = load_metadata(&manifest.root, stem)? else {
            continue;
        };
        if metadata.agent == "pr-maintainer" && metadata.parent.as_deref() == Some(parent) {
            children.push(stem.to_string());
        }
    }
    children.sort();
    Ok(children)
}

fn notify_normalize_capability(unit: &str) {
    let message = format!("unit={unit}");
    if let Some(lib) = std::env::var("SYSTEM_NOTIFY_LIB")
        .ok()
        .filter(|value| !value.is_empty())
    {
        let _ = Command::new(lib)
            .args(["omp-runtime", "inform", &message])
            .status();
        return;
    }
    let lib = dogfood_lib("system-notify");
    let _ = Command::new("bash")
        .args([
            "-c",
            "source \"$1\"; system_notify omp-runtime inform \"$2\"",
            "system-notify",
            lib.to_str().unwrap_or(""),
            &message,
        ])
        .status();
}

pub fn plan_normalize_capability(
    args: &Value,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let spec = args
        .get("spec")
        .ok_or_else(|| BackendError("missing required argument: spec".into()))?;
    let parsed = parse_normalize_capability_spec(spec)?;
    let manifest = resolved_manifest(explicit_root, cwd)?;
    crate::operator::require_owned(&manifest, &parsed.unit)?;
    let bound_cwd = bound_working_directory(&parsed.unit)
        .ok_or_else(|| BackendError("capability has no WorkingDirectory to verify".into()))?;
    let generation = current_upstream_generation(&manifest.root)?;
    if !invoke_capability_pass_is_verified_noop(&generation, Path::new(&bound_cwd))? {
        return Err(BackendError(
            "verified_noop failed against the current upstream generation".into(),
        ));
    }
    let children = pr_maintainer_children(&manifest, &parsed.unit)?;
    if !children.is_empty() {
        return Err(BackendError(format!(
            "pr-maintainer child remains: {}",
            children.join(", ")
        )));
    }
    let nested = plan_retire(&parsed.unit, explicit_root, cwd)?;
    let retire_plan_token = nested
        .get("plan_token")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("automation retire plan did not return a plan token".into()))?;
    let cfg = crate::config::current_or_load()?;
    let payload = json!({
        "action": "normalize-capability",
        "retire_plan_token": retire_plan_token,
        "scope_root": manifest.root.to_string_lossy(),
        "bound_cwd": bound_cwd,
    });
    let (plan_token, sealed) = token::mint(
        &cfg,
        PlanClass::Automation,
        &parsed.unit,
        cwd.map(str::to_string),
        payload,
    )?;
    Ok(json!({
        "plan_token": plan_token,
        "class": "automation",
        "unit": parsed.unit,
        "action": "normalize-capability",
        "issued_at": crate::config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": crate::config::unix_to_rfc3339(sealed.expires_at),
        "systemd": nested,
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

fn apply_normalize_capability(
    plan: &token::SealedPlan,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let scope_root = plan
        .payload
        .get("scope_root")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    let retire_plan_token = plan
        .payload
        .get("retire_plan_token")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid automation plan token".into()))?;
    notify_normalize_capability(&plan.unit);
    let systemd_result = crate::write::apply_with_context(retire_plan_token, cwd)?;
    let worktree_removed = match plan.payload.get("bound_cwd").and_then(Value::as_str) {
        Some(bound) => remove_registered_worktree(Path::new(scope_root), Path::new(bound))?,
        None => false,
    };
    Ok(json!({
        "class": "automation",
        "unit": plan.unit,
        "action": "normalize-capability",
        "applied": true,
        "worktree_removed": worktree_removed,
        "systemd": systemd_result,
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
        "admit" => apply_admit(plan, cwd),
        "deploy-canary-ack" => apply_deploy_canary_ack(plan, cwd),
        "deploy-occupancy" => apply_deploy_occupancy(plan, cwd),
        "deploy-gc" => apply_deploy_gc(plan, cwd),
        "normalize-pr" => apply_normalize_pr(plan, cwd),
        "normalize-capability" => apply_normalize_capability(plan, cwd),
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
    let mut automation = operation.get("automation").cloned().unwrap_or(Value::Null);
    let service_active = operation.get("state").and_then(Value::as_str) == Some("active");
    attach_inspect_surfaces(&manifest.root, stem, service_active, &mut automation);
    let mut payload = json!({
        "unit": stem,
        "metadata_file": metadata_path(&manifest.root, stem),
        "automation": automation.clone(),
        "relations": operation.get("relations").cloned().unwrap_or(Value::Null),
        "operation": operation,
    });
    if stem == RUNTIME_INTEGRATOR {
        payload["runtime_generation"] = automation
            .get("runtime_generation")
            .cloned()
            .unwrap_or(Value::Null);
        payload["deployment"] = automation.get("deployment").cloned().unwrap_or(Value::Null);
    }
    if stem.starts_with("managed-omp-cap-") {
        payload["desired_inputs"] = automation
            .get("desired_inputs")
            .cloned()
            .unwrap_or(Value::Null);
        payload["desired_unconsumed"] = automation
            .get("desired_unconsumed")
            .cloned()
            .unwrap_or(json!(false));
    }
    Ok(payload)
}

pub fn inspect_operation_view(
    name: &str,
    explicit_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value, BackendError> {
    let mut view = crate::operations::get_operation(name)?;
    let stem = view
        .get("unit")
        .and_then(Value::as_str)
        .unwrap_or(name)
        .to_string();
    let Ok(manifest) = resolved_manifest(explicit_root, cwd) else {
        return Ok(view);
    };
    let (mut automation, _) = join_operation(&manifest, &stem);
    let service_active = view.get("state").and_then(Value::as_str) == Some("active");
    attach_inspect_surfaces(&manifest.root, &stem, service_active, &mut automation);
    if let Some(object) = view.as_object_mut() {
        object.insert("automation".into(), automation);
    }
    Ok(view)
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
        let _guard = crate::systemd::TEST_ENV_LOCK
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
        let _guard = crate::systemd::TEST_ENV_LOCK
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
        let _guard = crate::systemd::TEST_ENV_LOCK
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
    fn native_provisioning_is_a_known_blocker_kind() {
        let root = tmp_root("native-provisioning-kind");
        with_bound_operation(&root, "managed-child", || {
            write_parent_fixture(&root, "managed-child", None);
            let written = write_blocker(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "native-provisioning",
                Some("parent"),
                Some("fp-native"),
                None,
                Some(4),
                "native addon missing or export-deficient",
            )
            .unwrap();
            assert_eq!(written["changed"], true);
            assert_eq!(written["blocker"]["kind"], "native-provisioning");
            let err = write_blocker(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "not-a-kind",
                Some("parent"),
                Some("fp-native"),
                None,
                Some(4),
                "bogus kind",
            )
            .unwrap_err();
            assert!(err.0.contains("unknown blocker kind"));
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
        let first = effective_input_fingerprint("world-a", "brain-a", None);
        assert_eq!(
            first,
            effective_input_fingerprint("world-a", "brain-a", None)
        );
        assert_ne!(
            first,
            effective_input_fingerprint("world-a", "brain-b", None)
        );
    }

    #[test]
    fn effective_input_without_desired_inputs_matches_world_brain_formula() {
        let expected = sha256_hex(b"world=world-a\nbrain=brain-a\n");
        assert_eq!(
            effective_input_fingerprint("world-a", "brain-a", None),
            expected
        );
    }

    #[test]
    fn write_observation_hashes_desired_inputs_file_bytes_when_present() {
        let root = tmp_root("desired-inputs-hash");
        with_bound_operation(&root, "managed-proof", || {
            write_parent_fixture(&root, "managed-proof", None);
            let expected_absent = sha256_hex(b"world=world-a\nbrain=brain-a\n");
            let absent = write_observation(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "world-a",
                "brain-a",
                None,
            )
            .unwrap();
            assert_eq!(
                absent["observation"]["input_fingerprint"].as_str(),
                Some(expected_absent.as_str())
            );

            let desired_path =
                operation_home(&root, "managed-proof").join("state/desired-inputs.json");
            let desired_bytes = br#"{"version":1,"inputs":[{"originating_work":"work-a"}]}"#;
            fs::create_dir_all(desired_path.parent().unwrap()).unwrap();
            fs::write(&desired_path, desired_bytes).unwrap();

            let present = write_observation(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "world-a",
                "brain-a",
                None,
            )
            .unwrap();
            let present_hex = present["observation"]["input_fingerprint"]
                .as_str()
                .expect("input_fingerprint");
            assert_ne!(present_hex, expected_absent);
            let expected_present = sha256_hex(
                format!(
                    "world=world-a\nbrain=brain-a\ndesired={}\n",
                    sha256_hex(desired_bytes)
                )
                .as_bytes(),
            );
            assert_eq!(present_hex, expected_present);

            fs::remove_file(&desired_path).unwrap();
            let restored = write_observation(
                Some(root.to_str().unwrap()),
                Some(root.to_str().unwrap()),
                "world-a",
                "brain-a",
                None,
            )
            .unwrap();
            assert_eq!(
                restored["observation"]["input_fingerprint"].as_str(),
                Some(expected_absent.as_str())
            );
        });
        let _ = fs::remove_dir_all(root);
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
