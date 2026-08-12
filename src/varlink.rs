//! A minimal varlink client.
//!
//! Varlink is NUL-terminated JSON over a Unix socket; `UnixStream` plus
//! the serde_json already in the tree covers it, so no varlink crate is
//! used.
//!
//! systemd ≥ 258 serves PID 1's API at `/run/systemd/io.systemd.Manager`
//! (one socket, several interfaces; unit listing is `io.systemd.Unit.List`,
//! verified against the v261.2 interface definitions in
//! `src/shared/varlink-io.systemd.Unit.c`). Any failure (no socket,
//! older systemd, an error reply, an unfamiliar reply shape) is an
//! `Err`, and the caller falls back to the CLI. The probe is the
//! `connect()` itself; a failed connect to a missing path is cheap
//! enough that the result is not cached.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{json, Value};

use crate::systemd::BackendError;

const MANAGER_SOCKET: &str = "/run/systemd/io.systemd.Manager";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Units via `io.systemd.Unit.List`, normalized to the exact shape
/// `systemctl list-units --output=json` emits, so callers cannot tell
/// the backends apart.
pub fn list_units() -> Result<Vec<Value>, BackendError> {
    call(MANAGER_SOCKET, "io.systemd.Unit.List", true)?
        .iter()
        .map(normalize_unit)
        .collect()
}

/// Boot timestamps via `io.systemd.Manager.Describe` (a plain method, no
/// streaming): the same five monotonic values `systemctl show` serves,
/// in the [firmware, loader, initrd, userspace, finish] order
/// `compute_boot_times` expects. Absent or null phases map to 0, matching
/// the CLI's semantics; a reply without `UserspaceTimestamp` at all is
/// not the interface we expect and refuses, so the CLI answers.
pub fn boot_timestamps() -> Result<[u64; 5], BackendError> {
    let replies = call(MANAGER_SOCKET, "io.systemd.Manager.Describe", false)?;
    extract_boot_timestamps(
        replies
            .first()
            .ok_or_else(|| BackendError("empty varlink Describe reply".into()))?,
    )
}

fn extract_boot_timestamps(reply: &Value) -> Result<[u64; 5], BackendError> {
    let runtime = reply
        .get("runtime")
        .ok_or_else(|| BackendError("varlink Describe reply has no runtime".into()))?;
    let monotonic = |key: &str| {
        runtime
            .get(key)
            .and_then(|t| t.get("monotonic"))
            .and_then(Value::as_u64)
    };
    let userspace = monotonic("UserspaceTimestamp")
        .ok_or_else(|| BackendError("varlink Describe reply has no UserspaceTimestamp".into()))?;
    Ok([
        monotonic("FirmwareTimestamp").unwrap_or(0),
        monotonic("LoaderTimestamp").unwrap_or(0),
        monotonic("InitRDTimestamp").unwrap_or(0),
        userspace,
        monotonic("FinishTimestamp").unwrap_or(0),
    ])
}

/// One method call. With `more`, expect systemd's List-style streaming
/// (one message per entry, `continues` on all but the last); without it,
/// a single reply. Returns each reply's `parameters`, in order.
fn call(socket_path: &str, method: &str, more: bool) -> Result<Vec<Value>, BackendError> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| BackendError(format!("connect {socket_path}: {e}")))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let mut message = json!({ "method": method, "parameters": {} });
    if more {
        message["more"] = json!(true);
    }
    let mut request = serde_json::to_vec(&message).expect("static request serializes");
    request.push(0);
    stream
        .write_all(&request)
        .map_err(|e| BackendError(format!("write to {socket_path}: {e}")))?;

    let mut reader = BufReader::new(&stream);
    let mut replies = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        reader
            .read_until(0, &mut buf)
            .map_err(|e| BackendError(format!("read from {socket_path}: {e}")))?;
        if buf.pop() != Some(0) {
            return Err(BackendError("truncated varlink reply".into()));
        }
        let mut reply: Value = serde_json::from_slice(&buf)
            .map_err(|e| BackendError(format!("bad varlink JSON: {e}")))?;
        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Err(BackendError(format!("varlink error: {error}")));
        }
        let continues = reply
            .get("continues")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // Move the per-unit payload out rather than deep-cloning it;
        // this path runs once per unit on every list call.
        replies.push(
            reply
                .get_mut("parameters")
                .map(Value::take)
                .unwrap_or(json!({})),
        );
        if !continues {
            return Ok(replies);
        }
    }
}

