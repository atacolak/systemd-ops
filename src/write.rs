//! Control and authoring writes. Nothing mutates without a sealed token.
//!
//! `plan` / `plan_authoring` mint an HMAC token that records the
//! observed precondition. `apply` re-reads that precondition, refuses
//! drift, then calls the single mutating backend (`systemd::apply_verb`
//! or `operations::apply_authoring`). Tokens are not a process-local
//! ledger; expiry and HMAC live in `crate::token`.

use serde_json::{json, Value};

use crate::config;
use crate::systemd::{self, BackendError};
use crate::token::{self, PlanClass};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Reload,
    Enable,
    Disable,
    ResetFailed,
    Mask,
    Unmask,
    LogLevel,
    LogTarget,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dimension {
    Active,
    UnitFile,
    Log,
}

pub const LOG_LEVELS: &[&str] = &[
    "emerg", "alert", "crit", "err", "warning", "notice", "info", "debug",
];
pub const LOG_TARGETS: &[&str] = &[
    "console",
    "kmsg",
    "journal",
    "journal-or-kmsg",
    "auto",
    "null",
];

impl Action {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "start" => Action::Start,
            "stop" => Action::Stop,
            "restart" => Action::Restart,
            "reload" => Action::Reload,
            "enable" => Action::Enable,
            "disable" => Action::Disable,
            "reset-failed" => Action::ResetFailed,
            "mask" => Action::Mask,
            "unmask" => Action::Unmask,
            "log-level" => Action::LogLevel,
            "log-target" => Action::LogTarget,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Action::LogLevel => "log-level",
            Action::LogTarget => "log-target",
            other => other.verb(),
        }
    }

    pub fn verb(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
            Action::Reload => "reload",
            Action::Enable => "enable",
            Action::Disable => "disable",
            Action::ResetFailed => "reset-failed",
            Action::Mask => "mask",
            Action::Unmask => "unmask",
            Action::LogLevel => "service-log-level",
            Action::LogTarget => "service-log-target",
        }
    }

    fn dimension(self) -> Dimension {
        match self {
            Action::Start
            | Action::Stop
            | Action::Restart
            | Action::Reload
            | Action::ResetFailed => Dimension::Active,
            Action::Enable | Action::Disable | Action::Mask | Action::Unmask => Dimension::UnitFile,
            Action::LogLevel | Action::LogTarget => Dimension::Log,
        }
    }

    pub fn takes_value(self) -> bool {
        matches!(self.dimension(), Dimension::Log)
    }

    fn inverse(self) -> Option<Action> {
        match self {
            Action::Start => Some(Action::Stop),
            Action::Stop => Some(Action::Start),
            Action::Enable => Some(Action::Disable),
            Action::Disable => Some(Action::Enable),
            Action::Mask => Some(Action::Unmask),
            Action::Unmask => Some(Action::Mask),
            Action::LogLevel | Action::LogTarget => Some(self),
            Action::Restart | Action::Reload | Action::ResetFailed => None,
        }
    }

    pub fn predicted(self) -> Option<&'static str> {
        match self {
            Action::Start | Action::Restart | Action::Reload => Some("active"),
            Action::Stop => Some("inactive"),
            Action::Enable => Some("enabled"),
            Action::Disable => Some("disabled"),
            Action::Mask => Some("masked"),
            Action::ResetFailed | Action::Unmask | Action::LogLevel | Action::LogTarget => None,
        }
    }

    pub fn state_key(self) -> &'static str {
        match self {
            Action::LogLevel => "log_level",
            Action::LogTarget => "log_target",
            Action::Start
            | Action::Stop
            | Action::Restart
            | Action::Reload
            | Action::ResetFailed => "active",
            Action::Enable | Action::Disable | Action::Mask | Action::Unmask => "unit_file_state",
        }
    }

    pub fn visible_on_surface(self) -> bool {
        match systemd::surface() {
            systemd::Surface::Full => true,
            systemd::Surface::Compact => matches!(
                self,
                Action::Start
                    | Action::Stop
                    | Action::Restart
                    | Action::Reload
                    | Action::Enable
                    | Action::Disable
                    | Action::ResetFailed
            ),
        }
    }
}

pub fn action_visible(action: Action) -> bool {
    action.visible_on_surface()
}

