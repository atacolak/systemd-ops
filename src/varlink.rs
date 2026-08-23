//! PID 1 varlink reads used by the system-manager backend.
//!
//! systemd ≥ 258 exposes `/run/systemd/io.systemd.Manager`. This module
//! calls two methods on that socket and nothing else:
//!
//! - `io.systemd.Unit.List` (streaming; `more` / `continues`)
//! - `io.systemd.Manager.Describe` (one reply)
//!
//! Wire format is the varlink protocol: one JSON object per message,
//! terminated by a single NUL byte. See https://varlink.org.
//! Interface field names come from systemd v261.2
//! (`varlink-io.systemd.Unit.c`, `varlink-io.systemd.Manager.c`).
//!
//! Every failure — missing socket, timeout, protocol error, or a reply
//! that is not the expected schema — is `Err`. `systemd.rs` then uses
//! the CLI. Connect is the probe; the result is not cached. The user
//! manager never comes here (no socket on systemd 255).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::systemd::BackendError;

const PID1: &str = "/run/systemd/io.systemd.Manager";
const BUDGET: Duration = Duration::from_secs(5);
const LIST: &str = "io.systemd.Unit.List";
const DESCRIBE: &str = "io.systemd.Manager.Describe";

/// Loaded units as `systemctl list-units --output=json` rows:
/// `unit`, `load`, `active`, `sub`, `description`.
pub fn list_units() -> Result<Vec<Value>, BackendError> {
    exchange(PID1, LIST, true)?
        .iter()
        .map(row_from_unit_list)
        .collect()
}

/// Monotonic boot timestamps in microseconds, in the order
/// `compute_boot_times` consumes: firmware, loader, initrd, userspace,
/// finish. Userspace is required. The other four default to 0 when the
/// Timestamp object is null or omitted, matching `systemctl show`.
pub fn boot_timestamps() -> Result<[u64; 5], BackendError> {
    let mut replies = exchange(PID1, DESCRIBE, false)?;
    let payload = replies
        .pop()
        .ok_or_else(|| BackendError("Describe returned no parameters".into()))?;
    boot_from_describe(&payload)
}

fn exchange(path: &str, method: &str, stream: bool) -> Result<Vec<Value>, BackendError> {
    let mut sock =
        UnixStream::connect(path).map_err(|e| BackendError(format!("connect {path}: {e}")))?;
    let _ = sock.set_read_timeout(Some(BUDGET));
    let _ = sock.set_write_timeout(Some(BUDGET));

    let mut req = Map::new();
    req.insert("method".into(), json!(method));
    req.insert("parameters".into(), json!({}));
    if stream {
        req.insert("more".into(), json!(true));
    }
    let mut bytes = serde_json::to_vec(&Value::Object(req))
        .map_err(|e| BackendError(format!("encode request: {e}")))?;
    bytes.push(0);
    sock.write_all(&bytes)
        .map_err(|e| BackendError(format!("write {path}: {e}")))?;

    let mut reader = BufReader::new(sock);
    let mut frames = Vec::new();
    loop {
        let mut frame = Vec::new();
        reader
            .read_until(0, &mut frame)
            .map_err(|e| BackendError(format!("read {path}: {e}")))?;
        let Some(0) = frame.pop() else {
            return Err(BackendError("incomplete varlink frame".into()));
        };
        let msg: Value = serde_json::from_slice(&frame)
            .map_err(|e| BackendError(format!("decode varlink frame: {e}")))?;
        if let Some(err) = msg.get("error").and_then(Value::as_str) {
            return Err(BackendError(format!("varlink error: {err}")));
        }
        let cont = msg
            .get("continues")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let params = match msg {
            Value::Object(mut o) => o.remove("parameters").unwrap_or(json!({})),
            _ => json!({}),
        };
        frames.push(params);
        if !cont {
            return Ok(frames);
        }
    }
}

/// Map one `Unit.List` `parameters` object onto the systemctl JSON row.
///
/// IDL (v261.2): output is `{ context: UnitContext, runtime: UnitRuntime }`.
/// `context.ID` is the unit name. `context.Description` is nullable;
/// systemd omits it when it would equal the id, and `systemctl
/// list-units --output=json` then prints the id. We do the same so the
/// two backends stay indistinguishable. `runtime.{Load,Active,Sub}State`
/// are nullable in the IDL; a missing required state is treated as the
/// wrong interface so the CLI answers instead of inventing a row.
fn row_from_unit_list(params: &Value) -> Result<Value, BackendError> {
    let context = params
        .get("context")
        .and_then(Value::as_object)
        .ok_or_else(|| BackendError("Unit.List parameters missing context".into()))?;
    let runtime = params
        .get("runtime")
        .and_then(Value::as_object)
        .ok_or_else(|| BackendError("Unit.List parameters missing runtime".into()))?;
    let id = context
        .get("ID")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError("Unit.List context missing ID".into()))?;
    let need = |key: &str| {
        runtime
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError(format!("Unit.List runtime missing {key}")))
    };
    let description = context
        .get("Description")
        .and_then(Value::as_str)
        .unwrap_or(id);
    Ok(json!({
        "unit": id,
        "load": need("LoadState")?,
        "active": need("ActiveState")?,
        "sub": need("SubState")?,
        "description": description,
    }))
}

