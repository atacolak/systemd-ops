//! `--json` envelope. stdout is always one JSON object.

use serde_json::{json, Value};

use crate::systemd::BackendError;

pub const SCHEMA_VERSION: u32 = 1;

pub fn ok(data: Value) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "ok": true,
        "data": data,
    })
}

pub fn err(code: &str, message: &str, details: Value) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "ok": false,
        "error": {
            "code": code,
            "message": message,
            "details": details,
        }
    })
}

pub fn from_backend(error: &BackendError) -> Value {
    err(classify(&error.0), &error.0, Value::Null)
}

fn classify(message: &str) -> &'static str {
    let m = message.to_ascii_lowercase();
    if m.contains("stale") {
        "stale_plan"
    } else if m.contains("expired") {
        "expired_plan"
    } else if m.contains("for the") && m.contains("manager") {
        "manager_mismatch"
    } else if m.contains("tamper") || m.contains("invalid plan token") || m.contains("bad mac") {
        "invalid_token"
    } else if m.contains("not mcp-authored")
        || m.contains("not systemd-ops-authored")
        || m.contains("not authored")
    {
        "not_authored"
    } else if m.contains("restricted") || m.contains("must match") || m.contains("refused") {
        "forbidden"
    } else if m.contains("no such") {
        "not_found"
    } else if m.contains("unknown") || m.contains("missing") {
        "invalid_argument"
    } else {
        "error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_ok() {
        let v = ok(json!({"x": 1}));
        assert_eq!(v["schema_version"], json!(1));
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["data"]["x"], json!(1));
    }

    #[test]
    fn classify_manager_mismatch() {
        assert_eq!(
            classify("plan token is for the user manager; current manager is system"),
            "manager_mismatch"
        );
    }
}
