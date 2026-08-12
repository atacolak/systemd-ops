//! The write path. Nothing mutates without a plan.
//!
//! A change is made in two steps. `plan` reads the unit's current state
//! and returns it with the predicted state, the rollback action, and a
//! plan id, executing nothing. `apply` takes the id, re-reads the
//! state, refuses if it no longer matches what the plan was made
//! against, executes, and returns a before/after diff. Plans are
//! single-use and live only as long as the server process; a stale or
//! unknown plan produces an error directing the client to re-plan.
//!
//! The one mutating process invocation in the program is here, at the
//! end of `apply`, reachable through no other path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::systemd::{self, BackendError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Reload,
    Enable,
    Disable,
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
            "mask" => Some(Action::Mask),
            "unmask" => Some(Action::Unmask),
            "log-level" => Some(Action::LogLevel),
            "log-target" => Some(Action::LogTarget),
            _ => None,
        }
    }

    /// The wire name, as accepted by plan_change.
    fn name(self) -> &'static str {
        match self {
            Action::LogLevel => "log-level",
            Action::LogTarget => "log-target",
            other => other.verb(),
        }
    }

    /// The systemctl verb. The names are systemd's own.
    fn verb(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
            Action::Reload => "reload",
            Action::Enable => "enable",
            Action::Disable => "disable",
            Action::Mask => "mask",
            Action::Unmask => "unmask",
            Action::LogLevel => "service-log-level",
            Action::LogTarget => "service-log-target",
        }
    }

    fn kind(self) -> Kind {
        match self {
            Action::Start | Action::Stop | Action::Restart | Action::Reload => Kind::UnitState,
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
            Action::Restart | Action::Reload => None,
            Action::Enable => Some(Action::Disable),
            Action::Disable => Some(Action::Enable),
            Action::Mask => Some(Action::Unmask),
            Action::Unmask => Some(Action::Mask),
            Action::LogLevel => Some(Action::LogLevel),
            Action::LogTarget => Some(Action::LogTarget),
        }
    }

    /// The state the unit is expected to be in afterwards. None for
    /// unmask, whose outcome depends on the unit's install
    /// configuration; log-control predictions come from the requested
    /// value instead.
    fn predicted(self) -> Option<&'static str> {
        match self {
            Action::Start | Action::Restart | Action::Reload => Some("active"),
            Action::Stop => Some("inactive"),
            Action::Enable => Some("enabled"),
            Action::Disable => Some("disabled"),
            Action::Mask => Some("masked"),
            Action::Unmask | Action::LogLevel | Action::LogTarget => None,
        }
    }

    /// The JSON key for this action's state dimension, used in the
    /// `current`, `predicted`, and `diff` objects.
    fn state_key(self) -> &'static str {
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
}

struct Plan {
    id: u64,
    unit: String,
    action: Action,
    /// The observed value of the action's state dimension at plan time.
    observed: String,
    /// SubState at plan time; empty for non-lifecycle actions.
    sub: String,
    /// The requested value, for actions that take one.
    value: Option<String>,
}

/// The stdio loop is single-threaded, so the Mutex is uncontended; it
/// exists to make the static Sync. The cap bounds memory against a
/// client that plans without applying; oldest plans are evicted first.
static PLANS: Mutex<Vec<Plan>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
const MAX_PLANS: usize = 32;

/// Reads the state dimension the action's precondition uses:
/// (observed value, SubState or empty).
fn observe(action: Action, unit: &str) -> Result<(String, String), BackendError> {
    match action.kind() {
        Kind::UnitState => systemd::unit_state(unit),
        Kind::FileState => Ok((systemd::unit_file_state(unit)?, String::new())),
        Kind::LogControl => Ok((
            systemd::service_log_get(action.verb(), unit)?,
            String::new(),
        )),
    }
}

/// The rollback description: the inverse action, carrying the
/// previously observed value where the inverse needs one.
fn rollback_json(action: Action, observed: &str) -> Value {
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
    let (observed, sub) = observe(action, unit)?;
    let key = action.state_key();
    let mut current = json!({ key: observed });
    if action.kind() == Kind::UnitState {
        current["sub"] = json!(sub);
    }
    // Log-control predictions are the requested value; everything else
    // predicts from the action.
    let predicted = value
        .map(Value::from)
        .unwrap_or_else(|| action.predicted().map(Value::from).unwrap_or(Value::Null));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut plans = PLANS.lock().unwrap();
    if plans.len() >= MAX_PLANS {
        plans.remove(0);
    }
    plans.push(Plan {
        id,
        unit: unit.to_string(),
        action,
        observed: observed.clone(),
        sub,
        value: value.map(String::from),
    });
    Ok(json!({
        "plan": id,
        "unit": unit,
        "action": action.name(),
        "value": value,
        "current": current,
        "predicted": { key: predicted },
        "rollback": rollback_json(action, &observed),
        "note": "nothing has been executed; apply with apply_plan",
    }))
}

pub fn apply(id: u64) -> Result<Value, BackendError> {
    let plan = {
        let mut plans = PLANS.lock().unwrap();
        let index = plans.iter().position(|p| p.id == id).ok_or_else(|| {
            BackendError(format!(
                "unknown plan {id}: plans are single-use and per-session; re-plan"
            ))
        })?;
        plans.remove(index)
    };

    let (observed_now, _) = observe(plan.action, &plan.unit)?;
    if observed_now != plan.observed {
        return Err(BackendError(format!(
            "plan {id} is stale: '{}' was {} at plan time but is {} now; re-plan",
            plan.unit, plan.observed, observed_now
        )));
    }

    let changes = systemd::apply_verb(plan.action.verb(), &plan.unit, plan.value.as_deref())?;
    let (observed_after, sub_after) = observe(plan.action, &plan.unit)?;
    let key = plan.action.state_key();
    let mut diff = json!({
        key: { "before": plan.observed, "after": observed_after },
    });
    if plan.action.kind() == Kind::UnitState {
        diff["sub"] = json!({ "before": plan.sub, "after": sub_after });
    }
    Ok(json!({
        "plan": id,
        "unit": plan.unit,
        "action": plan.action.name(),
        "applied": true,
        "diff": diff,
        // Filesystem changes as systemd reported them (symlink
        // creations and removals for enablement actions; usually empty
        // otherwise).
        "changes": changes,
        "rollback": rollback_json(plan.action, &plan.observed),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions() {
        assert_eq!(Action::parse("start"), Some(Action::Start));
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
        let err = apply(987_654_321).unwrap_err();
        assert!(err.0.contains("unknown plan"), "got: {err}");
    }
}
