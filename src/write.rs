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
}

impl Action {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "start" => Some(Action::Start),
            "stop" => Some(Action::Stop),
            "restart" => Some(Action::Restart),
            "reload" => Some(Action::Reload),
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
        }
    }

    /// The action that undoes this one. Restart and reload have no
    /// inverse; the reply reports null for them.
    fn inverse(self) -> Option<Action> {
        match self {
            Action::Start => Some(Action::Stop),
            Action::Stop => Some(Action::Start),
            Action::Restart | Action::Reload => None,
        }
    }

    /// The active state the unit is expected to be in afterwards.
    fn predicted(self) -> &'static str {
        match self {
            Action::Start | Action::Restart | Action::Reload => "active",
            Action::Stop => "inactive",
        }
    }
}

struct Plan {
    id: u64,
    unit: String,
    action: Action,
    active: String,
    sub: String,
}

/// The stdio loop is single-threaded, so the Mutex is uncontended; it
/// exists to make the static Sync. The cap bounds memory against a
/// client that plans without applying; oldest plans are evicted first.
static PLANS: Mutex<Vec<Plan>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
const MAX_PLANS: usize = 32;

pub fn plan(action: Action, unit: &str) -> Result<Value, BackendError> {
    systemd::validate_unit_name(unit)?;
    let (active, sub) = systemd::unit_state(unit)?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut plans = PLANS.lock().unwrap();
    if plans.len() >= MAX_PLANS {
        plans.remove(0);
    }
    plans.push(Plan {
        id,
        unit: unit.to_string(),
        action,
        active: active.clone(),
        sub: sub.clone(),
    });
    Ok(json!({
        "plan": id,
        "unit": unit,
        "action": action.verb(),
        "current": { "active": active, "sub": sub },
        "predicted": { "active": action.predicted() },
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

    let (active_now, _) = systemd::unit_state(&plan.unit)?;
    if active_now != plan.active {
        return Err(BackendError(format!(
            "plan {id} is stale: '{}' was {} at plan time but is {} now; re-plan",
            plan.unit, plan.active, active_now
        )));
    }

    systemd::apply_verb(plan.action.verb(), &plan.unit)?;
    let (active_after, sub_after) = systemd::unit_state(&plan.unit)?;
    Ok(json!({
        "plan": id,
        "unit": plan.unit,
        "action": plan.action.verb(),
        "applied": true,
        "diff": {
            "active": { "before": plan.active, "after": active_after },
            "sub": { "before": plan.sub, "after": sub_after },
        },
        "rollback": plan.action.inverse().map(Action::verb),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions() {
        assert_eq!(Action::parse("start"), Some(Action::Start));
        assert_eq!(Action::parse("enable"), None);
        assert_eq!(Action::Start.inverse(), Some(Action::Stop));
        assert_eq!(Action::Stop.inverse(), Some(Action::Start));
        assert_eq!(Action::Restart.inverse(), None);
        assert_eq!(Action::Reload.inverse(), None);
        assert_eq!(Action::Stop.predicted(), "inactive");
        assert_eq!(Action::Restart.predicted(), "active");
    }

    #[test]
    fn unknown_plans_are_refused() {
        let err = apply(987_654_321).unwrap_err();
        assert!(err.0.contains("unknown plan"), "got: {err}");
    }
}