#[derive(Clone, Debug)]
pub struct FileSnapshot {
    pub path: String,
    pub sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthoringVerb {
    Create,
    Update,
    Retire,
}

impl AuthoringVerb {
    pub fn parse(s: &str) -> Result<Self, BackendError> {
        match s {
            "create" => Ok(AuthoringVerb::Create),
            "update" => Ok(AuthoringVerb::Update),
            "retire" => Ok(AuthoringVerb::Retire),
            other => Err(BackendError(format!("unknown authoring verb '{other}'"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AuthoringVerb::Create => "create",
            AuthoringVerb::Update => "update",
            AuthoringVerb::Retire => "retire",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuthoringWork {
    pub verb: AuthoringVerb,
    pub spec: Option<crate::operations::NormalizedSpec>,
    pub snapshots: Vec<FileSnapshot>,
    pub origin_cwd: Option<String>,
}

pub fn observe(action: Action, unit: &str) -> Result<(String, String), BackendError> {
    match action.dimension() {
        Dimension::Active => systemd::unit_state(unit),
        Dimension::UnitFile => Ok((systemd::unit_file_state(unit)?, String::new())),
        Dimension::Log => Ok((
            systemd::service_log_get(action.verb(), unit)?,
            String::new(),
        )),
    }
}

pub fn rollback_json(action: Action, observed: &str) -> Value {
    match action.inverse() {
        None => Value::Null,
        Some(inverse) if inverse.takes_value() => {
            json!({ "action": inverse.name(), "value": observed })
        }
        Some(inverse) => json!({ "action": inverse.name() }),
    }
}

pub fn plan(action: Action, unit: &str, value: Option<&str>) -> Result<Value, BackendError> {
    systemd::validate_unit_name(unit)?;
    systemd::require_write_unit(unit)?;
    systemd::ensure_unit_known(unit)?;
    let (observed, sub) = observe(action, unit)?;
    let key = action.state_key();
    let mut current = json!({ key: observed });
    if action.dimension() == Dimension::Active {
        current["sub"] = json!(sub);
    }
    let predicted = value
        .map(Value::from)
        .unwrap_or_else(|| action.predicted().map(Value::from).unwrap_or(Value::Null));
    let cfg = config::current_or_load()?;
    let (token, sealed) = token::mint(
        &cfg,
        PlanClass::Control,
        unit,
        None,
        json!({
            "action": action.name(),
            "observed": observed,
            "sub": sub,
            "value": value,
        }),
    )?;
    Ok(json!({
        "plan_token": token,
        "class": "control",
        "unit": unit,
        "action": action.name(),
        "value": value,
        "issued_at": config::unix_to_rfc3339(sealed.issued_at),
        "expires_at": config::unix_to_rfc3339(sealed.expires_at),
        "current": current,
        "predicted": { key: predicted },
        "rollback": rollback_json(action, &observed),
        "note": "nothing has been executed; apply with the plan_token",
    }))
}

pub fn plan_authoring(work: AuthoringWork, extra: Value) -> Result<Value, BackendError> {
    let cfg = config::current_or_load()?;
    let unit = work
        .spec
        .as_ref()
        .map(|s| s.unit.clone())
        .or_else(|| {
            extra
                .get("unit")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let files: Vec<Value> = work
        .snapshots
        .iter()
        .map(|s| json!({ "path": s.path, "sha256": s.sha256 }))
        .collect();
    let payload = json!({
        "verb": work.verb.as_str(),
        "spec": work.spec.as_ref().map(|s| s.to_json()),
        "snapshots": files,
        "origin_cwd": work.origin_cwd,
    });
    let (token, sealed) = token::mint(
        &cfg,
        PlanClass::Author,
        &unit,
        work.origin_cwd.clone(),
        payload,
    )?;
    let mut out = extra;
    out["plan_token"] = json!(token);
    out["class"] = json!("author");
    out["unit"] = json!(unit);
    out["action"] = json!(work.verb.as_str());
    out["issued_at"] = json!(config::unix_to_rfc3339(sealed.issued_at));
    out["expires_at"] = json!(config::unix_to_rfc3339(sealed.expires_at));
    out["files"] = json!(files);
    out["note"] = json!("nothing has been executed; apply with the plan_token");
    Ok(out)
}

pub fn apply(token: &str) -> Result<Value, BackendError> {
    apply_with_context(token, None)
}

pub fn apply_with_context(token: &str, cwd: Option<&str>) -> Result<Value, BackendError> {
    let cfg = config::current_or_load()?;
    let plan = token::parse(&cfg, token)?;
    match plan.class {
        PlanClass::Control => apply_control(&plan),
        PlanClass::Author => apply_author(&plan, cwd),
    }
}

fn payload_str(plan: &token::SealedPlan, key: &str) -> Option<String> {
    plan.payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn apply_control(plan: &token::SealedPlan) -> Result<Value, BackendError> {
    let action_name = payload_str(plan, "action").unwrap_or_default();
    let action =
        Action::parse(&action_name).ok_or_else(|| BackendError("invalid plan token".into()))?;
    systemd::require_write_unit(&plan.unit)?;
    let observed = payload_str(plan, "observed").unwrap_or_default();
    let sub = payload_str(plan, "sub").unwrap_or_default();
    let value = payload_str(plan, "value");
    let (now, _) = observe(action, &plan.unit)?;
    if now != observed {
        return Err(BackendError(format!(
            "plan is stale: '{}' was {observed} at plan time but is {now} now; re-plan",
            plan.unit
        )));
    }
    let changes = systemd::apply_verb(action.verb(), &plan.unit, value.as_deref())?;
    let (after, sub_after) = observe(action, &plan.unit)?;
    let key = action.state_key();
    let mut diff = json!({
        key: { "before": observed, "after": after },
    });
    if action.dimension() == Dimension::Active {
        diff["sub"] = json!({ "before": sub, "after": sub_after });
    }
    Ok(json!({
        "class": "control",
        "unit": plan.unit,
        "action": action.name(),
        "applied": true,
        "diff": diff,
        "changes": changes,
        "rollback": rollback_json(action, &observed),
    }))
}

fn apply_author(plan: &token::SealedPlan, cwd: Option<&str>) -> Result<Value, BackendError> {
    token::require_class(plan, PlanClass::Author)?;
    let verb = AuthoringVerb::parse(payload_str(plan, "verb").as_deref().unwrap_or(""))?;
    let snapshots = snapshots_from_json(plan.payload.get("snapshots"))?;
    let spec = match plan.payload.get("spec") {
        Some(Value::Null) | None => None,
        Some(v) => Some(crate::operations::NormalizedSpec::from_json(v)?),
    };
    crate::operations::apply_authoring(
        &plan.unit,
        AuthoringWork {
            verb,
            spec,
            snapshots,
            origin_cwd: plan.origin_cwd.clone(),
        },
        cwd,
    )
}

fn snapshots_from_json(v: Option<&Value>) -> Result<Vec<FileSnapshot>, BackendError> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    arr.iter()
        .map(|item| {
            let path = item
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| BackendError("invalid plan token".into()))?
                .to_string();
            Ok(FileSnapshot {
                path,
                sha256: item
                    .get("sha256")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions() {
        assert_eq!(Action::parse("start"), Some(Action::Start));
        assert_eq!(Action::parse("reset-failed"), Some(Action::ResetFailed));
        assert_eq!(Action::parse("enable"), Some(Action::Enable));
        assert_eq!(Action::parse("isolate"), None);
        assert_eq!(Action::Start.inverse(), Some(Action::Stop));
        assert_eq!(Action::Stop.inverse(), Some(Action::Start));
        assert_eq!(Action::Restart.inverse(), None);
        assert_eq!(Action::Reload.inverse(), None);
        assert_eq!(Action::Enable.inverse(), Some(Action::Disable));
        assert_eq!(Action::Mask.inverse(), Some(Action::Unmask));
        assert_eq!(Action::Unmask.inverse(), Some(Action::Mask));
        assert_eq!(Action::Stop.predicted(), Some("inactive"));
        assert_eq!(Action::Restart.predicted(), Some("active"));
        assert_eq!(Action::Enable.predicted(), Some("enabled"));
        assert_eq!(Action::Mask.predicted(), Some("masked"));
        assert_eq!(Action::Unmask.predicted(), None);
        assert_eq!(Action::Start.state_key(), "active");
        assert_eq!(Action::Enable.state_key(), "unit_file_state");
    }

    #[test]
    fn log_control_actions() {
        assert_eq!(Action::parse("log-level"), Some(Action::LogLevel));
        assert_eq!(Action::parse("log-target"), Some(Action::LogTarget));
        assert_eq!(Action::LogLevel.verb(), "service-log-level");
        assert_eq!(Action::LogLevel.name(), "log-level");
        assert!(Action::LogLevel.takes_value());
        assert!(!Action::Start.takes_value());
        assert_eq!(Action::LogLevel.state_key(), "log_level");
        assert_eq!(Action::LogTarget.state_key(), "log_target");
        assert_eq!(
            rollback_json(Action::LogLevel, "info"),
            serde_json::json!({ "action": "log-level", "value": "info" })
        );
        assert_eq!(
            rollback_json(Action::Start, "inactive"),
            serde_json::json!({ "action": "stop" })
        );
        assert_eq!(rollback_json(Action::Restart, "active"), Value::Null);
        assert!(LOG_LEVELS.contains(&"debug") && LOG_TARGETS.contains(&"journal"));
    }

    #[test]
    fn unknown_plans_are_refused() {
        let err = apply("not-a-token").unwrap_err();
        assert!(
            err.0.contains("invalid plan token") || err.0.contains("expired"),
            "got: {err}"
        );
    }
}
