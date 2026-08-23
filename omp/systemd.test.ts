import { expect, test } from "bun:test";
import { inspectArgv, operatorArgv, opsCliArgv, sessionCwd } from "./systemd.ts";

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

test("scope_show uses session cwd when it differs from factory", () => {
	const session = "/project/speech-core";
	const factory = "/home/sf/workspace";
	const cwd = sessionCwd({ cwd: session }, factory);
	expect(cwd).toBe(session);
	expect(cwd).not.toBe(factory);
	const argv = opsCliArgv(inspectArgv("scope_show", {}), cwd);
	expect(argv).toContain("--cwd");
	expect(argv).toContain(session);
	expect(argv).not.toContain(factory);
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
