//! Sealed plan tokens.
//!
//! Integrity: HMAC-SHA256 over canonical JSON. Token form `v1.<payload>.<mac>`.
//! Payload is base64url JSON including the systemd manager the plan was
//! made against. Short expiry + stale/precondition checks. No nonce
//! ledger: a fully stateless token cannot honestly be one-time.

use serde_json::{json, Value};

use crate::config::{self, OpsConfig};
use crate::sha256::{hex, hmac_sha256};
use crate::systemd::BackendError;

const TOKEN_VERSION: &str = "v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanClass {
    Control,
    Author,
    Automation,
}

impl PlanClass {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanClass::Control => "control",
            PlanClass::Author => "author",
            PlanClass::Automation => "automation",
        }
    }

    pub fn parse(s: &str) -> Result<Self, BackendError> {
        match s {
            "control" => Ok(PlanClass::Control),
            "author" => Ok(PlanClass::Author),
            "automation" => Ok(PlanClass::Automation),
            other => Err(BackendError(format!("unknown plan class '{other}'"))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SealedPlan {
    pub class: PlanClass,
    pub manager: crate::systemd::Manager,
    pub unit: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub origin_cwd: Option<String>,
    pub payload: Value,
}

pub fn issue(cfg: &OpsConfig, plan: &SealedPlan) -> Result<String, BackendError> {
    let body = json!({
        "class": plan.class.as_str(),
        "manager": plan.manager.as_str(),
        "unit": plan.unit,
        "issued_at": plan.issued_at,
        "expires_at": plan.expires_at,
        "origin_cwd": plan.origin_cwd,
        "payload": plan.payload,
    });
    let raw = body.to_string();
    let mac = hmac_sha256(&cfg.hmac_key()?, raw.as_bytes());
    Ok(format!(
        "{TOKEN_VERSION}.{}.{}",
        b64url(raw.as_bytes()),
        hex(&mac)
    ))
}

pub fn mint(
    cfg: &OpsConfig,
    class: PlanClass,
    unit: &str,
    origin_cwd: Option<String>,
    payload: Value,
) -> Result<(String, SealedPlan), BackendError> {
    let now = config::now_unix();
    let plan = SealedPlan {
        class,
        manager: cfg.manager,
        unit: unit.to_string(),
        issued_at: now,
        expires_at: now + cfg.plan_ttl_secs,
        origin_cwd,
        payload,
    };
    let token = issue(cfg, &plan)?;
    Ok((token, plan))
}

pub fn parse(cfg: &OpsConfig, token: &str) -> Result<SealedPlan, BackendError> {
    let mut parts = token.split('.');
    let ver = parts
        .next()
        .ok_or_else(|| BackendError("invalid plan token".into()))?;
    let payload_b64 = parts
        .next()
        .ok_or_else(|| BackendError("invalid plan token".into()))?;
    let mac_hex = parts
        .next()
        .ok_or_else(|| BackendError("invalid plan token".into()))?;
    if parts.next().is_some() || ver != TOKEN_VERSION {
        return Err(BackendError("invalid plan token".into()));
    }
    let raw = b64url_decode(payload_b64)?;
    let expected = hmac_sha256(&cfg.hmac_key()?, &raw);
    let got = decode_hex(mac_hex)?;
    if !ct_eq(&expected, &got) {
        return Err(BackendError("invalid plan token: bad mac".into()));
    }
    let body: Value =
        serde_json::from_slice(&raw).map_err(|_| BackendError("invalid plan token".into()))?;
    let class = PlanClass::parse(body.get("class").and_then(Value::as_str).unwrap_or(""))?;
    let manager =
        crate::systemd::Manager::parse(body.get("manager").and_then(Value::as_str).unwrap_or(""))
            .ok_or_else(|| BackendError("invalid plan token".into()))?;
    if manager != cfg.manager {
        return Err(BackendError(format!(
            "plan token is for the {} manager; current manager is {}",
            manager.as_str(),
            cfg.manager.as_str()
        )));
    }
    let expires_at = body
        .get("expires_at")
        .and_then(Value::as_u64)
        .ok_or_else(|| BackendError("invalid plan token".into()))?;
    if config::now_unix() > expires_at {
        return Err(BackendError(format!(
            "plan token expired at {}",
            config::unix_to_rfc3339(expires_at)
        )));
    }
    let unit = body
        .get("unit")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("invalid plan token".into()))?
        .to_string();
    Ok(SealedPlan {
        class,
        manager,
        unit,
        issued_at: body.get("issued_at").and_then(Value::as_u64).unwrap_or(0),
        expires_at,
        origin_cwd: body
            .get("origin_cwd")
            .and_then(Value::as_str)
            .map(str::to_string),
        payload: body.get("payload").cloned().unwrap_or(Value::Null),
    })
}

pub fn require_class(plan: &SealedPlan, expect: PlanClass) -> Result<(), BackendError> {
    if plan.class == expect {
        Ok(())
    } else {
        Err(BackendError(format!(
            "plan class is {}; {} cannot apply it",
            plan.class.as_str(),
            expect.as_str()
        )))
    }
}

fn b64url(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
        let b2 = if i + 2 < bytes.len() { bytes[i + 2] } else { 0 };
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if i + 2 < bytes.len() {
            out.push(T[(b2 & 0x3f) as usize] as char);
        }
        i += 3;
    }
    out
}

fn b64url_decode(s: &str) -> Result<Vec<u8>, BackendError> {
    fn val(c: u8) -> Result<u8, BackendError> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'-' => Ok(62),
            b'_' => Ok(63),
            _ => Err(BackendError("invalid plan token".into())),
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let v0 = val(bytes[i])?;
        let v1 = if i + 1 < bytes.len() {
            val(bytes[i + 1])?
        } else {
            0
        };
        out.push((v0 << 2) | (v1 >> 4));
        if i + 2 < bytes.len() {
            let v2 = val(bytes[i + 2])?;
            out.push((v1 << 4) | (v2 >> 2));
            if i + 3 < bytes.len() {
                let v3 = val(bytes[i + 3])?;
                out.push((v2 << 6) | v3);
            }
        }
        i += 4;
    }
    Ok(out)
}

