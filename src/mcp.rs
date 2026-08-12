//! The Model Context Protocol layer.
//!
//! MCP's stdio transport is line-delimited JSON-RPC 2.0. The server
//! sends no reply to notifications. At this size an SDK and its async
//! runtime would account for most of the binary, so the protocol is
//! implemented directly as a blocking read loop.
//!
//! The server is dual-era. Revision 2026-07-28 removed the `initialize`
//! handshake: a modern request carries its protocol version and the
//! client's capabilities in `_meta`, every result carries a
//! `resultType`, and `server/discover` reports what the server speaks.
//! Legacy clients (2025-11-25 and earlier) still open with
//! `initialize`, and most deployed clients are legacy today, so both
//! are served. Which era a request belongs to is decided by the request
//! itself, never by what came before it on the connection: the protocol
//! is stateless, and a client may interleave unrelated requests on one
//! process.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::systemd::{self, BackendError, Grants, Scope};
use crate::write;

/// Modern revisions: version, capabilities and identity ride on every
/// request. These are the versions accepted in `_meta` and the ones
/// `server/discover` advertises.
const MODERN_VERSIONS: &[&str] = &["2026-07-28"];

/// Legacy revisions, reached through the `initialize` handshake,
/// newest first. Per spec: echo the client's requested version if we
/// support it, otherwise answer with our newest and let the client
/// decide whether to proceed.
const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The `_meta` keys the modern era reserves. `protocolVersion` and
/// `clientCapabilities` are required on every request; `serverInfo`
/// is what a server puts in each result.
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// `UnsupportedProtocolVersion`, from the range (-32020..-32099) the
/// specification reserves for itself. Emitting an undefined code from
/// that range is forbidden, so this is the only one we use.
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// Guidance for the model, identical in both eras.
const INSTRUCTIONS: &str = "Capability-scoped view of systemd on this host. Tools appear \
                            only if their scope was granted at startup. Reads are direct; \
                            state changes (if units:write was granted) go through \
                            plan_change/apply_plan and are refused when the planned state \
                            has drifted.";

/// A tool call that failed. Both bad arguments and backend failures
/// travel as tool-level errors (`isError: true`): each is feedback a
/// model can act on by correcting the call, which is what the
/// specification reserves that channel for. Protocol errors are for
/// what the model cannot fix, and are decided before a handler runs:
/// an unknown tool, or one outside the granted scopes.
struct CallError(String);

impl From<BackendError> for CallError {
    fn from(e: BackendError) -> Self {
        CallError(e.to_string())
    }
}

impl From<&str> for CallError {
    fn from(msg: &str) -> Self {
        CallError(msg.to_string())
    }
}

fn required_unit(args: &Value) -> Result<&str, CallError> {
    args.get("unit")
        .and_then(Value::as_str)
        .ok_or_else(|| CallError::from("missing required argument: unit"))
}

/// The name-glob argument the list tools share, spelled once so the four
/// schemas cannot describe it differently.
fn pattern_schema(what: &str) -> Value {
    json!({
        "type": "string",
        "description": format!(
            "Only return {what} whose name matches this glob \
             ('nginx*', 'systemd-*.service', '*.timer'). Supports * and ?."
        )
    })
}

