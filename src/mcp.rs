//! The Model Context Protocol layer.
//!
//! MCP's stdio transport is line-delimited JSON-RPC 2.0. The server
//! implements the four required methods — `initialize`, `tools/list`,
//! `tools/call`, `ping` — and sends no reply to notifications. At this
//! size an SDK and its async runtime would account for most of the
//! binary, so the protocol is implemented directly as a blocking read
//! loop.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::systemd::{self, BackendError, Grants, Scope};
use crate::write;

/// Protocol revisions we know, newest first. Per spec: echo the client's
/// requested version if we support it, otherwise answer with our newest and
/// let the client decide whether to proceed.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// A tool call fails one of two ways, and they travel differently on the
/// wire: bad arguments are a protocol error (JSON-RPC -32602), backend
/// failures are tool-level errors (`isError: true`) the model reacts to.
enum CallError {
    Args(&'static str),
    Backend(BackendError),
}

impl From<BackendError> for CallError {
    fn from(e: BackendError) -> Self {
        CallError::Backend(e)
    }
}

fn required_unit(args: &Value) -> Result<&str, CallError> {
    args.get("unit")
        .and_then(Value::as_str)
        .ok_or(CallError::Args("missing required argument: unit"))
}

/// One tool: its wire description, the scope that gates it, and its handler.
struct Tool {
    name: &'static str,
    scope: Scope,
    description: &'static str,
    input_schema: fn() -> Value,
    run: fn(&Value) -> Result<Value, CallError>,
}

/// The registry. Adding a tool is one entry: schema, scope, and handler
/// are declared together, and there is no separate dispatch table.
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
                        "enum": systemd::STATES,
                        "description": "Only return units in this state."
                    }
                }
            })
        },
        run: |args| {
            Ok(systemd::list_units(
                args.get("state").and_then(Value::as_str),
            )?)
        },
    },
    Tool {
        name: "failed_units",
        scope: Scope::UnitsRead,
        description: "List units that are currently in the failed state.",
        input_schema: || json!({ "type": "object", "properties": {} }),
        run: |_| Ok(systemd::failed_units()?),
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
        run: |args| Ok(systemd::unit_properties(required_unit(args)?)?),
    },
    Tool {
        name: "list_timers",
        scope: Scope::UnitsRead,
        description: "List timer units: schedule, next elapse, last trigger, and the unit \
                      each one activates.",
        input_schema: || json!({ "type": "object", "properties": {} }),
        run: |_| Ok(systemd::list_timers()?),
    },
    Tool {
        name: "list_sockets",
        scope: Scope::UnitsRead,
        description: "List socket units: what they listen on and the unit each one activates.",
        input_schema: || json!({ "type": "object", "properties": {} }),
        run: |_| Ok(systemd::list_sockets()?),
    },
    Tool {
        name: "list_unit_files",
        scope: Scope::UnitsRead,
        description: "List installed unit files and their enablement state — the on-disk \
                      view, where list_units shows what is loaded. Optionally filter by \
                      state (enabled, disabled, static, masked, generated, ...).",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "description": "Only return unit files in this enablement state."
                    }
                }
            })
        },
        run: |args| {
            Ok(systemd::list_unit_files(
                args.get("state").and_then(Value::as_str),
            )?)
        },
    },
    Tool {
        name: "unit_dependencies",
        scope: Scope::UnitsRead,
        description: "One unit's dependency edges by relation, forward (Requires, Wants, \
                      After, ...) and reverse (WantedBy, TriggeredBy, ...). Every relation \
                      is present; empty ones are empty arrays.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "unit": { "type": "string", "description": "Unit name, e.g. ssh.service" }
                },
                "required": ["unit"]
            })
        },
        run: |args| Ok(systemd::unit_dependencies(required_unit(args)?)?),
    },
    Tool {
        name: "unit_security",
        scope: Scope::UnitsRead,
        description: "systemd-analyze's sandboxing exposure analysis of one running \
                      service: which hardening options it uses, which it lacks, and the \
                      overall exposure score.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "unit": { "type": "string", "description": "Unit name, e.g. ssh.service" }
                },
                "required": ["unit"]
            })
        },
        run: |args| Ok(systemd::unit_security(required_unit(args)?)?),
    },
    Tool {
        name: "unit_log_control",
        scope: Scope::UnitsRead,
        description: "One service's runtime log level and log target, read through \
                      systemd's LogControl1 interface over D-Bus. The service must \
                      declare BusName= and implement the interface (systemd-logind, \
                      systemd-resolved, and similar do); the error names the \
                      requirement otherwise.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "unit": { "type": "string", "description": "Unit name, e.g. systemd-logind.service" }
                },
                "required": ["unit"]
            })
        },
        run: |args| Ok(systemd::unit_log_control(required_unit(args)?)?),
    },
    Tool {
        name: "unit_logs",
        scope: Scope::JournalRead,
        description: "Read journal entries for one unit, filtered by priority, time \
                      window, boot, and message pattern.",
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
                    },
                    "priority": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 7,
                        "description": "Only entries at this syslog priority or more severe \
                                        (0=emerg .. 7=debug)."
                    },
                    "since": {
                        "type": "string",
                        "description": "Start of the time window, journalctl syntax \
                                        ('2026-08-12 06:00:00', '-5min', 'yesterday')."
                    },
                    "until": {
                        "type": "string",
                        "description": "End of the time window, journalctl syntax."
                    },
                    "boot": {
                        "type": "integer",
                        "description": "Boot offset: 0 is the current boot, -1 the one before."
                    },
                    "grep": {
                        "type": "string",
                        "description": "Only entries whose message matches this regular \
                                        expression."
                    }
                },
                "required": ["unit"]
            })
        },
        run: |args| {
            let filter = systemd::LogFilter {
                lines: args.get("lines").and_then(Value::as_u64).unwrap_or(50),
                priority: args.get("priority").and_then(Value::as_u64),
                since: args.get("since").and_then(Value::as_str),
                until: args.get("until").and_then(Value::as_str),
                boot: args.get("boot").and_then(Value::as_i64),
                grep: args.get("grep").and_then(Value::as_str),
            };
            Ok(systemd::unit_logs(required_unit(args)?, &filter)?)
        },
    },
    Tool {
        name: "list_boots",
        scope: Scope::JournalRead,
        description: "List the boots recorded in the journal, with boot ids and first/last \
                      entry timestamps. Boot offsets from this list select a boot in \
                      unit_logs.",
        input_schema: || json!({ "type": "object", "properties": {} }),
        run: |_| Ok(systemd::list_boots()?),
    },
    Tool {
        name: "boot_times",
        scope: Scope::BootRead,
        description: "How long the last boot took, split into firmware, loader, kernel, initrd \
                      and userspace phases. Microsecond values, read from the same manager \
                      timestamps systemd-analyze uses. Phases that did not occur are omitted.",
        input_schema: || json!({ "type": "object", "properties": {} }),
        run: |_| Ok(systemd::boot_times()?),
    },
    Tool {
        name: "critical_chain",
        scope: Scope::BootRead,
        description: "The chain of units that gated reaching the default target (or one given \
                      unit) during boot. 'activated' is when the unit became active relative to \
                      boot start; 'duration' is how long its own startup took. The slowest link \
                      is usually the entry with the largest duration.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "unit": {
                        "type": "string",
                        "description": "Analyze the chain to this unit instead of the default target."
                    }
                }
            })
        },
        run: |args| {
            Ok(systemd::critical_chain(
                args.get("unit").and_then(Value::as_str),
            )?)
        },
    },
    Tool {
        name: "boot_blame",
        scope: Scope::BootRead,
        description: "Units ordered by how long their own startup took, slowest first. \
                      Unlike critical_chain this includes units that did not gate the \
                      boot; a slow entry here may still have run in parallel.",
        input_schema: || json!({ "type": "object", "properties": {} }),
        run: |_| Ok(systemd::boot_blame()?),
    },
    Tool {
        name: "plan_change",
        scope: Scope::UnitsWrite,
        description: "Plan a unit lifecycle change (start, stop, restart, reload), \
                      enablement change (enable, disable, mask, unmask), or log-control \
                      change (log-level, log-target, which require a value) without \
                      executing anything. Returns the unit's current state, the \
                      predicted state (null where the outcome cannot be derived), the \
                      rollback action (with the value that restores the previous state, \
                      where one applies), and a plan id for apply_plan. Plans are \
                      single-use and last only for this session.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["start", "stop", "restart", "reload",
                                 "enable", "disable", "mask", "unmask",
                                 "log-level", "log-target"],
                        "description": "The change to plan."
                    },
                    "unit": { "type": "string", "description": "Unit name, e.g. ssh.service" },
                    "value": {
                        "type": "string",
                        "description": "For log-level: emerg..debug. For log-target: \
                                        console, kmsg, journal, journal-or-kmsg, auto, \
                                        null. Rejected for other actions."
                    }
                },
                "required": ["action", "unit"]
            })
        },
        run: |args| {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .and_then(write::Action::parse)
                .ok_or(CallError::Args(
                    "missing or unknown action (start, stop, restart, reload, enable, \
                     disable, mask, unmask, log-level, log-target)",
                ))?;
            let value = args.get("value").and_then(Value::as_str);
            match (action, value) {
                (write::Action::LogLevel, Some(v)) if !write::LOG_LEVELS.contains(&v) => {
                    return Err(CallError::Args(
                        "unknown log level (emerg, alert, crit, err, warning, notice, \
                         info, debug)",
                    ))
                }
                (write::Action::LogTarget, Some(v)) if !write::LOG_TARGETS.contains(&v) => {
                    return Err(CallError::Args(
                        "unknown log target (console, kmsg, journal, journal-or-kmsg, \
                         auto, null)",
                    ))
                }
                (a, None) if a.takes_value() => {
                    return Err(CallError::Args("this action requires a value"))
                }
                (a, Some(_)) if !a.takes_value() => {
                    return Err(CallError::Args(
                        "value is only accepted for log-level and log-target",
                    ))
                }
                _ => {}
            }
            Ok(write::plan(action, required_unit(args)?, value)?)
        },
    },
    Tool {
        name: "apply_plan",
        scope: Scope::UnitsWrite,
        description: "Execute a plan created by plan_change. Re-checks the state the \
                      plan was made against and refuses stale plans — if the unit \
                      changed in between, re-plan. Returns a before/after diff, the \
                      filesystem changes systemd reported (symlink creations and \
                      removals for enablement actions), and the rollback action.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "plan": { "type": "integer", "description": "Plan id from plan_change." }
                },
                "required": ["plan"]
            })
        },
        run: |args| {
            let id = args
                .get("plan")
                .and_then(Value::as_u64)
                .ok_or(CallError::Args("missing required argument: plan"))?;
            Ok(write::apply(id)?)
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
            "initialize" => result_reply(id, initialize_result(&params)),
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

fn initialize_result(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "systemd-mcpd",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Capability-scoped view of systemd on this host. Tools appear \
                         only if their scope was granted at startup. Reads are direct; \
                         state changes (if units:write was granted) go through \
                         plan_change/apply_plan and are refused when the planned state \
                         has drifted."
    })
}

