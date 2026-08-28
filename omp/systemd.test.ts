import { expect, test } from "bun:test";
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
