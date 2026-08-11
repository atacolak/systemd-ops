//! A varlink client in the fewest lines that are correct.
//!
//! Varlink is NUL-terminated JSON over a Unix socket — `UnixStream` plus the
//! serde_json we already ship covers it; a varlink crate would be a third
//! dependency for a protocol this file fits in.
//!
//! systemd ≥ 258 serves PID 1's API at `/run/systemd/io.systemd.Manager`
//! (one socket, several interfaces; unit listing is `io.systemd.Unit.List`,
//! verified against the v261.2 interface definitions in
//! `src/shared/varlink-io.systemd.Unit.c`). We speak to it when it's there
//! and say nothing when it isn't: any surprise at all — no socket, old
//! systemd, unfamiliar reply shape — is an `Err`, and the caller falls back
//! to the CLI. The probe is the `connect()` itself; a failed connect to a
//! missing path costs nothing worth caching.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{json, Value};

use crate::systemd::BackendError;

const MANAGER_SOCKET: &str = "/run/systemd/io.systemd.Manager";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Units via `io.systemd.Unit.List`, normalized to the exact shape
/// `systemctl list-units --output=json` emits — callers cannot tell the
/// backends apart, which is the point.
pub fn list_units() -> Result<Vec<Value>, BackendError> {
    call(MANAGER_SOCKET, "io.systemd.Unit.List")?
        .iter()
        .map(normalize_unit)
        .collect()
}

/// One streamed method call ("more" mode, which is how systemd's List*
/// methods reply: one message per entry, `continues` on all but the last).
/// Returns each reply's `parameters`, in order.
fn call(socket_path: &str, method: &str) -> Result<Vec<Value>, BackendError> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| BackendError(format!("connect {socket_path}: {e}")))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let mut request =
        serde_json::to_vec(&json!({ "method": method, "parameters": {}, "more": true }))
            .expect("static request serializes");
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
        // Move the per-unit payload out rather than deep-cloning it — this
        // is the hot path that exists to be the cheap backend.
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
/// isn't what we expect — refuse and let the CLI answer, rather than emit
/// half a unit.
fn normalize_unit(entry: &Value) -> Result<Value, BackendError> {
    let field = |section: &str, key: &str| {
        entry
            .get(section)
            .and_then(|s| s.get(key))
            .and_then(Value::as_str)
            .ok_or_else(|| BackendError(format!("varlink unit entry missing '{section}.{key}'")))
    };
    Ok(json!({
        "unit": field("context", "ID")?,
        "load": field("runtime", "LoadState")?,
        "active": field("runtime", "ActiveState")?,
        "sub": field("runtime", "SubState")?,
        "description": field("context", "Description").unwrap_or(""),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A one-shot fake varlink server: accepts one connection, reads the
    /// request, plays back canned replies. Lives in /tmp because AF_UNIX
    /// paths are capped at ~108 bytes.
    fn serve(
        replies: &'static [&'static str],
    ) -> (std::path::PathBuf, std::thread::JoinHandle<Value>) {
        let dir =
            std::env::temp_dir().join(format!("mcpd-vl-{}-{:p}", std::process::id(), replies));
        std::fs::create_dir_all(&dir).unwrap();
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
        (path, handle)
    }

    #[test]
    fn streams_until_continues_stops() {
        let (path, server) = serve(&[
            r#"{"parameters":{"context":{"ID":"a.service"}},"continues":true}"#,
            r#"{"parameters":{"context":{"ID":"b.service"}}}"#,
        ]);
        let replies = call(path.to_str().unwrap(), "io.systemd.Unit.List").unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1]["context"]["ID"], json!("b.service"));
        // The request asked for streaming, as systemd's List methods require.
        let request = server.join().unwrap();
        assert_eq!(request["more"], json!(true));
        assert_eq!(request["method"], json!("io.systemd.Unit.List"));
    }

    #[test]
    fn error_replies_and_dead_sockets_are_errors() {
        let (path, _server) = serve(&[r#"{"error":"org.varlink.service.MethodNotFound"}"#]);
        let err = call(path.to_str().unwrap(), "io.systemd.Unit.List").unwrap_err();
        assert!(err.0.contains("MethodNotFound"), "got: {err}");
        // No socket at all — the everyday case on systemd < 258.
        assert!(call("/no/such/socket", "x").is_err());
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
        assert_eq!(normalize_unit(&no_desc).unwrap()["description"], json!(""));
        // Unfamiliar shape → refuse, so the caller falls back to the CLI.
        assert!(normalize_unit(&json!({ "context": { "ID": "x.service" } })).is_err());
    }
}