fn pattern_arg(args: &Value) -> Option<&str> {
    args.get("pattern").and_then(Value::as_str)
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
        description: "List loaded systemd units with their load, active and sub states. \
                      Filter by state, by a glob on the unit name, or both. A typical \
                      host has several hundred loaded units, so filter unless the whole \
                      inventory is the point: 'nginx*' for one service and its instances, \
                      '*.timer' for a unit type.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "enum": systemd::STATES,
                        "description": "Only return units in this state."
                    },
                    "pattern": pattern_schema("units")
                }
            })
        },
        run: |args| {
            Ok(systemd::list_units(
                args.get("state").and_then(Value::as_str),
                pattern_arg(args),
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
        description: "Show the properties of one unit (ExecStart, restart policy, resource \
                      limits, state timestamps, ...). The full set is about 200 properties; \
                      name the ones you need to get those alone. Common: ActiveState, \
                      SubState, Result, ExecMainStatus, ExecMainPID, FragmentPath, \
                      UnitFileState, Restart, NRestarts, MemoryCurrent.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "unit": { "type": "string", "description": "Unit name, e.g. ssh.service" },
                    "properties": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Return only these properties, by exact name. \
                                        Omit for all of them."
                    }
                },
                "required": ["unit"]
            })
        },
        run: |args| {
            let select: Vec<String> = args
                .get("properties")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            Ok(systemd::unit_properties(required_unit(args)?, &select)?)
        },
    },
    Tool {
        name: "list_timers",
        scope: Scope::UnitsRead,
        description: "List timer units: the unit each one activates, when it next elapses, \
                      and when it last did. Both times are UTC timestamps, null for a timer \
                      that has never run or is not scheduled.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": { "pattern": pattern_schema("timers") }
            })
        },
        run: |args| Ok(systemd::list_timers(pattern_arg(args))?),
    },
    Tool {
        name: "list_sockets",
        scope: Scope::UnitsRead,
        description: "List socket units: what they listen on and the unit each one activates.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": { "pattern": pattern_schema("sockets") }
            })
        },
        run: |args| Ok(systemd::list_sockets(pattern_arg(args))?),
    },
    Tool {
        name: "list_unit_files",
        scope: Scope::UnitsRead,
        description: "List installed unit files and their enablement state, the on-disk \
                      view, where list_units shows what is loaded. Filter by state \
                      (enabled, disabled, static, masked, generated, ...), by a glob on \
                      the file name, or both. A host carries hundreds of unit files.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "description": "Only return unit files in this enablement state."
                    },
                    "pattern": pattern_schema("unit files")
                }
            })
        },
        run: |args| {
            Ok(systemd::list_unit_files(
                args.get("state").and_then(Value::as_str),
                pattern_arg(args),
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
                      boot; a slow entry here may still have run in parallel. Returns the \
                      slowest 'limit' units and the total number measured.",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": 25,
                        "description": "How many of the slowest units to return."
                    }
                }
            })
        },
        run: |args| {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(25);
            Ok(systemd::boot_blame(limit as usize)?)
        },
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
                .ok_or_else(|| {
                    CallError::from(
                        "missing or unknown action (start, stop, restart, reload, enable, \
                         disable, mask, unmask, log-level, log-target)",
                    )
                })?;
            let value = args.get("value").and_then(Value::as_str);
            match (action, value) {
                (write::Action::LogLevel, Some(v)) if !write::LOG_LEVELS.contains(&v) => {
                    return Err(CallError::from(
                        "unknown log level (emerg, alert, crit, err, warning, notice, \
                         info, debug)",
                    ))
                }
                (write::Action::LogTarget, Some(v)) if !write::LOG_TARGETS.contains(&v) => {
                    return Err(CallError::from(
                        "unknown log target (console, kmsg, journal, journal-or-kmsg, \
                         auto, null)",
                    ))
                }
                (a, None) if a.takes_value() => {
                    return Err(CallError::from("this action requires a value"))
                }
                (a, Some(_)) if !a.takes_value() => {
                    return Err(CallError::from(
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
                      plan was made against and refuses stale plans. If the unit \
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
                .ok_or_else(|| CallError::from("missing required argument: plan"))?;
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

        // The era is a property of the request, not of the connection.
        // A declared protocol version means the modern era; its absence
        // means a client that opened, or will open, with `initialize`.
        let meta = params.get("_meta");
        let declared = meta
            .and_then(|meta| meta.get(META_VERSION))
            .and_then(Value::as_str);
        // Client capabilities are a modern field, so a request carrying
        // them without a version is a broken modern request rather than
        // a legacy one. Serving it under the older semantics would hide
        // the client's mistake behind an answer that looks fine.
        let modern_but_versionless =
            declared.is_none() && meta.is_some_and(|m| m.get(META_CLIENT_CAPABILITIES).is_some());

        let reply = match declared {
            _ if modern_but_versionless => error_reply(
                id,
                -32602,
                &format!("missing required _meta field: {META_VERSION}"),
            ),
            Some(version) if !MODERN_VERSIONS.contains(&version) => {
                unsupported_version_reply(id, version)
            }
            Some(_) => modern_reply(id, method, &params, grants),
            // A discovery probe from a client that has not chosen a
            // version yet is answered rather than refused: reporting
            // what this server speaks is the whole purpose of the
            // method, and refusing it would tell the client nothing.
            None if method == "server/discover" => {
                result_reply(id, complete(cacheable(discover_result())))
            }
            None => legacy_reply(id, method, &params, grants),
        };
        writeln!(output, "{reply}")?;
        output.flush()?;
    }
    Ok(())
}

fn server_info() -> Value {
    json!({ "name": "systemd-mcpd", "version": env!("CARGO_PKG_VERSION") })
}

/// Marks a modern result complete and attaches the server identity the
/// specification asks every result to carry. `input_required`, the
/// other result type, belongs to the multi-round-trip pattern: this
/// server never asks the client for anything mid-call.
fn complete(mut result: Value) -> Value {
    result["resultType"] = json!("complete");
    result["_meta"] = json!({ META_SERVER_INFO: server_info() });
    result
}

/// Caching hints, mandatory on complete results from `server/discover`
/// and `tools/list`.
///
/// The advertised set is fixed at startup by `--grant` and cannot
/// change while the process runs, which is why no `listChanged`
/// capability is declared and why an hour is a truthful freshness
/// hint. `public` because the reply holds nothing caller-specific: on
/// stdio every request reaches the same process under the same grants.
fn cacheable(mut result: Value) -> Value {
    result["ttlMs"] = json!(3_600_000);
    result["cacheScope"] = json!("public");
    result
}

fn discover_result() -> Value {
    json!({
        "supportedVersions": MODERN_VERSIONS,
        "capabilities": { "tools": {} },
        "instructions": INSTRUCTIONS,
    })
}

fn unsupported_version_reply(id: Value, requested: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": UNSUPPORTED_PROTOCOL_VERSION,
            "message": "Unsupported protocol version",
            "data": { "supported": MODERN_VERSIONS, "requested": requested },
        }
    })
    .to_string()
}

