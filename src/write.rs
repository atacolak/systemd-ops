//! The write path. Nothing mutates without a plan.
//!
//! A change is made in two steps. `plan` reads the unit's current state
//! and returns it with the predicted state, the rollback action, and a
//! sealed plan token, executing nothing. `apply` takes the token,
//! re-reads the state, refuses if it no longer matches what the plan
//! was made against, executes, and returns a before/after diff.
//! Tokens are HMAC-sealed, expire, and are not a replay ledger.
//!
//! The one mutating process invocation in the program is here, at the
//! end of `apply`, reachable through no other path.

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

/// Which state dimension an action changes, and therefore which
/// property the plan records and the apply precondition re-checks:
/// `ActiveState` for lifecycle actions, `UnitFileState` for enablement,
/// the LogControl1 value for log tuning.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    UnitState,
    FileState,
    LogControl,
}

/// Accepted values for the log-control actions, checked before argv.
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
        match s {
            "start" => Some(Action::Start),
            "stop" => Some(Action::Stop),
            "restart" => Some(Action::Restart),
            "reload" => Some(Action::Reload),
            "enable" => Some(Action::Enable),
            "disable" => Some(Action::Disable),
            "reset-failed" => Some(Action::ResetFailed),
            "mask" => Some(Action::Mask),
            "unmask" => Some(Action::Unmask),
            "log-level" => Some(Action::LogLevel),
            "log-target" => Some(Action::LogTarget),
            _ => None,
        }
    }

    /// The wire name, as accepted by plan_change.
    pub fn name(self) -> &'static str {
        match self {
            Action::LogLevel => "log-level",
            Action::LogTarget => "log-target",
            other => other.verb(),
        }
    }

    /// The systemctl verb. The names are systemd's own.
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

    fn kind(self) -> Kind {
        match self {
            Action::Start
            | Action::Stop
            | Action::Restart
            | Action::Reload
            | Action::ResetFailed => Kind::UnitState,
            Action::Enable | Action::Disable | Action::Mask | Action::Unmask => Kind::FileState,
            Action::LogLevel | Action::LogTarget => Kind::LogControl,
        }
    }

    /// Whether this action takes a value ("debug", "journal", ...).
    pub fn takes_value(self) -> bool {
        self.kind() == Kind::LogControl
    }

    /// The action that undoes this one. Restart and reload have no
    /// inverse; the reply reports null for them. Log-control actions
    /// invert to themselves with the previously observed value.
    fn inverse(self) -> Option<Action> {
        match self {
            Action::Start => Some(Action::Stop),
            Action::Stop => Some(Action::Start),
            Action::Restart | Action::Reload | Action::ResetFailed => None,
            Action::Enable => Some(Action::Disable),
            Action::Disable => Some(Action::Enable),
            Action::Mask => Some(Action::Unmask),
            Action::Unmask => Some(Action::Mask),
            Action::LogLevel => Some(Action::LogLevel),
            Action::LogTarget => Some(Action::LogTarget),
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

    /// The JSON key for this action's state dimension, used in the
    /// `current`, `predicted`, and `diff` objects.
    pub fn state_key(self) -> &'static str {
        match self {
            Action::LogLevel => "log_level",
            Action::LogTarget => "log_target",
            other => match other.kind() {
                Kind::UnitState => "active",
                Kind::FileState => "unit_file_state",
                Kind::LogControl => unreachable!("log actions matched above"),
            },
        }
    }

    /// Compact surface hides mask/unmask and log-control. Full surface
    /// keeps the upstream set plus reset-failed.
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
    match action.kind() {
        Kind::UnitState => systemd::unit_state(unit),
        Kind::FileState => Ok((systemd::unit_file_state(unit)?, String::new())),
        Kind::LogControl => Ok((
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
    if action.kind() == Kind::UnitState {
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
    let payload = json!({
        "verb": work.verb.as_str(),
        "spec": work.spec.as_ref().map(|s| s.to_json()),
        "snapshots": work.snapshots.iter().map(|s| json!({
            "path": s.path,
            "sha256": s.sha256,
        })).collect::<Vec<_>>(),
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
    out["files"] = json!(work
        .snapshots
        .iter()
        .map(|s| json!({
            "path": s.path,
            "sha256": s.sha256,
        }))
        .collect::<Vec<_>>());
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
        PlanClass::Control => apply_control(&plan, cwd),
        PlanClass::Author => apply_author(&plan, cwd),
    }
}

fn apply_control(plan: &token::SealedPlan, _cwd: Option<&str>) -> Result<Value, BackendError> {
    let action_name = plan
        .payload
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid plan token".into()))?;
    let action = Action::parse(action_name)
        .ok_or_else(|| BackendError(format!("unknown action '{action_name}'")))?;
    systemd::require_write_unit(&plan.unit)?;
    let observed = plan
        .payload
        .get("observed")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let sub = plan
        .payload
        .get("sub")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let value = plan
        .payload
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_string);
    let (observed_now, _) = observe(action, &plan.unit)?;
    if observed_now != observed {
        return Err(BackendError(format!(
            "plan is stale: '{}' was {observed} at plan time but is {observed_now} now; re-plan",
            plan.unit
        )));
    }
    let changes = systemd::apply_verb(action.verb(), &plan.unit, value.as_deref())?;
    let (observed_after, sub_after) = observe(action, &plan.unit)?;
    let key = action.state_key();
    let mut diff = json!({
        key: { "before": observed, "after": observed_after },
    });
    if action.kind() == Kind::UnitState {
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
    let verb = AuthoringVerb::parse(
        plan.payload
            .get("verb")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )?;
    let snapshots = snapshots_from_json(plan.payload.get("snapshots"))?;
    let spec = match plan.payload.get("spec") {
        Some(Value::Null) | None => None,
        Some(v) => Some(crate::operations::NormalizedSpec::from_json(v)?),
    };
    let work = AuthoringWork {
        verb,
        spec,
        snapshots,
        origin_cwd: plan.origin_cwd.clone(),
    };
    crate::operations::apply_authoring(&plan.unit, work, cwd)
}

fn snapshots_from_json(v: Option<&Value>) -> Result<Vec<FileSnapshot>, BackendError> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in arr {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError("invalid plan token".into()))?
            .to_string();
        let sha256 = item
            .get("sha256")
            .and_then(Value::as_str)
            .map(str::to_string);
        out.push(FileSnapshot { path, sha256 });
    }
    Ok(out)
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
        // unmask's outcome depends on the unit's install configuration.
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
        // Log-control actions invert to themselves with the previous
        // value; lifecycle inverses carry no value.
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
