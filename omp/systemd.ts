/**
 * systemd — OMP custom tools over the systemd-ops CLI.
 *
 * Inspect first. systemd_control to restart/enable. systemd_author to
 * define. Never shell systemctl for write-prefix units.
 *
 * Each call is one `systemd-ops --json` process. No hidden MCP child.
 * Write-prefix comes from systemd-ops config/env, not this adapter.
 *
 * Symlink: ~/worlds/base/tools/systemd.ts → this file.
 */
import { spawn } from "node:child_process";
import { homedir } from "node:os";

const DEFAULT_BIN = `${homedir()}/.local/bin/systemd-ops`;

const INSPECT_ACTIONS = [
	"list_operations",
	"get_operation",
	"list_units",
	"failed_units",
	"get_unit",
	"list_timers",
	"list_unit_files",
	"unit_dependencies",
	"unit_logs",
	"scope_show",
] as const;

const CONTROL_LIFECYCLE = [
	"start",
	"stop",
	"restart",
	"reload",
	"enable",
	"disable",
	"reset-failed",
] as const;

const SPEC_KEYS = [
	"unit",
	"kind",
	"title",
	"purpose",
	"tags",
	"description",
	"exec",
	"cwd",
	"env",
	"path",
	"environment_files",
	"after",
	"wants_network_online",
	"restart",
	"nice",
	"schedule",
	"enabled",
	"start_now",
] as const;

type Json = Record<string, unknown>;

type SessionLike = {
	cwd?: string;
};

function textResult(text: string, extra?: { isError?: boolean; details?: unknown }) {
	return {
		content: [{ type: "text" as const, text }],
		...(extra?.isError ? { isError: true as const } : {}),
		...(extra?.details !== undefined ? { details: extra.details } : {}),
	};
}

function sessionCwd(ctx: SessionLike | undefined, fallback: string | undefined): string | undefined {
	if (typeof ctx?.cwd === "string" && ctx.cwd.length > 0) return ctx.cwd;
	if (typeof fallback === "string" && fallback.length > 0) return fallback;
	return undefined;
}

function pick(obj: Json, keys: readonly string[]): Json {
	const out: Json = {};
	for (const k of keys) {
		if (obj[k] !== undefined) out[k] = obj[k];
	}
	return out;
}

function opsBin(): string {
	return process.env.SYSTEMD_OPS_BIN?.trim() || DEFAULT_BIN;
}

function inspectArgv(action: string, args: Json): string[] {
	const argv: string[] = [];
	switch (action) {
		case "list_operations":
			argv.push("inspect", "list-operations");
			if (typeof args.pattern === "string") argv.push("--pattern", args.pattern);
			break;
		case "get_operation":
			argv.push("inspect", "get-operation", "--unit", String(args.unit ?? ""));
			break;
		case "list_units":
			argv.push("inspect", "list-units");
			if (typeof args.pattern === "string") argv.push("--pattern", args.pattern);
			if (typeof args.state === "string") argv.push("--state", args.state);
			break;
		case "failed_units":
			argv.push("inspect", "failed-units");
			break;
		case "get_unit":
			argv.push("inspect", "get-unit", "--unit", String(args.unit ?? ""));
			break;
		case "list_timers":
			argv.push("inspect", "list-timers");
			if (typeof args.pattern === "string") argv.push("--pattern", args.pattern);
			break;
		case "list_unit_files":
			argv.push("inspect", "list-unit-files");
			if (typeof args.pattern === "string") argv.push("--pattern", args.pattern);
			if (typeof args.state === "string") argv.push("--state", args.state);
			break;
		case "unit_dependencies":
			argv.push("inspect", "unit-dependencies", "--unit", String(args.unit ?? ""));
			break;
		case "unit_logs":
			argv.push("inspect", "unit-logs", "--unit", String(args.unit ?? ""));
			if (typeof args.lines === "number") argv.push("--lines", String(args.lines));
			if (typeof args.since === "string") argv.push("--since", args.since);
			if (typeof args.until === "string") argv.push("--until", args.until);
			if (typeof args.priority === "number") argv.push("--priority", String(args.priority));
			if (typeof args.grep === "string") argv.push("--grep", args.grep);
			if (typeof args.boot === "number") argv.push("--boot", String(args.boot));
			break;
		case "scope_show":
			argv.push("scope", "show");
			break;
		default:
			throw new Error(`unknown inspect action '${action}'`);
	}
	return argv;
}