/// A request in the modern era: 2026-07-28 and later.
fn modern_reply(id: Value, method: &str, params: &Value, grants: &Grants) -> String {
    // Client capabilities are required on every request, and a request
    // missing a required `_meta` field is invalid params. Only requests
    // that already declared a modern version reach here, so this
    // cannot reject a legacy client.
    if params
        .get("_meta")
        .and_then(|meta| meta.get(META_CLIENT_CAPABILITIES))
        .is_none()
    {
        return error_reply(
            id,
            -32602,
            &format!("missing required _meta field: {META_CLIENT_CAPABILITIES}"),
        );
    }

    match method {
        "server/discover" => result_reply(id, complete(cacheable(discover_result()))),
        "ping" => result_reply(id, complete(json!({}))),
        "tools/list" => result_reply(id, complete(cacheable(tools_list(grants)))),
        // One dispatch for both eras, so a tool cannot be reachable in
        // one and gated in the other.
        "tools/call" => match call_tool(params, grants, true) {
            Ok(result) => result_reply(id, complete(result)),
            Err((code, message)) => error_reply(id, code, &message),
        },
        other => error_reply(id, -32601, &format!("method not found: {other}")),
    }
}

/// A request in the legacy era: the `initialize` handshake, 2025-11-25
/// and earlier.
fn legacy_reply(id: Value, method: &str, params: &Value, grants: &Grants) -> String {
    match method {
        "initialize" => result_reply(id, initialize_result(params)),
        "ping" => result_reply(id, json!({})),
        "tools/list" => result_reply(id, tools_list(grants)),
        "tools/call" => match call_tool(params, grants, false) {
            Ok(result) => result_reply(id, result),
            Err((code, message)) => error_reply(id, code, &message),
        },
        other => error_reply(id, -32601, &format!("method not found: {other}")),
    }
}

