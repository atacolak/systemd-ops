//! A varlink client in the fewest lines that are correct.
//!
//! Varlink is NUL-terminated JSON over a Unix socket — `UnixStream` plus the
//! serde_json we already ship covers it; a varlink crate would be a third
//! dependency for a protocol this file fits in.
//!
//! systemd ≥ 257 exposes PID 1's manager at `/run/systemd/io.systemd.Manager`.
//! We speak to it when it's there and say nothing when it isn't: any surprise
//! at all — no socket, old systemd, unfamiliar reply shape — is an `Err`, and
//! the caller falls back to the CLI. The probe is the `connect()` itself; a
//! failed connect to a missing path costs nothing worth caching.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{json, Value};

const MANAGER_SOCKET: &str = "/run/systemd/io.systemd.Manager";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Units via `io.systemd.Manager.ListUnits`, normalized to the exact shape
/// `systemctl list-units --output=json` emits — callers cannot tell the
/// backends apart, which is the point.
pub fn list_units() -> Result<Vec<Value>, String> {
    call(MANAGER_SOCKET, "io.systemd.Manager.ListUnits")?
        .iter()
        .map(normalize_unit)
        .collect()
}

/// One streamed method call ("more" mode, which is how systemd's List*
/// methods reply: one message per entry, `continues` on all but the last).
/// Returns each reply's `parameters`, in order.
fn call(socket_path: &str, method: &str) -> Result<Vec<Value>, String> {
    let stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let mut request =
        serde_json::to_vec(&json!({ "method": method, "parameters": {}, "more": true }))
            .expect("static request serializes");
    request.push(0);
    (&stream)
        .write_all(&request)
        .map_err(|e| format!("write to {socket_path}: {e}"))?;

    let mut reader = BufReader::new(&stream);
    let mut replies = Vec::new();
    loop {
        let mut buf = Vec::new();
        reader
            .read_until(0, &mut buf)
            .map_err(|e| format!("read from {socket_path}: {e}"))?;
        if buf.pop() != Some(0) {
            return Err("truncated varlink reply".into());
        }
        let reply: Value =
            serde_json::from_slice(&buf).map_err(|e| format!("bad varlink JSON: {e}"))?;
        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Err(format!("varlink error: {error}"));
        }
        let continues = reply
            .get("continues")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        replies.push(reply.get("parameters").cloned().unwrap_or(json!({})));
        if !continues {
            return Ok(replies);
        }
    }
}

/// varlink speaks camelCase (`activeState`); systemctl's JSON speaks short
/// names (`active`). One shape goes out either way. A missing required field
/// means the interface isn't what we expect — refuse and let the CLI answer,
/// rather than emit half a unit.
fn normalize_unit(entry: &Value) -> Result<Value, String> {
    let field = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("varlink unit entry missing '{key}'"))
    };
    Ok(json!({
        "unit": field("name")?,
        "load": field("loadState")?,
        "active": field("activeState")?,
        "sub": field("subState")?,
        "description": entry.get("description").and_then(Value::as_str).unwrap_or(""),
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
            r#"{"parameters":{"name":"a.service"},"continues":true}"#,
            r#"{"parameters":{"name":"b.service"}}"#,
        ]);
        let replies = call(path.to_str().unwrap(), "io.systemd.Manager.ListUnits").unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1]["name"], json!("b.service"));
        // The request asked for streaming, as systemd's List* methods require.
        let request = server.join().unwrap();
        assert_eq!(request["more"], json!(true));
        assert_eq!(request["method"], json!("io.systemd.Manager.ListUnits"));
    }

    #[test]
    fn error_replies_and_dead_sockets_are_errors() {
        let (path, _server) = serve(&[r#"{"error":"org.varlink.service.MethodNotFound"}"#]);
        let err = call(path.to_str().unwrap(), "io.systemd.Manager.ListUnits").unwrap_err();
        assert!(err.contains("MethodNotFound"), "got: {err}");
        // No socket at all — the everyday case on systemd < 257.
        assert!(call("/no/such/socket", "x").is_err());
    }

    #[test]
    fn normalization_is_strict() {
        let full = json!({
            "name": "ssh.service", "loadState": "loaded",
            "activeState": "active", "subState": "running",
            "description": "OpenBSD Secure Shell server"
        });
        assert_eq!(
            normalize_unit(&full).unwrap(),
            json!({
                "unit": "ssh.service", "load": "loaded", "active": "active",
                "sub": "running", "description": "OpenBSD Secure Shell server"
            })
        );
        // Unfamiliar shape → refuse, so the caller falls back to the CLI.
        assert!(normalize_unit(&json!({ "name": "x.service" })).is_err());
    }
}