fn decode_hex(s: &str) -> Result<Vec<u8>, BackendError> {
    if s.len() % 2 != 0 {
        return Err(BackendError("invalid plan token".into()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let nibble = |c: u8| -> Result<u8, BackendError> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(BackendError("invalid plan token".into())),
        }
    };
    let mut i = 0;
    while i < b.len() {
        out.push((nibble(b[i])? << 4) | nibble(b[i + 1])?);
        i += 2;
    }
    Ok(out)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for i in 0..a.len() {
        acc |= a[i] ^ b[i];
    }
    acc == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OpsConfig;
    use crate::systemd::Manager;

    #[test]
    fn b64_roundtrip() {
        let s = b"{\"class\":\"control\"}";
        let enc = b64url(s);
        assert_eq!(b64url_decode(&enc).unwrap(), s);
    }

    #[test]
    fn mint_parse_and_reject_tamper() {
        let dir = std::env::temp_dir().join(format!("systemd-ops-token-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg = OpsConfig {
            manager: Manager::User,
            write_prefix: Some("managed-*".into()),
            plan_ttl_secs: 600,
            state_dir: dir.clone(),
        };
        let (token, _) = mint(
            &cfg,
            PlanClass::Control,
            "managed-x.service",
            None,
            json!({"action":"start"}),
        )
        .unwrap();
        let parsed = parse(&cfg, &token).unwrap();
        assert_eq!(parsed.class, PlanClass::Control);
        assert_eq!(parsed.unit, "managed-x.service");
        assert_eq!(parsed.manager, Manager::User);
        let mut bad = token.clone();
        let last = bad.pop().unwrap();
        bad.push(if last == 'a' { 'b' } else { 'a' });
        let err = parse(&cfg, &bad).unwrap_err();
        assert!(err.0.contains("bad mac") || err.0.contains("invalid"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn cfg(manager: Manager, dir: &std::path::Path) -> OpsConfig {
        OpsConfig {
            manager,
            write_prefix: Some("managed-*".into()),
            plan_ttl_secs: 600,
            state_dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn user_token_rejected_on_system_manager() {
        let dir =
            std::env::temp_dir().join(format!("systemd-ops-token-user-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let user = cfg(Manager::User, &dir);
        let system = cfg(Manager::System, &dir);
        let (token, _) = mint(
            &user,
            PlanClass::Control,
            "managed-x.service",
            None,
            json!({"action":"start"}),
        )
        .unwrap();
        let err = parse(&system, &token).unwrap_err();
        assert!(
            err.0.contains("user") && err.0.contains("system"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_token_rejected_on_user_manager() {
        let dir =
            std::env::temp_dir().join(format!("systemd-ops-token-sys-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let user = cfg(Manager::User, &dir);
        let system = cfg(Manager::System, &dir);
        let (token, _) = mint(
            &system,
            PlanClass::Control,
            "managed-x.service",
            None,
            json!({"action":"start"}),
        )
        .unwrap();
        let err = parse(&user, &token).unwrap_err();
        assert!(
            err.0.contains("user") && err.0.contains("system"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
