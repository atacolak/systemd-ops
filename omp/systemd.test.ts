import { afterEach, expect, test } from "bun:test";
import systemdTools, {
	automationArgv,
	automationAuthorArgv,
	inspectArgv,
	operatorArgv,
	opsCliArgv,
	opsSpawnContract,
	serializeAgentDefinition,
	sessionCwd,
} from "./systemd.ts";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const originalOpsBin = process.env.SYSTEMD_OPS_BIN;

afterEach(() => {
	if (originalOpsBin === undefined) delete process.env.SYSTEMD_OPS_BIN;
	else process.env.SYSTEMD_OPS_BIN = originalOpsBin;
});

async function fakeAgentAuthorScope(): Promise<{ root: string; scope: string }> {
	const root = await fs.mkdtemp(join(tmpdir(), "systemd-ops-agent-author-"));
	const scope = join(root, "scope");
	const agentRoot = join(root, "agents");
	const bin = join(root, "systemd-ops");
	await fs.mkdir(scope, { recursive: true });
	await fs.writeFile(
		bin,
		`#!/bin/sh\nprintf '%s\\n' '${JSON.stringify({ ok: true, data: { automation: { agent_root: agentRoot } } })}'\n`,
		{ mode: 0o755 },
	);
	process.env.SYSTEMD_OPS_BIN = bin;
	return { root, scope };
}

type AgentAuthorResult = {
	isError?: boolean;
	content: Array<{ text: string }>;
	details?: Record<string, unknown>;
};

type AgentAuthorExecute = (
	id: string,
	params: Record<string, unknown>,
	update?: unknown,
	ctx?: { cwd?: string },
) => Promise<AgentAuthorResult>;

function detailsRecord(result: AgentAuthorResult): Record<string, unknown> {
	if (!result.details) throw new Error("missing tool details");
	return result.details;
}

test("sessionCwd prefers session over factory", () => {
	expect(sessionCwd({ cwd: "/project/speech-core" }, "/factory")).toBe(
		"/project/speech-core",
	);
	expect(sessionCwd({}, "/factory")).toBe("/factory");
	expect(sessionCwd(undefined, undefined)).toBeUndefined();
});

test("scope_show inspect argv is scope show", () => {
	expect(inspectArgv("scope_show", {})).toEqual(["scope", "show"]);
});

test("operator_show inspect argv", () => {
	expect(inspectArgv("operator_show", { unit: "managed-personal-x" })).toEqual([
		"operator",
		"show",
		"--unit",
		"managed-personal-x",
	]);
});

test("scope_show keeps the PR worktree as process cwd and --cwd", () => {
	const worktree = "/project/speech-core";
	const factory = "/home/sf/workspace";
	const cwd = sessionCwd({ cwd: worktree }, factory);
	expect(cwd).toBe(worktree);
	expect(cwd).not.toBe(factory);
	const contract = opsSpawnContract(inspectArgv("scope_show", {}), cwd);
	expect(contract.args).toEqual([
		"--json",
		"--manager",
		"user",
		"--cwd",
		worktree,
		"scope",
		"show",
	]);
	expect(contract.options.cwd).toBe(worktree);
});

test("spawn contract naturally inherits the central scope environment", () => {
	const worktree = "/worktrees/pr-123";
	const contract = opsSpawnContract(
		inspectArgv("operator_show", { unit: "managed-omp-pr-maintainer" }),
		worktree,
	);
	expect(contract.args).toEqual([
		"--json",
		"--manager",
		"user",
		"--cwd",
		worktree,
		"operator",
		"show",
		"--unit",
		"managed-omp-pr-maintainer",
	]);
	expect(contract.options.cwd).toBe(worktree);
	expect(Object.prototype.hasOwnProperty.call(contract.options, "env")).toBe(false);
});

test("without session or factory cwd, opsCliArgv omits --cwd", () => {
	const argv = opsCliArgv(inspectArgv("scope_show", {}), sessionCwd(undefined, undefined));
	expect(argv).not.toContain("--cwd");
});