function runOps(argv: string[], cwd?: string): Promise<Json> {
	const { promise, resolve, reject } = Promise.withResolvers<Json>();
	const child = spawn(opsBin(), ["--json", "--manager", "user", ...(cwd ? ["--cwd", cwd] : []), ...argv], {
		stdio: ["ignore", "pipe", "pipe"],
	});
	let stdout = "";
	let stderr = "";
	child.stdout.setEncoding("utf8");
	child.stderr.setEncoding("utf8");
	child.stdout.on("data", (c: string) => {
		stdout += c;
	});
	child.stderr.on("data", (c: string) => {
		stderr += c;
	});
	child.on("error", reject);
	child.on("close", (code) => {
		const line = stdout.trim().split("\n").pop() ?? "";
		if (!line) {
			reject(new Error(stderr.trim() || `systemd-ops exited ${code ?? "null"}`));
			return;
		}
		try {
			resolve(JSON.parse(line) as Json);
		} catch {
			reject(new Error(`systemd-ops produced non-json: ${line}`));
		}
	});
	return promise;
}

function envelopeResult(env: Json) {
	if (env.ok === true) {
		const data = env.data;
		return {
			content: [{ type: "text" as const, text: JSON.stringify(data, null, 2) }],
			details: data,
		};
	}
	const err = (env.error ?? {}) as Json;
	const message = typeof err.message === "string" ? err.message : JSON.stringify(env);
	return textResult(message, { isError: true, details: env });
}

function specFromParams(params: Json): Json {
	if (params.spec && typeof params.spec === "object") return params.spec as Json;
	return pick(params, SPEC_KEYS);
}

function tokenOf(params: Json): string | undefined {
	if (typeof params.plan_token === "string" && params.plan_token.length > 0) return params.plan_token;
	if (typeof params.plan === "string" && params.plan.includes(".")) return params.plan;
	return undefined;
}

