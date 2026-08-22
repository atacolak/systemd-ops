import { expect, test } from "bun:test";
import { inspectArgv, opsCliArgv, sessionCwd } from "./systemd.ts";

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