test("operator set/append/clear argv", () => {
	expect(
		operatorArgv("set", {
			unit: "managed-omp-pr-maintainer",
			about: "Maintains PRs",
			headline: "waiting",
			body: "body",
		}),
	).toEqual([
		"operator",
		"set",
		"--unit",
		"managed-omp-pr-maintainer",
		"--about",
		"Maintains PRs",
		"--headline",
		"waiting",
		"--body",
		"body",
	]);
	expect(
		operatorArgv("append", {
			unit: "managed-omp-pr-maintainer",
			text: "found 2 review threads",
		}),
	).toEqual([
		"operator",
		"append",
		"--unit",
		"managed-omp-pr-maintainer",
		"--text",
		"found 2 review threads",
	]);
	expect(operatorArgv("clear", { unit: "managed-omp-pr-maintainer" })).toEqual([
		"operator",
		"clear",
		"--unit",
		"managed-omp-pr-maintainer",
	]);
});

test("operator argv forwards session cwd", () => {
	const cwd = sessionCwd({ cwd: "/home/sf/worlds/personal" }, "/factory");
	const argv = opsCliArgv(
		operatorArgv("append", { unit: "managed-personal-x", text: "hi" }),
		cwd,
	);
	expect(argv.slice(0, 4)).toEqual(["--json", "--manager", "user", "--cwd"]);
	expect(argv).toContain("/home/sf/worlds/personal");
	expect(argv).toContain("operator");
	expect(argv).not.toContain("systemctl");
});

test("bound automation argv never accepts a unit", () => {
	expect(automationArgv("context", { unit: "managed-other" })).toEqual([
		"automation",
		"context",
	]);
	expect(
		automationArgv("report", {
			unit: "managed-other",
			headline: "waiting for review",
			summary: ["all actionable feedback is addressed"],
		}),
	).toEqual([
		"automation",
		"report",
		"--headline",
		"waiting for review",
		"--summary",
		'["all actionable feedback is addressed"]',
	]);
	expect(automationArgv("activity", { unit: "managed-other", text: "requested review" })).toEqual([
		"automation",
		"activity",
		"--text",
		"requested review",
	]);
});

test("autonomous tools are strict bound surfaces", () => {
	const tools = systemdTools({ cwd: "/worktrees/pr-123" });
	const byName = new Map(tools.map((tool) => [tool.name, tool]));
	for (const name of ["automation_context", "automation_report", "automation_activity"]) {
		const tool = byName.get(name);
		expect(tool).toBeDefined();
		expect(tool?.hidden).toBe(true);
		expect(tool?.loadMode).toBe("essential");
		expect(tool?.strict).toBe(true);
		expect(JSON.stringify(tool?.parameters)).not.toContain("unit");
	}
	expect(byName.get("automation_context")?.approval).toBe("read");
	expect(byName.get("automation_report")?.approval).toBe("write");
	expect(byName.get("automation_activity")?.approval).toBe("write");
});

test("broad tools state their capability audiences", () => {
	const tools = systemdTools({});
	const byName = new Map(tools.map((tool) => [tool.name, tool.description]));
	expect(byName.get("systemd_inspect")).toContain("Project builder, operator, and admin");
	expect(byName.get("systemd_control")).toContain("Trusted project operator and admin");
	expect(byName.get("systemd_author")).toContain("Automation and system builder");
	expect(byName.get("systemd_operator")).toContain("Low-level manual operator-state");
});

test("automation author argv carries typed metadata", () => {
	expect(automationAuthorArgv("plan_create", {
		unit: "managed-omp-pr-9969",
		title: "PR 9969",
		agent: "pr-maintainer",
		brain_paths: [".systemd-ops/pr-maintainer-run"],
	})).toEqual([
		"automation",
		"plan-create",
		"--spec",
		JSON.stringify({
			unit: "managed-omp-pr-9969",
			title: "PR 9969",
			agent: "pr-maintainer",
			brain_paths: [".systemd-ops/pr-maintainer-run"],
		}),
	]);
	expect(automationAuthorArgv("plan_complete", {
		unit: "managed-omp-pr-9969",
		reason: "merged upstream",
	})).toEqual([
		"automation", "plan-complete", "--unit", "managed-omp-pr-9969", "--reason", "merged upstream",
	]);
});