export default function systemdTools(pi: { cwd?: string }) {
	const factoryCwd = pi?.cwd;

	return [
		{
			name: "systemd_inspect",
			label: "systemd inspect",
			hidden: false as const,
			loadMode: "discoverable" as const,
			approval: "read" as const,
			description:
				"Read systemd state. Inspect first: list_operations / get_operation for write-prefix stems, " +
				"scope_show for the nearest .systemd-ops.toml, then list_units, failed_units, get_unit, " +
				"list_timers, list_unit_files, unit_dependencies, unit_logs. Never apply, never plan. " +
				"Do not shell systemctl for these units.",
			parameters: {
				type: "object",
				required: ["action"],
				properties: {
					action: {
						type: "string",
						enum: [...INSPECT_ACTIONS],
						description: "Read-only systemd-ops inspect action",
					},
					pattern: { type: "string", description: "Glob over unit or operation name" },
					unit: { type: "string", description: "Unit or operation stem" },
					state: {
						type: "string",
						enum: ["active", "inactive", "failed", "activating", "deactivating"],
					},
					lines: { type: "integer", description: "unit_logs entry count, 1..1000" },
					since: { type: "string", description: "unit_logs start, journalctl syntax" },
					until: { type: "string", description: "unit_logs end, journalctl syntax" },
					priority: { type: "integer", description: "unit_logs syslog priority 0..7" },
					grep: { type: "string", description: "unit_logs message regexp" },
					boot: { type: "integer", description: "unit_logs boot offset (0 current, -1 previous)" },
				},
			},
			async execute(_id: string, params: Json) {
				const action = String(params?.action ?? "");
				if (!(INSPECT_ACTIONS as readonly string[]).includes(action)) {
					return textResult("systemd_inspect requires a read action.", { isError: true });
				}
				const args = pick(params, [
					"pattern",
					"unit",
					"state",
					"lines",
					"since",
					"until",
					"priority",
					"grep",
					"boot",
				]);
				try {
					return envelopeResult(await runOps(inspectArgv(action, args)));
				} catch (e) {
					return textResult(e instanceof Error ? e.message : String(e), { isError: true });
				}
			},
		},
		{
			name: "systemd_control",
			label: "systemd control",
			approval: "write" as const,
			description:
				"Mutate running or enablement state of write-prefix units (start/stop/restart/reload/enable/disable/reset-failed). " +
				"Does not create or delete unit files. action=plan then action=apply with that plan_token. " +
				"Inspect first. Never shell systemctl for these units.",
			parameters: {
				type: "object",
				required: ["action"],
				properties: {
					action: {
						type: "string",
						enum: ["plan", "apply"],
						description: "plan a lifecycle change, or apply a control plan_token",
					},
					unit: { type: "string", description: "Unit name, e.g. managed-mail-check.service" },
					lifecycle: {
						type: "string",
						enum: [...CONTROL_LIFECYCLE],
						description: "plan: start|stop|restart|reload|enable|disable|reset-failed",
					},
					plan_token: { type: "string", description: "apply: sealed token from systemd_control plan" },
					context: {
						type: "object",
						properties: { cwd: { type: "string" } },
					},
				},
			},
			async execute(_id: string, params: Json, _onUpdate: unknown, ctx: SessionLike) {
				const action = String(params?.action ?? "");
				const cwd = sessionCwd(ctx, factoryCwd);
				try {
					if (action === "plan") {
						const unit = params.unit;
						const lifecycle = params.lifecycle;
						if (typeof unit !== "string" || typeof lifecycle !== "string") {
							return textResult("plan requires unit and lifecycle.", { isError: true });
						}
						if (!(CONTROL_LIFECYCLE as readonly string[]).includes(lifecycle)) {
							return textResult(`unknown lifecycle '${lifecycle}'`, { isError: true });
						}
						return envelopeResult(
							await runOps(["control", "plan", "--action", lifecycle, "--unit", unit], cwd),
						);
					}
					if (action === "apply") {
						const token = tokenOf(params);
						if (!token) {
							return textResult("apply requires plan_token.", { isError: true });
						}
						return envelopeResult(await runOps(["control", "apply", "--plan-token", token], cwd));
					}
					return textResult("systemd_control action is plan or apply.", { isError: true });
				} catch (e) {
					return textResult(e instanceof Error ? e.message : String(e), { isError: true });
				}
			},
		},
		{
			name: "systemd_author",
			label: "systemd author",
			approval: "write" as const,
			description:
				"Create, update, or retire systemd-ops-managed definitions (# managed: systemd-ops 1). " +
				"Refuses unmarked and out-of-prefix units. action=plan_create|plan_update|plan_retire then apply. " +
				"Inspect first. Never shell systemctl for these units.",
			parameters: {
				type: "object",
				required: ["action"],
				properties: {
					action: {
						type: "string",
						enum: ["plan_create", "plan_update", "plan_retire", "apply"],
						description: "author a definition, or apply an author plan_token",
					},
					unit: { type: "string", description: "Stem without suffix, e.g. managed-mail-check" },
					kind: { type: "string", enum: ["simple", "oneshot", "oneshot-linger"] },
					title: { type: "string" },
					purpose: { type: "string" },
					tags: { type: "array", items: { type: "string" } },
					description: { type: "string" },
					exec: {
						type: "object",
						properties: {
							path: { type: "string" },
							argv: { type: "array", items: { type: "string" } },
						},
					},
					cwd: { type: "string" },
					env: { type: "object", additionalProperties: { type: "string" } },
					path: { type: "array", items: { type: "string" } },
					environment_files: { type: "array", items: { type: "string" } },
					after: { type: "array", items: { type: "string" } },
					wants_network_online: { type: "boolean" },
					restart: { type: "string", enum: ["no", "on-failure", "always"] },
					nice: { type: "integer" },
					schedule: { type: "object" },
					enabled: { type: "boolean" },
					start_now: { type: "boolean" },
					spec: { type: "object", description: "OperationSpec v1; alternative to flat fields" },
					plan_token: { type: "string", description: "apply: sealed token from systemd_author plan" },
					context: {
						type: "object",
						properties: { cwd: { type: "string" } },
					},
				},
			},
			async execute(_id: string, params: Json, _onUpdate: unknown, ctx: SessionLike) {
				const action = String(params?.action ?? "");
				const cwd = sessionCwd(ctx, factoryCwd);
				try {
					if (action === "plan_create" || action === "plan_update") {
						const spec = specFromParams(params);
						if (typeof spec.unit !== "string") {
							return textResult(`${action} requires spec.unit (stem without suffix).`, {
								isError: true,
							});
						}
						const verb = action === "plan_create" ? "plan-create" : "plan-update";
						return envelopeResult(
							await runOps(["author", verb, "--spec", JSON.stringify(spec)], cwd),
						);
					}
					if (action === "plan_retire") {
						const unit = params.unit ?? (params.spec as Json | undefined)?.unit;
						if (typeof unit !== "string") {
							return textResult("plan_retire requires unit (stem without suffix).", {
								isError: true,
							});
						}
						return envelopeResult(await runOps(["author", "plan-retire", "--unit", unit], cwd));
					}
					if (action === "apply") {
						const token = tokenOf(params);
						if (!token) {
							return textResult("apply requires plan_token.", { isError: true });
						}
						return envelopeResult(await runOps(["author", "apply", "--plan-token", token], cwd));
					}
					return textResult(
						"systemd_author action is plan_create, plan_update, plan_retire, or apply.",
						{ isError: true },
					);
				} catch (e) {
					return textResult(e instanceof Error ? e.message : String(e), { isError: true });
				}
			},
		},
	];
}