/// `io.systemd.Unit.List` streams one `{context, runtime}` pair per unit,
/// PascalCase fields; systemctl's JSON speaks short names (`active`). One
/// shape goes out either way. A missing required field means the interface
/// isn't what we expect, so refuse and let the CLI answer rather than emit
/// half a unit.
fn normalize_unit(entry: &Value) -> Result<Value, BackendError> {
    let field = |section: &str, key: &str| {
        entry
            .get(section)
            .and_then(|s| s.get(key))
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError(format!("varlink unit entry missing '{section}.{key}'")))
    };
    let id = field("context", "ID")?;
    Ok(json!({
        "unit": id,
        "load": field("runtime", "LoadState")?,
        "active": field("runtime", "ActiveState")?,
        "sub": field("runtime", "SubState")?,
        // systemd omits Description when it equals the unit id, and
        // systemctl fills the id back in. Falling back to "" instead
        // made the two backends distinguishable, which is the one
        // thing this module promises they are not.
        "description": field("context", "Description").unwrap_or(id),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// The socket directory, removed when the test that made it ends,
    /// including when it ends by panicking. Without this every `cargo
    /// test` left three directories in /tmp forever; 173 of them had
    /// accumulated before anyone looked.
    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A one-shot fake varlink server: accepts one connection, reads the
    /// request, plays back canned replies. Lives in /tmp because AF_UNIX
    /// paths are capped at ~108 bytes.
    fn serve(
        replies: &'static [&'static str],
    ) -> (std::path::PathBuf, std::thread::JoinHandle<Value>, TempDir) {
        // A counter, not the address of `replies`: `{:p}` on a slice
        // reference formats the whole fat pointer, which put
        // `Pointer { addr: 0x..., metadata: 1 }`, spaces and braces
        // included, into the path.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mcpd-vl-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let guard = TempDir(dir.clone());
        let path = dir.join("sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut buf = Vec::new();
            reader.read_until(0, &mut buf).unwrap();
            buf.pop();
            let request: Value = serde_json::from_slice(&buf).unwrap();
            for reply in replies {
                (&stream).write_all(reply.as_bytes()).unwrap();
                (&stream).write_all(&[0]).unwrap();
            }
            request
        });
        (path, handle, guard)
    }

    #[test]
    fn streams_until_continues_stops() {
        let (path, server, _dir) = serve(&[
            r#"{"parameters":{"context":{"ID":"a.service"}},"continues":true}"#,
            r#"{"parameters":{"context":{"ID":"b.service"}}}"#,
        ]);
        let replies = call(path.to_str().unwrap(), "io.systemd.Unit.List", true).unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1]["context"]["ID"], json!("b.service"));
        // The request asked for streaming, as systemd's List methods require.
        let request = server.join().unwrap();
        assert_eq!(request["more"], json!(true));
        assert_eq!(request["method"], json!("io.systemd.Unit.List"));
    }

    #[test]
    fn plain_methods_omit_more() {
        let (path, server, _dir) = serve(&[r#"{"parameters":{"runtime":{}}}"#]);
        let replies = call(path.to_str().unwrap(), "io.systemd.Manager.Describe", false).unwrap();
        assert_eq!(replies.len(), 1);
        // Describe is a plain method: the request must not ask to stream.
        let request = server.join().unwrap();
        assert!(request.get("more").is_none());
    }

    #[test]
    fn boot_timestamp_extraction() {
        // Shape per v261.2: runtime carries dual-clock Timestamp objects.
        let reply = json!({
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
            extract_boot_timestamps(&reply).unwrap(),
            [5_000_000, 2_000_000, 0, 4_000_000, 10_000_000]
        );
        // No UserspaceTimestamp at all → not our interface → refuse.
        assert!(extract_boot_timestamps(&json!({ "runtime": {} })).is_err());
    }

    #[test]
    fn error_replies_and_dead_sockets_are_errors() {
        let (path, _server, _dir) = serve(&[r#"{"error":"org.varlink.service.MethodNotFound"}"#]);
        let err = call(path.to_str().unwrap(), "io.systemd.Unit.List", true).unwrap_err();
        assert!(err.0.contains("MethodNotFound"), "got: {err}");
        // No socket at all, the everyday case on systemd < 258.
        assert!(call("/no/such/socket", "x", true).is_err());
    }

    #[test]
    fn normalization_is_strict() {
        // Reply shape per v261.2 varlink-io.systemd.Unit.c: one
        // {context, runtime} pair per unit, PascalCase fields.
        let full = json!({
            "context": { "ID": "ssh.service", "Description": "OpenBSD Secure Shell server" },
            "runtime": { "LoadState": "loaded", "ActiveState": "active", "SubState": "running" }
        });
        assert_eq!(
            normalize_unit(&full).unwrap(),
            json!({
                "unit": "ssh.service", "load": "loaded", "active": "active",
                "sub": "running", "description": "OpenBSD Secure Shell server"
            })
        );
        // Description is nullable in the IDL; absent maps to "".
        let no_desc = json!({
            "context": { "ID": "x.service" },
            "runtime": { "LoadState": "loaded", "ActiveState": "inactive", "SubState": "dead" }
        });
        // Absent Description means "same as the id", which is what
        // systemctl reports and therefore what this must report.
        assert_eq!(
            normalize_unit(&no_desc).unwrap()["description"],
            json!("x.service")
        );
        // Unfamiliar shape → refuse, so the caller falls back to the CLI.
        assert!(normalize_unit(&json!({ "context": { "ID": "x.service" } })).is_err());
    }
}