test("agent author serialization uses canonical OMP field names", () => {
	const text = serializeAgentDefinition({
		name: "pr-maintainer",
		description: "Maintains one PR",
		hide: true,
		tools: ["github", "automation_context"],
		thinkingLevel: "high",
		readSummarize: false,
		autoloadSkills: ["hcom"],
		advisor: false,
		systemPrompt: "maintain exactly one PR.",
	});
	expect(text).toContain("thinkingLevel: high");
	expect(text).toContain("readSummarize: false");
	expect(text).not.toContain("thinking-level");
	expect(text).not.toContain("read-summarize");
	expect(text).toContain("maintain exactly one PR.");
});

test("privileged automation author tools are hidden", () => {
	const tools = systemdTools({});
	const byName = new Map(tools.map((tool) => [tool.name, tool]));
	expect(byName.get("automation_agent_author")?.hidden).toBe(true);
	expect(byName.get("automation_author")?.hidden).toBe(true);
	expect(byName.get("automation_context")?.hidden).toBe(true);
});

test("agent author create update list inspect and retire use configured root", async () => {
	const { root, scope } = await fakeAgentAuthorScope();
	try {
		const tool = systemdTools({ cwd: scope }).find((candidate) => candidate.name === "automation_agent_author");
		expect(tool).toBeDefined();
		const execute = tool!.execute as AgentAuthorExecute;
		const definition = {
			name: "proof-agent",
			description: "proof agent",
			hide: true,
			tools: ["read"],
			thinkingLevel: "high",
			readSummarize: false,
			systemPrompt: "perform the proof.",
		};
		const created = await execute("create", { action: "create", ...definition }, undefined, { cwd: scope });
		expect(created.isError).toBeUndefined();
		const path = join(root, "agents", ".omp", "agents", "proof-agent.md");
		expect(await fs.readFile(path, "utf8")).toContain("perform the proof.");

		const listed = await execute("list", { action: "list" }, undefined, { cwd: scope });
		expect(detailsRecord(listed).agents).toEqual(["proof-agent"]);
		const inspected = await execute("inspect", { action: "inspect", name: "proof-agent" }, undefined, { cwd: scope });
		expect(detailsRecord(inspected).path).toBe(path);

		const updated = await execute(
			"update",
			{ action: "update", ...definition, systemPrompt: "perform the updated proof." },
			undefined,
			{ cwd: scope },
		);
		expect(updated.isError).toBeUndefined();
		expect(await fs.readFile(path, "utf8")).toContain("updated proof");

		const retired = await execute("retire", { action: "retire", name: "proof-agent" }, undefined, { cwd: scope });
		expect(detailsRecord(retired).retired).toBe(true);
		expect(await fs.lstat(path).then(() => true).catch(() => false)).toBe(false);
	} finally {
		await fs.rm(root, { recursive: true, force: true });
	}
});

test("agent author refuses path escape and writable symlink", async () => {
	const { root, scope } = await fakeAgentAuthorScope();
	try {
		const tool = systemdTools({ cwd: scope }).find((candidate) => candidate.name === "automation_agent_author");
		expect(tool).toBeDefined();
		const execute = tool!.execute as AgentAuthorExecute;
		const escaped = await execute("escape", { action: "inspect", name: "../escape" }, undefined, { cwd: scope });
		expect(escaped.isError).toBe(true);
		const agents = join(root, "agents", ".omp", "agents");
		await fs.mkdir(agents, { recursive: true });
		const target = join(root, "target.md");
		await fs.writeFile(target, "keep");
		await fs.symlink(target, join(agents, "proof-agent.md"));
		const refused = await execute(
			"update",
			{
				action: "update",
				name: "proof-agent",
				description: "proof agent",
				systemPrompt: "do not overwrite the symlink.",
			},
			undefined,
			{ cwd: scope },
		);
		expect(refused.isError).toBe(true);
		expect(refused.content[0].text).toContain("refusing writable symlink");
		expect(await fs.readFile(target, "utf8")).toBe("keep");
	} finally {
		await fs.rm(root, { recursive: true, force: true });
	}
});
