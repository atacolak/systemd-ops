//! Local configuration for systemd-ops.
//!
//! No write-prefix means reads still work and writes are refused.
//! Prefixes are never hardcoded to an operator namespace.

use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::systemd::{self, BackendError, Manager};

const KEY_BYTES: usize = 32;
const DEFAULT_PLAN_TTL_SECS: u64 = 600;

thread_local! {
    static CURRENT: RefCell<Option<OpsConfig>> = const { RefCell::new(None) };
}

pub fn set_current(cfg: OpsConfig) {
    CURRENT.with(|c| *c.borrow_mut() = Some(cfg));
}

pub fn current() -> Option<OpsConfig> {
    CURRENT.with(|c| c.borrow().clone())
}

pub fn current_or_load() -> Result<OpsConfig, BackendError> {
    if let Some(cfg) = current() {
        return Ok(cfg);
    }
    let cfg = OpsConfig::load(None, None, None)?;
    cfg.apply();
    Ok(cfg)
}

#[derive(Clone, Debug)]
pub struct OpsConfig {
    pub manager: Manager,
    pub write_prefix: Option<String>,
    pub plan_ttl_secs: u64,
    pub state_dir: PathBuf,
}

impl OpsConfig {
    pub fn load(
        manager: Option<Manager>,
        write_prefix: Option<String>,
        config_path: Option<&Path>,
    ) -> Result<Self, BackendError> {
        Self::load_with_default(manager, write_prefix, config_path, Manager::User)
    }

    pub fn load_with_default(
        manager: Option<Manager>,
        write_prefix: Option<String>,
        config_path: Option<&Path>,
        default_manager: Manager,
    ) -> Result<Self, BackendError> {
        let state_dir = state_dir();
        let file = load_file(config_path)?;
        let manager = manager
            .or_else(|| env_manager())
            .or(file.manager)
            .unwrap_or(default_manager);
        let write_prefix = write_prefix.or_else(|| env_prefix()).or(file.write_prefix);
        if let Some(spec) = &write_prefix {
            systemd::parse_write_prefix(spec).map_err(BackendError)?;
        }
        let plan_ttl_secs = env_u64("SYSTEMD_OPS_PLAN_TTL_SECS")
            .or(file.plan_ttl_secs)
            .unwrap_or(DEFAULT_PLAN_TTL_SECS)
            .clamp(30, 86_400);
        Ok(OpsConfig {
            manager,
            write_prefix,
            plan_ttl_secs,
            state_dir,
        })
    }

    pub fn apply(&self) {
        systemd::set_manager(self.manager);
        systemd::set_write_prefix(self.write_prefix.clone());
        set_current(self.clone());
    }

    pub fn hmac_key(&self) -> Result<[u8; KEY_BYTES], BackendError> {
        load_or_create_key(&self.state_dir.join("hmac.key"))
    }
}

#[derive(Default)]
struct FileConfig {
    manager: Option<Manager>,
    write_prefix: Option<String>,
    plan_ttl_secs: Option<u64>,
}

fn xdg_config_home() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        PathBuf::from("/tmp")
    }
}

pub fn config_dir() -> PathBuf {
    xdg_config_home().join("systemd-ops")
}

pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SYSTEMD_OPS_STATE_DIR") {
        let p = PathBuf::from(dir.trim());
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(xdg).join("systemd-ops")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/state/systemd-ops")
    } else {
        PathBuf::from("/tmp/systemd-ops")
    }
}

fn default_config_path() -> PathBuf {
    config_dir().join("config.toml")
}

fn env_prefix() -> Option<String> {
    std::env::var("SYSTEMD_OPS_WRITE_PREFIX")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn env_manager() -> Option<Manager> {
    std::env::var("SYSTEMD_OPS_MANAGER")
        .ok()
        .and_then(|s| Manager::parse(s.trim()))
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn load_file(explicit: Option<&Path>) -> Result<FileConfig, BackendError> {
    let path = explicit
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| BackendError(format!("cannot read {}: {e}", path.display())))?;
    Ok(parse_toml(&text))
}

fn parse_toml(text: &str) -> FileConfig {
    let mut out = FileConfig::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let val = unquote(v.trim());
        match key {
            "write_prefix" | "write-prefix" => {
                if !val.is_empty() {
                    out.write_prefix = Some(val);
                }
            }
            "manager" => out.manager = Manager::parse(&val),
            "plan_ttl_secs" | "plan-ttl-secs" => {
                out.plan_ttl_secs = val.parse().ok();
            }
            _ => {}
        }
    }
    out
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

fn load_or_create_key(path: &Path) -> Result<[u8; KEY_BYTES], BackendError> {
    if path.exists() {
        let bytes = fs::read(path)
            .map_err(|e| BackendError(format!("cannot read {}: {e}", path.display())))?;
        if bytes.len() != KEY_BYTES {
            return Err(BackendError(format!(
                "{} must be {KEY_BYTES} bytes",
                path.display()
            )));
        }
        let mut key = [0u8; KEY_BYTES];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| BackendError(format!("cannot create {}: {e}", parent.display())))?;
    }
    let mut key = [0u8; KEY_BYTES];
    fill_random(&mut key)?;
    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| BackendError(format!("cannot write {}: {e}", path.display())))?;
    f.write_all(&key)
        .map_err(|e| BackendError(format!("cannot write {}: {e}", path.display())))?;
    Ok(key)
}

fn fill_random(buf: &mut [u8]) -> Result<(), BackendError> {
    let mut f = fs::File::open("/dev/urandom")
        .map_err(|e| BackendError(format!("cannot open /dev/urandom: {e}")))?;
    use std::io::Read;
    f.read_exact(buf)
        .map_err(|e| BackendError(format!("cannot read /dev/urandom: {e}")))?;
    Ok(())
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn unix_to_rfc3339(secs: u64) -> String {
    systemd::usec_to_rfc3339(secs.saturating_mul(1_000_000))
}

#[cfg(unix)]
trait OpenMode {
    fn mode(&mut self, mode: u32) -> &mut Self;
}

#[cfg(unix)]
impl OpenMode for fs::OpenOptions {
    fn mode(&mut self, mode: u32) -> &mut Self {
        std::os::unix::fs::OpenOptionsExt::mode(self, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_toml() {
        let cfg =
            parse_toml("write_prefix = \"managed-*\"\nmanager = \"user\"\nplan_ttl_secs = 120\n");
        assert_eq!(cfg.write_prefix.as_deref(), Some("managed-*"));
        assert_eq!(cfg.manager, Some(Manager::User));
        assert_eq!(cfg.plan_ttl_secs, Some(120));
    }
}