fn initialize_result(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|v| LEGACY_VERSIONS.contains(v))
        .unwrap_or(LEGACY_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": server_info(),
        "instructions": INSTRUCTIONS,
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

/// Runs one tool call, shared by both eras. Returns the result object,
/// or the code and message of a protocol error.
///
/// `structured` adds the reply as JSON in `structuredContent` beside
/// the text block, which the modern era defines and the legacy clients
/// this server has to keep working predate. The text block is sent
/// either way: a tool returning structured content should also
/// serialize it there.
fn call_tool(params: &Value, grants: &Grants, structured: bool) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
        return Err((-32602, format!("unknown tool: {name}")));
    };

    // Enforced here as well as in `tools/list`: an unadvertised tool
    // must also fail when called directly.
    if !grants.allows(tool.scope) {
        return Err((
            -32602,
            format!(
                "tool '{}' requires scope '{}' which was not granted",
                name, tool.scope
            ),
        ));
    }

    // Bad arguments and backend failures are both tool-level errors
    // (isError: true), which is the channel a model can recover from:
    // the session continues and the client passes the message on.
    Ok(match (tool.run)(&args) {
        Ok(value) => {
            let mut result = json!({
                "content": [{ "type": "text", "text": value.to_string() }],
                "isError": false,
            });
            if structured {
                result["structuredContent"] = value;
            }
            result
        }
        Err(CallError(message)) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    })
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

    /// A modern request line: `_meta` carrying the required fields.
    fn modern(id: u32, method: &str, params: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{{params}"_meta":{{"{META_VERSION}":"2026-07-28","{META_CLIENT_CAPABILITIES}":{{}}}}}}}}"#
        )
    }

    #[test]
    fn discovery_reports_what_the_server_speaks() {
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(&grants, &modern(1, "server/discover", ""));
        let result = &replies[0]["result"];
        assert_eq!(result["resultType"], json!("complete"));
        assert_eq!(result["supportedVersions"], json!(MODERN_VERSIONS));
        assert_eq!(result["capabilities"]["tools"], json!({}));
        assert_eq!(
            result["_meta"][META_SERVER_INFO]["name"],
            json!("systemd-mcpd")
        );
        // server/discover is cacheable, so both hints must be present.
        assert!(result["ttlMs"].as_i64().is_some_and(|t| t >= 0));
        assert_eq!(result["cacheScope"], json!("public"));
    }

    #[test]
    fn discovery_answers_a_probe_that_declares_no_version() {
        // A dual-era client probes with server/discover before it knows
        // which era this server is. Refusing would defeat the probe.
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server/discover\"}\n",
        );
        assert_eq!(
            replies[0]["result"]["supportedVersions"],
            json!(MODERN_VERSIONS)
        );
    }

    #[test]
    fn unsupported_modern_version_is_refused_with_the_list() {
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"_meta":{{"{META_VERSION}":"1900-01-01","{META_CLIENT_CAPABILITIES}":{{}}}}}}}}"#
            ),
        );
        let error = &replies[0]["error"];
        assert_eq!(error["code"], json!(UNSUPPORTED_PROTOCOL_VERSION));
        assert_eq!(error["data"]["requested"], json!("1900-01-01"));
        assert_eq!(error["data"]["supported"], json!(MODERN_VERSIONS));
        // A legacy version is not usable in the modern era either: it
        // is reachable through initialize, not through _meta.
        let replies = exchange(
            &grants,
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"_meta":{{"{META_VERSION}":"2025-06-18","{META_CLIENT_CAPABILITIES}":{{}}}}}}}}"#
            ),
        );
        assert_eq!(
            replies[0]["error"]["code"],
            json!(UNSUPPORTED_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn modern_requests_must_carry_client_capabilities() {
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"_meta":{{"{META_VERSION}":"2026-07-28"}}}}}}"#
            ),
        );
        assert_eq!(replies[0]["error"]["code"], json!(-32602));
        let msg = replies[0]["error"]["message"].as_str().unwrap();
        assert!(msg.contains(META_CLIENT_CAPABILITIES), "got: {msg}");
    }

    #[test]
    fn a_versionless_modern_request_is_not_served_as_legacy() {
        // Found by the conformance suite: a request carrying client
        // capabilities has declared itself modern, so a missing version
        // is a malformed request, not an invitation to answer under the
        // handshake semantics.
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"_meta":{{"{META_CLIENT_CAPABILITIES}":{{}}}}}}}}"#
            ),
        );
        assert_eq!(replies[0]["error"]["code"], json!(-32602));
        let msg = replies[0]["error"]["message"].as_str().unwrap();
        assert!(msg.contains(META_VERSION), "got: {msg}");
        // A legacy request with an unrelated _meta key is still legacy:
        // progressToken predates the modern fields.
        let replies = exchange(
            &grants,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"progressToken":7}}}"#,
        );
        assert!(replies[0]["result"]["tools"].is_array());
    }

    #[test]
    fn modern_results_carry_the_era_fields() {
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            &format!("{}\n{}", modern(1, "tools/list", ""), modern(2, "ping", "")),
        );
        let list = &replies[0]["result"];
        assert_eq!(list["resultType"], json!("complete"));
        assert_eq!(list["cacheScope"], json!("public"));
        assert!(!list["tools"].as_array().unwrap().is_empty());
        // Every result carries the discriminator, including empty ones.
        assert_eq!(replies[1]["result"]["resultType"], json!("complete"));
    }

    #[test]
    fn scopes_gate_the_modern_era_too() {
        // The second dispatch path must not become a way around the
        // scope check.
        let grants = Grants::from_args("units:read").unwrap();
        let replies = exchange(
            &grants,
            &modern(
                1,
                "tools/call",
                r#""name":"unit_logs","arguments":{"unit":"ssh.service"},"#,
            ),
        );
        assert_eq!(replies[0]["error"]["code"], json!(-32602));
        let msg = replies[0]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("journal:read"), "got: {msg}");
    }

    #[test]
    fn bad_arguments_are_tool_errors_the_model_can_correct() {
        // An unknown action is input validation, not a malformed
        // request: it reaches the model as isError so it can retry,
        // rather than as a protocol error the client may swallow.
        let grants = Grants::from_args("units:write").unwrap();
        let replies = exchange(
            &grants,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"plan_change","arguments":{"action":"isolate","unit":"x.service"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"apply_plan","arguments":{}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"no_such_tool"}}
"#,
        );
        assert_eq!(replies[0]["result"]["isError"], json!(true));
        let msg = replies[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("unknown action"), "got: {msg}");
        assert_eq!(replies[1]["result"]["isError"], json!(true));
        // An unknown tool stays a protocol error: no argument fixes it.
        assert_eq!(replies[2]["error"]["code"], json!(-32602));
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
            json!(LEGACY_VERSIONS[0])
        );
    }

    #[test]
    fn legacy_replies_keep_their_shape() {
        // Modern clients get resultType, caching hints and structured
        // content; legacy clients must see none of it, whatever era
        // fields were added around them.
        let grants = Grants::from_args("units:write").unwrap();
        let replies = exchange(
            &grants,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"apply_plan","arguments":{"plan":123456789}}}
"#,
        );
        let list = &replies[0]["result"];
        assert!(list.get("resultType").is_none());
        assert!(list.get("ttlMs").is_none() && list.get("cacheScope").is_none());
        assert!(list.get("_meta").is_none());
        let call = &replies[1]["result"];
        assert!(call.get("resultType").is_none());
        assert!(call.get("structuredContent").is_none());
        assert_eq!(call["isError"], json!(true));
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
        // ...unknown actions and missing plan ids are tool errors, so
        // the model sees them and can correct the call...
        assert_eq!(replies[1]["result"]["isError"], json!(true));
        assert_eq!(replies[2]["result"]["isError"], json!(true));
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
