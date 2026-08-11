//! The Model Context Protocol, by hand.
//!
//! MCP's stdio transport is line-delimited JSON-RPC 2.0. A server needs to
//! answer exactly four things — `initialize`, `tools/list`, `tools/call`,
//! `ping` — and stay quiet about notifications. That is small enough that an
//! SDK (and the async runtime it drags in) would be most of the binary for
//! none of the benefit. A blocking read loop is the honest shape of a stdio
//! protocol.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::systemd::{self, Grants, Scope};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// One tool: its wire description and the scope that gates it.
struct Tool {
    name: &'static str,
    scope: Scope,
    description: &'static str,
    input_schema: fn() -> Value,
}

/// The registry. Adding a tool means adding one entry here and one match arm
/// in `call_tool` — the compiler complains if you forget the second half.
const TOOLS: &[Tool] = &[
    Tool {
        name: "list_units",
        scope: Scope::UnitsRead,
        description: "List all loaded systemd units with their load, active and sub states. \
                      Optionally filter by state.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "enum": ["active", "inactive", "failed", "activating", "deactivating"],
                        "description": "Only return units in this state."
                    }
                }
            })
        },
    },
    Tool {
        name: "failed_units",
        scope: Scope::UnitsRead,
        description: "List units that are currently in the failed state. \
                      The usual first question on an unhealthy machine.",
        input_schema: || json!({ "type": "object", "properties": {} }),
    },
    Tool {
        name: "unit_properties",
        scope: Scope::UnitsRead,
        description: "Show the full property set of one unit \
                      (ExecStart, restart policy, resource limits, state timestamps, ...).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "unit": { "type": "string", "description": "Unit name, e.g. ssh.service" }
                },
                "required": ["unit"]
            })
        },
    },
    Tool {
        name: "unit_logs",
        scope: Scope::JournalRead,
        description: "Read the most recent journal entries for one unit.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "unit": { "type": "string", "description": "Unit name, e.g. ssh.service" },
                    "lines": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": 50,
                        "description": "How many entries, newest last."
                    }
                },
                "required": ["unit"]
            })
        },
    },
];

/// Serve MCP on the given streams until EOF. Returns on clean shutdown.
pub fn serve(input: impl BufRead, mut output: impl Write, grants: &Grants) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                // Parse errors have no id to echo; -32700 per JSON-RPC.
                let reply = error_reply(Value::Null, -32700, &format!("parse error: {e}"));
                writeln!(output, "{reply}")?;
                continue;
            }
        };

        // Notifications (no id) get no reply. This includes
        // `notifications/initialized`, which is the only one MCP sends us.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };

        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let reply = match method {
            "initialize" => result_reply(id, initialize_result()),
            "ping" => result_reply(id, json!({})),
            "tools/list" => result_reply(id, tools_list(grants)),
            "tools/call" => tools_call(id, &params, grants),
            other => error_reply(id, -32601, &format!("method not found: {other}")),
        };
        writeln!(output, "{reply}")?;
        output.flush()?;
    }
    Ok(())
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "systemd-mcpd",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Read-only view of systemd on this host. \
                         Tools appear only if their capability scope was granted at startup."
    })
}

/// Only granted tools are advertised. A model cannot want what it cannot see.
fn tools_list(grants: &Grants) -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .filter(|t| grants.allows(t.scope))
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.input_schema)(),
            })
        })
        .collect();
    json!({ "tools": tools })
}

fn tools_call(id: Value, params: &Value, grants: &Grants) -> String {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
        return error_reply(id, -32602, &format!("unknown tool: {name}"));
    };

    // Enforced here as well as in `tools/list`: hiding a tool is UX,
    // refusing the call is security.
    if !grants.allows(tool.scope) {
        return error_reply(
            id,
            -32602,
            &format!("tool '{}' requires scope '{}' which was not granted", name, tool.scope),
        );
    }

    let result = match name {
        "list_units" => systemd::list_units(args.get("state").and_then(Value::as_str)),
        "failed_units" => systemd::failed_units(),
        "unit_properties" => match args.get("unit").and_then(Value::as_str) {
            Some(unit) => systemd::unit_properties(unit),
            None => return error_reply(id, -32602, "missing required argument: unit"),
        },
        "unit_logs" => match args.get("unit").and_then(Value::as_str) {
            Some(unit) => {
                let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(50);
                systemd::unit_logs(unit, lines)
            }
            None => return error_reply(id, -32602, "missing required argument: unit"),
        },
        _ => unreachable!("tool in registry but not dispatched: {name}"),
    };

    // Backend failures are tool-level errors (isError: true), not protocol
    // errors: the conversation goes on, the model just learns what happened.
    let (text, is_error) = match result {
        Ok(value) => (value.to_string(), false),
        Err(e) => (e.to_string(), true),
    };
    result_reply(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "isError": is_error,
        }),
    )
}

fn result_reply(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_reply(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the server with raw protocol lines, capture its replies.
    fn exchange(grants: &Grants, lines: &str) -> Vec<Value> {
        let mut out = Vec::new();
        serve(lines.as_bytes(), &mut out, grants).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn handshake() {
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"ping"}
"#,
        );
        // The notification produced no reply: two replies for three lines.
        assert_eq!(replies.len(), 2);
        assert_eq!(
            replies[0]["result"]["serverInfo"]["name"],
            json!("systemd-mcpd")
        );
    }

    #[test]
    fn grants_gate_listing_and_calls() {
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"unit_logs","arguments":{"unit":"ssh.service"}}}
"#,
        );
        let names: Vec<&str> = replies[0]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        // journal-scoped tool is invisible without journal:read...
        assert_eq!(names, ["list_units", "failed_units", "unit_properties"]);
        // ...and calling it anyway is refused, not routed.
        let msg = replies[1]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("journal:read"), "got: {msg}");
    }

    #[test]
    fn unknown_method_and_bad_json() {
        let grants = Grants::default();
        let replies = exchange(
            &grants,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"nope\"}\nthis is not json\n",
        );
        assert_eq!(replies[0]["error"]["code"], json!(-32601));
        assert_eq!(replies[1]["error"]["code"], json!(-32700));
    }
}
