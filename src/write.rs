//! The write path. Nothing mutates without a plan.
//!
//! A change is made in two steps. `plan` reads the unit's current state
//! and returns it with the predicted state, the rollback action, and a
//! plan id — executing nothing. `apply` takes the id, re-reads the
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
}

/// Which state dimension an action changes, and therefore which
/// property the plan records and the apply precondition re-checks:
/// `ActiveState` for lifecycle actions, `UnitFileState` for enablement.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    UnitState,
    FileState,
}

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
            _ => None,
        }
    }

    /// Also the systemctl verb — the names are systemd's own.
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
        }
    }

    fn kind(self) -> Kind {
        match self {
            Action::Start | Action::Stop | Action::Restart | Action::Reload => Kind::UnitState,
            Action::Enable | Action::Disable | Action::Mask | Action::Unmask => Kind::FileState,
        }
    }

    /// The action that undoes this one. Restart and reload have no
    /// inverse; the reply reports null for them.
    fn inverse(self) -> Option<Action> {
        match self {
            Action::Start => Some(Action::Stop),
            Action::Stop => Some(Action::Start),
            Action::Restart | Action::Reload => None,
            Action::Enable => Some(Action::Disable),
            Action::Disable => Some(Action::Enable),
            Action::Mask => Some(Action::Unmask),
            Action::Unmask => Some(Action::Mask),
        }
    }

    /// The state the unit is expected to be in afterwards. None for
    /// unmask: the resulting enablement state depends on the unit's
    /// install configuration and cannot be predicted from the action.
    fn predicted(self) -> Option<&'static str> {
        match self {
            Action::Start | Action::Restart | Action::Reload => Some("active"),
            Action::Stop => Some("inactive"),
            Action::Enable => Some("enabled"),
            Action::Disable => Some("disabled"),
            Action::Mask => Some("masked"),
            Action::Unmask => None,
        }
    }

    /// The JSON key for this action's state dimension, used in the
    /// `current`, `predicted`, and `diff` objects.
    fn state_key(self) -> &'static str {
        match self.kind() {
            Kind::UnitState => "active",
            Kind::FileState => "unit_file_state",
        }
    }
}

struct Plan {
    id: u64,
    unit: String,
    action: Action,
    /// The observed value of the action's state dimension at plan time.
    observed: String,
    /// SubState at plan time; empty for enablement actions.
    sub: String,
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
    }
}

pub fn plan(action: Action, unit: &str) -> Result<Value, BackendError> {
    systemd::validate_unit_name(unit)?;
    let (observed, sub) = observe(action, unit)?;
    let key = action.state_key();
    let mut current = json!({ key: observed });
    if action.kind() == Kind::UnitState {
        current["sub"] = json!(sub);
    }
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
    });
    Ok(json!({
        "plan": id,
        "unit": unit,
        "action": action.verb(),
        "current": current,
        "predicted": { key: action.predicted() },
        "rollback": action.inverse().map(Action::verb),
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

    let changes = systemd::apply_verb(plan.action.verb(), &plan.unit)?;
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
        "action": plan.action.verb(),
        "applied": true,
        "diff": diff,
        // Filesystem changes as systemd reported them (symlink
        // creations and removals for enablement actions; usually empty
        // for lifecycle actions).
        "changes": changes,
        "rollback": plan.action.inverse().map(Action::verb),
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
    fn unknown_plans_are_refused() {
        let err = apply(987_654_321).unwrap_err();
        assert!(err.0.contains("unknown plan"), "got: {err}");
    }
}