/// Only granted tools are advertised; ungranted tools are additionally
/// refused in `tools/call`.
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

    // Enforced here as well as in `tools/list`: an unadvertised tool
    // must also fail when called directly.
    if !grants.allows(tool.scope) {
        return error_reply(
            id,
            -32602,
            &format!(
                "tool '{}' requires scope '{}' which was not granted",
                name, tool.scope
            ),
        );
    }

    // Backend failures are tool-level errors (isError: true), not protocol
    // errors: the session continues and the client receives the message.
    let (text, is_error) = match (tool.run)(&args) {
        Ok(value) => (value.to_string(), false),
        Err(CallError::Args(msg)) => return error_reply(id, -32602, msg),
        Err(CallError::Backend(e)) => (e.to_string(), true),
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

    /// The advertised tool names in a tools/list reply.
    fn tool_names(reply: &Value) -> Vec<&str> {
        reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn handshake() {
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}
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
        // A supported requested version is echoed back, per spec.
        assert_eq!(replies[0]["result"]["protocolVersion"], json!("2025-03-26"));
    }

    #[test]
    fn version_negotiation_falls_back_to_newest() {
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-12-31"}}
"#,
        );
        assert_eq!(
            replies[0]["result"]["protocolVersion"],
            json!(PROTOCOL_VERSIONS[0])
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
        // journal- and boot-scoped tools are invisible without their scopes...
        assert_eq!(
            tool_names(&replies[0]),
            [
                "list_units",
                "failed_units",
                "unit_properties",
                "list_timers",
                "list_sockets",
                "list_unit_files",
                "unit_dependencies",
                "unit_security",
                "unit_log_control",
            ]
        );
        // ...and calling one anyway is refused, not routed.
        let msg = replies[1]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("journal:read"), "got: {msg}");
    }

    #[test]
    fn boot_scope_gates_boot_tools() {
        let grants = Grants::from_args("boot:read").unwrap();
        let replies = exchange(
            &grants,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_units"}}
"#,
        );
        assert_eq!(
            tool_names(&replies[0]),
            ["boot_times", "critical_chain", "boot_blame"]
        );
        let msg = replies[1]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("units:read"), "got: {msg}");
    }

    #[test]
    fn write_scope_gates_write_tools() {
        let grants = Grants::from_args("units:write").unwrap();
        let replies = exchange(
            &grants,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"plan_change","arguments":{"action":"isolate","unit":"x.service"}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"apply_plan","arguments":{}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"apply_plan","arguments":{"plan":123456789}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_units"}}
"#,
        );
        // Only the write tools are advertised under units:write...
        assert_eq!(tool_names(&replies[0]), ["plan_change", "apply_plan"]);
        // ...unknown actions and missing plan ids are protocol errors...
        assert_eq!(replies[1]["error"]["code"], json!(-32602));
        assert_eq!(replies[2]["error"]["code"], json!(-32602));
        // ...an unknown plan is a tool-level error directing the client
        // to re-plan...
        assert_eq!(replies[3]["result"]["isError"], json!(true));
        let msg = replies[3]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("unknown plan"), "got: {msg}");
        // ...and read tools are refused without their scope.
        let msg = replies[4]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("units:read"), "got: {msg}");
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
