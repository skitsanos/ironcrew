import { describe, expect, test } from "bun:test";
import { copyFile, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

async function runPreflight(options: { psql: boolean; dirty?: boolean }) {
  const root = await mkdtemp(join(tmpdir(), "ironcrew-pre-push-preflight-"));
  const bin = join(root, "bin");
  const scripts = join(root, "scripts");
  const trace = join(root, "trace");
  try {
    await Promise.all([mkdir(bin), mkdir(scripts)]);
    await copyFile(
      join(import.meta.dir, "../pre-push-check.sh"),
      join(scripts, "pre-push-check.sh"),
    );
    await symlink("/usr/bin/dirname", join(bin, "dirname"));
    await writeFile(join(bin, "git"), `#!/bin/bash
printf 'git %s\n' "$*" >> "$PREFLIGHT_TRACE"
case "$*" in
  'rev-parse --local-env-vars') ;;
  'status --porcelain=v1 --untracked-files=all'|'status --short')
    if [[ "$PREFLIGHT_DIRTY" == 1 ]]; then printf '?? fixture\n'; fi
    ;;
  *) exit 91 ;;
esac
`, { mode: 0o755 });
    // Expose only fake prerequisites, even when the host has psql installed.
    const commands = ["python3", "bun", "cargo", "rustc", "rustup", "actionlint"];
    if (options.psql) commands.push("psql");
    await Promise.all(commands.map((command) => writeFile(join(bin, command), `#!/bin/bash
printf 'unexpected %s\n' "$0" >> "$PREFLIGHT_TRACE"
exit 92
`, { mode: 0o755 })));
    // Stop at the refresh boundary without changing tools or dependencies.
    await writeFile(join(scripts, "refresh-develop-dependencies.sh"), `#!/bin/bash
printf 'refresh\n' >> "$PREFLIGHT_TRACE"
exit 90
`, { mode: 0o755 });

    const child = Bun.spawn(["/bin/bash", join(scripts, "pre-push-check.sh")], {
      cwd: root,
      env: {
        PATH: bin,
        PREFLIGHT_TRACE: trace,
        PREFLIGHT_DIRTY: options.dirty ? "1" : "0",
      },
      stdout: "pipe",
      stderr: "pipe",
    });
    const [exitCode, stdout, stderr] = await Promise.all([
      child.exited,
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
    ]);
    return { exitCode, stdout, stderr, trace: (await readFile(trace, "utf8")).trim().split("\n") };
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

describe("pre-push command preflight", () => {
  for (const dirty of [false, true]) {
    test(`missing psql fails before refresh or worktree checks (${dirty ? "dirty" : "clean"} checkout)`, async () => {
      const result = await runPreflight({ psql: false, dirty });
      expect(result.exitCode).toBe(1);
      expect(result.stderr).toContain("pre-push: required command not found: psql");
      expect(result.stderr).toContain("Install the PostgreSQL client (psql)");
      expect(result.stderr).toContain("add its bin directory to PATH");
      expect(result.stdout).toBe("");
      expect(result.trace).toEqual(["git rev-parse --local-env-vars"]);
    });
  }

  test("psql on PATH admits a clean checkout without running the client", async () => {
    const result = await runPreflight({ psql: true });
    expect(result.exitCode).toBe(90);
    expect(result.stderr).toBe("");
    expect(result.stdout).toContain("pre-push: running latest-stable tool and dependency refresh");
    expect(result.trace).toEqual([
      "git rev-parse --local-env-vars",
      "git status --porcelain=v1 --untracked-files=all",
      "refresh",
    ]);
  });
});
