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
import { promises as fs } from "node:fs";
import { dirname, join, resolve, sep } from "node:path";
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
	"operator_show",
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

const AUTOMATION_SPEC_KEYS = [...SPEC_KEYS, "agent", "parent", "brain_paths"] as const;

const AGENT_FIELDS = [
	"name",
	"description",
	"hide",
	"tools",
	"model",
	"thinkingLevel",
	"readSummarize",
	"autoloadSkills",
	"spawns",
	"advisor",
	"systemPrompt",
] as const;

function validateAgentName(name: string): void {
	if (!/^[a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?$/.test(name)) {
		throw new Error("agent name must be 1..64 lowercase letters, digits, or non-edge hyphens");
	}
}

function validateAgentDefinition(input: Json): Json {
	const unknown = Object.keys(input).filter(
		(key) => key !== "action" && !(AGENT_FIELDS as readonly string[]).includes(key),
	);
	if (unknown.length > 0) throw new Error(`unknown agent fields: ${unknown.join(", ")}`);
	const name = String(input.name ?? "");
	validateAgentName(name);
	if (typeof input.description !== "string" || input.description.trim().length === 0) {
		throw new Error("agent description is required");
	}
	if (typeof input.systemPrompt !== "string" || input.systemPrompt.trim().length === 0) {
		throw new Error("agent systemPrompt is required");
	}
	for (const key of ["tools", "autoloadSkills", "spawns"] as const) {
		const value = input[key];
		if (value !== undefined && (!Array.isArray(value) || value.some((item) => typeof item !== "string"))) {
			throw new Error(`${key} must be an array of strings`);
		}
	}
	for (const key of ["hide", "readSummarize", "advisor"] as const) {
		if (input[key] !== undefined && typeof input[key] !== "boolean") {
			throw new Error(`${key} must be boolean`);
		}
	}
	return pick(input, AGENT_FIELDS);
}

export function serializeAgentDefinition(input: Json): string {
	const agent = validateAgentDefinition(input);
	const frontmatter = pick(agent, AGENT_FIELDS.filter((key) => key !== "systemPrompt"));
	const yaml = Bun.YAML.stringify(frontmatter).trimEnd();
	return `---\n${yaml}\n---\n\n${String(agent.systemPrompt).trim()}\n`;
}

async function scopeAgentRoot(cwd?: string): Promise<string> {
	const env = await runOps(["scope", "show"], cwd);
	if (env.ok !== true) throw new Error(String((env.error as Json | undefined)?.message ?? "scope show failed"));
	const root = (env.data as Json | undefined)?.automation;
	const value = root && typeof root === "object" ? (root as Json).agent_root : undefined;
	if (typeof value !== "string" || value.length === 0) {
		throw new Error("scope has no [automation].agent_root configuration");
	}
	return value;
}

function containedAgentPath(root: string, name: string): string {
	validateAgentName(name);
	const base = resolve(root, ".omp", "agents");
	const target = resolve(base, `${name}.md`);
	if (!target.startsWith(`${base}${sep}`)) throw new Error("agent path escapes configured root");
	return target;
}

async function refuseWritableSymlink(path: string): Promise<void> {
	try {
		if ((await fs.lstat(path)).isSymbolicLink()) throw new Error(`refusing writable symlink ${path}`);
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
	}
}

async function atomicAgentWrite(path: string, content: string): Promise<void> {
	await fs.mkdir(dirname(path), { recursive: true });
	await refuseWritableSymlink(path);
	const temporary = join(dirname(path), `.${path.split(sep).pop()}.tmp.${process.pid}.${crypto.randomUUID()}`);
	await fs.writeFile(temporary, content, { mode: 0o600, flag: "wx" });
	await fs.rename(temporary, path);
}

export async function agentRootFromScope(cwd?: string): Promise<string> {
	return scopeAgentRoot(cwd);
}

export function automationAuthorArgv(action: string, params: Json): string[] {
	switch (action) {
		case "inspect":

			return ["automation", "inspect", "--unit", String(params.unit ?? "")];
		case "plan_create":
		case "plan_update": {
			const spec = params.spec && typeof params.spec === "object"
				? (params.spec as Json)
				: pick(params, AUTOMATION_SPEC_KEYS);
			return [
				"automation",
				action === "plan_create" ? "plan-create" : "plan-update",
				"--spec",
				JSON.stringify(spec),
			];
		}
		case "plan_complete":
			return ["automation", "plan-complete", "--unit", String(params.unit ?? ""), "--reason", String(params.reason ?? "")];
		case "plan_retire":
			return ["automation", "plan-retire", "--unit", String(params.unit ?? "")];
		case "apply":
			return ["automation", "apply", "--plan-token", String(tokenOf(params) ?? "")];
		default:
			throw new Error(`unknown automation author action '${action}'`);
	}
}

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

export function sessionCwd(ctx: SessionLike | undefined, fallback: string | undefined): string | undefined {
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

export function inspectArgv(action: string, args: Json): string[] {
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
		case "operator_show":
			argv.push("operator", "show", "--unit", String(args.unit ?? ""));
			break;
		default:
			throw new Error(`unknown inspect action '${action}'`);
	}
	return argv;
}

export function operatorArgv(action: string, args: Json): string[] {
	const unit = String(args.unit ?? "");
	switch (action) {
		case "set": {
			const argv = ["operator", "set", "--unit", unit];
			if (typeof args.about === "string") argv.push("--about", args.about);
			if (typeof args.headline === "string") argv.push("--headline", args.headline);
			if (typeof args.body === "string") argv.push("--body", args.body);
			return argv;
		}
		case "append":
			return ["operator", "append", "--unit", unit, "--text", String(args.text ?? "")];
		case "clear":
			return ["operator", "clear", "--unit", unit];
		default:
			throw new Error(`unknown operator action '${action}'`);
	}
}

export function automationArgv(action: string, args: Json): string[] {
	switch (action) {
		case "context":
			return ["automation", "context"];
		case "report":
			return [
				"automation",
				"report",
				"--headline",
				String(args.headline ?? ""),
				"--summary",
				JSON.stringify(args.summary ?? []),
			];
		case "activity":
			return ["automation", "activity", "--text", String(args.text ?? "")];
		default:
			throw new Error(`unknown automation action '${action}'`);
	}
}

export function opsCliArgv(argv: string[], cwd?: string): string[] {
	return ["--json", "--manager", "user", ...(cwd ? ["--cwd", cwd] : []), ...argv];
}
export function opsSpawnContract(argv: string[], cwd?: string) {
	return {
		command: opsBin(),
		args: opsCliArgv(argv, cwd),
		options: {
			...(cwd ? { cwd } : {}),
			stdio: ["ignore", "pipe", "pipe"] as ["ignore", "pipe", "pipe"],
		},
	};
}

function runOps(argv: string[], cwd?: string): Promise<Json> {
	const { promise, resolve, reject } = Promise.withResolvers<Json>();
	const contract = opsSpawnContract(argv, cwd);
	const child = spawn(contract.command, contract.args, contract.options);
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
				"Project builder, operator, and admin read surface for general systemd visibility. " +
				"Inspect operations, scopes, units, timers, dependencies, logs, and low-level operator state. " +
				"Ordinary autonomous runtime maintainers should use automation_context instead. Never apply or plan.",
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
			async execute(_id: string, params: Json, _onUpdate?: unknown, ctx?: SessionLike) {
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
				const cwd = sessionCwd(ctx, factoryCwd);
				try {
					return envelopeResult(await runOps(inspectArgv(action, args), cwd));
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
				"Trusted project operator and admin lifecycle surface. Plan and apply start, stop, restart, reload, " +
				"enable, disable, or reset-failed for write-prefix units. Do not expose this to ordinary runtime " +
				"maintainers or delegated workers. Does not create or delete unit files.",
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
				"Automation and system builder low-level managed definition authoring surface for project leads and admins. " +
				"Creates, updates, or retires systemd OperationSpec definitions only. Prefer automation_author " +
				"for agent-backed instances. Never expose either authoring surface to ordinary runtime maintainers " +
				"or delegated workers. Refuses unmarked and out-of-prefix units; use plan then apply.",
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
		{
			name: "systemd_operator",
			label: "systemd operator",
			approval: "write" as const,
			description:
				"Low-level manual operator-state administration and compatibility surface for project operators and admins. " +
				"Ordinary autonomous runtime maintainers should use automation_report and automation_activity. " +
				"Mutates advisory operator state, never systemd definitions or objective health.",
			parameters: {
				type: "object",
				required: ["action", "unit"],
				properties: {
					action: {
						type: "string",
						enum: ["set", "append", "clear"],
						description: "set brief fields, append an activity line, or clear the note",
					},
					unit: {
						type: "string",
						description: "Owned operation stem, e.g. managed-personal-youtube-poll",
					},
					about: { type: "string", description: "set: stable what/why text" },
					headline: { type: "string", description: "set: short current headline" },
					body: { type: "string", description: "set: current reconsolidated understanding" },
					text: { type: "string", description: "append: one semantic activity line" },
				},
			},
			async execute(_id: string, params: Json, _onUpdate: unknown, ctx: SessionLike) {
				const action = String(params?.action ?? "");
				const unit = params.unit;
				if (typeof unit !== "string" || unit.length === 0) {
					return textResult("systemd_operator requires unit (owned stem).", { isError: true });
				}
				if (action === "set") {
					const hasField =
						typeof params.about === "string" ||
						typeof params.headline === "string" ||
						typeof params.body === "string";
					if (!hasField) {
						return textResult("set requires at least one of about, headline, body.", {
							isError: true,
						});
					}
				} else if (action === "append") {
					if (typeof params.text !== "string" || params.text.length === 0) {
						return textResult("append requires text.", { isError: true });
					}
				} else if (action !== "clear") {
					return textResult("systemd_operator action is set, append, or clear.", { isError: true });
				}
				const cwd = sessionCwd(ctx, factoryCwd);
				try {
					return envelopeResult(await runOps(operatorArgv(action, params), cwd));
				} catch (e) {
					return textResult(e instanceof Error ? e.message : String(e), { isError: true });
				}
			},
		},
		{
			name: "automation_agent_author",
			label: "automation agent author",
			hidden: true as const,
			approval: "write" as const,
			strict: true as const,
			description:
				"Privileged explicit-only authoring surface for reusable OMP agent definitions under the current " +
				"scope's configured automation.agent_root. The configured root is authoritative; no destination path is accepted.",
			parameters: {
				type: "object",
				additionalProperties: false,
				required: ["action"],
				properties: {
					action: { type: "string", enum: ["inspect", "list", "create", "update", "retire"] },
					name: { type: "string" },
					description: { type: "string" },
					hide: { type: "boolean" },
					tools: { type: "array", items: { type: "string" } },
					model: { type: "string" },
					thinkingLevel: { type: "string" },
					readSummarize: { type: "boolean" },
					autoloadSkills: { type: "array", items: { type: "string" } },
					spawns: { type: "array", items: { type: "string" } },
					advisor: { type: "boolean" },
					systemPrompt: { type: "string" },
				},
			},
			async execute(_id: string, params: Json, _onUpdate?: unknown, ctx?: SessionLike) {
				try {
					const cwd = sessionCwd(ctx, factoryCwd);
					const root = await scopeAgentRoot(cwd);
					const action = String(params.action ?? "");
					if (action === "list") {
						const directory = resolve(root, ".omp", "agents");
						const entries = await fs.readdir(directory, { withFileTypes: true }).catch((error) => {
							if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
							throw error;
						});
						const agents = entries
							.filter((entry) => entry.isFile() && entry.name.endsWith(".md"))
							.map((entry) => entry.name.slice(0, -3))
							.sort();
						return textResult(JSON.stringify({ root, agents }, null, 2), { details: { root, agents } });
					}
					const name = String(params.name ?? "");
					const path = containedAgentPath(root, name);
					if (action === "inspect") {
						const content = await fs.readFile(path, "utf8");
						return textResult(content, { details: { root, name, path, content } });
					}
					if (action === "retire") {
						await refuseWritableSymlink(path);
						await fs.unlink(path);
						return textResult(JSON.stringify({ retired: true, name, path }, null, 2), { details: { retired: true, name, path } });
					}
					if (action !== "create" && action !== "update") {
						return textResult("action must be inspect, list, create, update, or retire.", { isError: true });
					}
					const exists = await fs.lstat(path).then(() => true).catch((error) => {
						if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
						throw error;
					});
					if (action === "create" && exists) throw new Error(`agent '${name}' already exists`);
					if (action === "update" && !exists) throw new Error(`agent '${name}' does not exist`);
					const content = serializeAgentDefinition(params);
					await atomicAgentWrite(path, content);
					return textResult(content, { details: { action, name, path, content } });
				} catch (e) {
					return textResult(e instanceof Error ? e.message : String(e), { isError: true });
				}
			},
		},
		{
			name: "automation_author",
			label: "automation author",
			hidden: true as const,
			approval: "write" as const,
			strict: true as const,
			description:
				"Privileged explicit-only automation instance author. Composes existing sealed systemd author plans " +
				"with generic agent metadata and completed lifecycle state. Not for ordinary runtime maintainers.",
			parameters: {
				type: "object",
				additionalProperties: false,
				required: ["action"],
				properties: {
					action: { type: "string", enum: ["inspect", "plan_create", "plan_update", "plan_complete", "plan_retire", "apply"] },
					unit: { type: "string" },
					title: { type: "string" },
					purpose: { type: "string" },
					tags: { type: "array", items: { type: "string" } },
					agent: { type: "string" },
					parent: { type: "string" },
					brain_paths: { type: "array", items: { type: "string" } },
					kind: { type: "string", enum: ["simple", "oneshot", "oneshot-linger"] },
					cwd: { type: "string" },
					exec: { type: "object" },
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
					reason: { type: "string" },
					spec: { type: "object" },
					plan_token: { type: "string" },
				},
			},
			async execute(_id: string, params: Json, _onUpdate?: unknown, ctx?: SessionLike) {
				try {
					const action = String(params.action ?? "");
					return envelopeResult(await runOps(automationAuthorArgv(action, params), sessionCwd(ctx, factoryCwd)));
				} catch (e) {
					return textResult(e instanceof Error ? e.message : String(e), { isError: true });
				}
			},
		},
		{
			name: "automation_context",
			label: "automation context",
			hidden: true as const,
			loadMode: "essential" as const,
			approval: "read" as const,
			strict: true as const,
			description:
				"Responsible autonomous-operation read surface. Return focused context for the operation bound by " +
				"SYSTEMD_OPS_SCOPE_ROOT and SYSTEMD_OPS_OPERATION: canonical identity, objective runtime, current " +
				"human report, active iteration, latest 20 iterations, and notable activity. No parameters. No raw journal.",
			parameters: { type: "object", additionalProperties: false, properties: {} },
			async execute(_id: string, _params: Json, _onUpdate?: unknown, ctx?: SessionLike) {
				try {
					return envelopeResult(await runOps(automationArgv("context", {}), sessionCwd(ctx, factoryCwd)));
				} catch (e) {
					return textResult(e instanceof Error ? e.message : String(e), { isError: true });
				}
			},
		},
		{
			name: "automation_report",
			label: "automation report",
			hidden: true as const,
			loadMode: "essential" as const,
			approval: "write" as const,
			strict: true as const,
			description:
				"Responsible autonomous-operation surface. Submit the current concise human-facing state for your " +
				"bound operation. This is an operator cockpit, not working memory: include only what a human needs " +
				"to understand what is happening, what materially changed, what remains, and whether attention is " +
				"needed. Omit noisy evidence, review inventories, commands, logs, and internal reasoning. Required before normal exit.",
			parameters: {
				type: "object",
				additionalProperties: false,
				required: ["headline", "summary"],
				properties: {
					headline: { type: "string", minLength: 1, maxLength: 80, pattern: "^[^\\r\\n]+$" },
					summary: {
						type: "array",
						minItems: 1,
						maxItems: 5,
						items: { type: "string", minLength: 1, maxLength: 280, pattern: "^[^\\r\\n]+$" },
					},
				},
			},
			async execute(_id: string, params: Json, _onUpdate?: unknown, ctx?: SessionLike) {
				try {
					return envelopeResult(await runOps(automationArgv("report", params), sessionCwd(ctx, factoryCwd)));
				} catch (e) {
					return textResult(e instanceof Error ? e.message : String(e), { isError: true });
				}
			},
		},
		{
			name: "automation_activity",
			label: "automation activity",
			hidden: true as const,
			loadMode: "essential" as const,
			approval: "write" as const,
			strict: true as const,
			description:
				"Record one notable human-facing milestone for your bound operation. Not every tool call, command, " +
				"poll, or log line. Omit routine no-change checks and internal reasoning. Optional per iteration.",
			parameters: {
				type: "object",
				additionalProperties: false,
				required: ["text"],
				properties: {
					text: { type: "string", minLength: 1, maxLength: 200, pattern: "^[^\\r\\n]+$" },
				},
			},
			async execute(_id: string, params: Json, _onUpdate?: unknown, ctx?: SessionLike) {
				try {
					return envelopeResult(await runOps(automationArgv("activity", params), sessionCwd(ctx, factoryCwd)));
				} catch (e) {
					return textResult(e instanceof Error ? e.message : String(e), { isError: true });
				}
			},
		},
	];
}