fn boot_from_describe(params: &Value) -> Result<[u64; 5], BackendError> {
    let runtime = params
        .get("runtime")
        .ok_or_else(|| BackendError("Describe parameters missing runtime".into()))?;
    let usec = |name: &str| -> Option<u64> {
        runtime
            .get(name)
            .and_then(|ts| ts.get("monotonic"))
            .and_then(Value::as_u64)
    };
    let userspace = usec("UserspaceTimestamp").ok_or_else(|| {
        BackendError("Describe runtime missing UserspaceTimestamp.monotonic".into())
    })?;
    Ok([
        usec("FirmwareTimestamp").unwrap_or(0),
        usec("LoaderTimestamp").unwrap_or(0),
        usec("InitRDTimestamp").unwrap_or(0),
        userspace,
        usec("FinishTimestamp").unwrap_or(0),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn playback(frames: &'static [&'static str]) -> (PathBuf, thread::JoinHandle<Value>, Dir) {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sops-vl-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let guard = Dir(dir.clone());
        let sock = dir.join("s");
        let listener = UnixListener::bind(&sock).unwrap();
        let join = thread::spawn(move || {
            let (peer, _) = listener.accept().unwrap();
            let mut r = BufReader::new(&peer);
            let mut buf = Vec::new();
            r.read_until(0, &mut buf).unwrap();
            buf.pop();
            let seen: Value = serde_json::from_slice(&buf).unwrap();
            for frame in frames {
                (&peer).write_all(frame.as_bytes()).unwrap();
                (&peer).write_all(&[0]).unwrap();
            }
            seen
        });
        (sock, join, guard)
    }

    #[test]
    fn list_sets_more_and_drains_until_final_frame() {
        let (path, join, _dir) = playback(&[
            r#"{"parameters":{"context":{"ID":"a.service"}},"continues":true}"#,
            r#"{"parameters":{"context":{"ID":"b.service"}}}"#,
        ]);
        let out = exchange(path.to_str().unwrap(), LIST, true).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["context"]["ID"], json!("b.service"));
        let req = join.join().unwrap();
        assert_eq!(req["method"], json!(LIST));
        assert_eq!(req["more"], json!(true));
    }

    #[test]
    fn describe_does_not_set_more() {
        let (path, join, _dir) = playback(&[r#"{"parameters":{"runtime":{}}}"#]);
        let out = exchange(path.to_str().unwrap(), DESCRIBE, false).unwrap();
        assert_eq!(out.len(), 1);
        let req = join.join().unwrap();
        assert!(req.get("more").is_none());
        assert_eq!(req["method"], json!(DESCRIBE));
    }

    #[test]
    fn describe_reads_monotonic_usec_and_null_phases() {
        let payload = json!({
            "context": {},
            "runtime": {
                "FirmwareTimestamp": { "realtime": 1, "monotonic": 5_000_000 },
                "LoaderTimestamp": { "realtime": 2, "monotonic": 2_000_000 },
                "InitRDTimestamp": null,
                "UserspaceTimestamp": { "realtime": 3, "monotonic": 4_000_000 },
                "FinishTimestamp": { "realtime": 4, "monotonic": 10_000_000 }
            }
        });
        assert_eq!(
            boot_from_describe(&payload).unwrap(),
            [5_000_000, 2_000_000, 0, 4_000_000, 10_000_000]
        );
        assert!(boot_from_describe(&json!({ "runtime": {} })).is_err());
    }

    #[test]
    fn method_errors_and_absent_socket_fail() {
        let (path, _join, _dir) = playback(&[r#"{"error":"org.varlink.service.MethodNotFound"}"#]);
        let err = exchange(path.to_str().unwrap(), LIST, true).unwrap_err();
        assert!(err.0.contains("MethodNotFound"), "got {err}");
        assert!(exchange("/no/such/socket", LIST, true).is_err());
    }

    #[test]
    fn unit_row_matches_systemctl_json_keys() {
        let full = json!({
            "context": { "ID": "ssh.service", "Description": "OpenBSD Secure Shell server" },
            "runtime": { "LoadState": "loaded", "ActiveState": "active", "SubState": "running" }
        });
        assert_eq!(
            row_from_unit_list(&full).unwrap(),
            json!({
                "unit": "ssh.service",
                "load": "loaded",
                "active": "active",
                "sub": "running",
                "description": "OpenBSD Secure Shell server"
            })
        );
        let no_desc = json!({
            "context": { "ID": "x.service" },
            "runtime": { "LoadState": "loaded", "ActiveState": "inactive", "SubState": "dead" }
        });
        assert_eq!(
            row_from_unit_list(&no_desc).unwrap()["description"],
            json!("x.service")
        );
        assert!(row_from_unit_list(&json!({ "context": { "ID": "x.service" } })).is_err());
    }
}
