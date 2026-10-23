import { describe, expect, test } from "bun:test";
import { copyFile, mkdir, mkdtemp, readFile, readdir, realpath, rm, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

type Fixture = { root: string; repository: string; base: string };
type TemporaryProbe = {
  TMPDIR: string;
  TEMP: string;
  TMP: string;
  child: string;
  mode: number;
  inheritedDotenv: boolean;
};

async function runPreflight(options: {
  psql: boolean;
  dirty?: boolean;
  temporaryRoot?: (fixture: Fixture) => Promise<string | undefined>;
}) {
  const root = await realpath(await mkdtemp(join(tmpdir(), "ironcrew-pre-push-preflight-")));
  const repository = join(root, "checkout");
  const base = join(root, "temporary files");
  const bin = join(root, "bin");
  const scripts = join(repository, "scripts");
  const trace = join(root, "trace");
  const receipt = join(root, "receipt.json");
  try {
    await Promise.all([mkdir(bin), mkdir(scripts, { recursive: true }), mkdir(base)]);
    await writeFile(join(base, "keep"), "caller-owned sibling\n");
    await Promise.all(["pre-push-check.sh", "pre_push_tempdir.py"].map((file) =>
      copyFile(join(import.meta.dir, "..", file), join(scripts, file))));
    await symlink("/usr/bin/dirname", join(bin, "dirname"));
    await symlink("/bin/rm", join(bin, "rm"));
    await symlink(Bun.which("python3")!, join(bin, "python3"));
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
    const commands = ["bun", "cargo", "rustc", "rustup", "actionlint"];
    if (options.psql) commands.push("psql");
    await Promise.all(commands.map((command) => writeFile(join(bin, command), `#!/bin/bash
printf 'unexpected %s\n' "$0" >> "$PREFLIGHT_TRACE"
exit 92
`, { mode: 0o755 })));
    // Stop at the refresh boundary without changing tools or dependencies.
    await writeFile(join(scripts, "refresh-develop-dependencies.sh"), `#!/bin/bash
printf 'refresh\n' >> "$PREFLIGHT_TRACE"
python3 -I - <<'PY'
import json, os, stat, tempfile
from pathlib import Path
with tempfile.TemporaryDirectory() as child:
    path = Path(child).resolve()
    probe = {key: os.environ[key] for key in ("TMPDIR", "TEMP", "TMP")}
    probe.update(child=str(path), mode=stat.S_IMODE(Path(os.environ["TMPDIR"]).stat().st_mode),
                 inheritedDotenv=any((parent / ".env").exists() for parent in (path, *path.parents)))
    Path(os.environ["PREFLIGHT_RECEIPT"]).write_text(json.dumps(probe))
PY
exit 90
`, { mode: 0o755 });

    const configuredTemp = options.temporaryRoot
      ? await options.temporaryRoot({ root, repository, base })
      : base;
    const child = Bun.spawn(["/bin/bash", join(scripts, "pre-push-check.sh")], {
      cwd: repository,
      env: {
        PATH: bin,
        TMPDIR: configuredTemp,
        // These unsafe inherited alternatives must be replaced, not used as fallbacks.
        TEMP: repository,
        TMP: repository,
        PREFLIGHT_TRACE: trace,
        PREFLIGHT_RECEIPT: receipt,
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
    const probe: TemporaryProbe | undefined = await Bun.file(receipt).exists()
      ? await Bun.file(receipt).json() : undefined;
    return {
      exitCode, stdout, stderr, probe, base,
      scratchRemains: probe ? await stat(probe.TMPDIR).then(() => true, (error) => {
        if (error.code === "ENOENT") return false;
        throw error;
      }) : false,
      baseEntries: await readdir(base),
      trace: (await readFile(trace, "utf8")).trim().split("\n"),
    };
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

describe("pre-push temporary-directory isolation", () => {
  const sentinel = "PREFLIGHT_SYNTHETIC_SECRET=must-never-be-loaded-or-printed\n";

  const unsafeRoots: Array<[string, NonNullable<Parameters<typeof runPreflight>[0]["temporaryRoot"]>]> = [
    ["checkout itself", async ({ repository }) => repository],
    ["ignored target subtree without .env", async ({ repository }) => {
      const target = join(repository, "target", "tmp");
      await mkdir(target, { recursive: true });
      return target;
    }],
    ["symlink into the checkout", async ({ root, repository }) => {
      const alias = join(root, "alias");
      await symlink(repository, alias);
      return alias;
    }],
    [".env in the base", async ({ base }) => {
      await writeFile(join(base, ".env"), sentinel);
      return base;
    }],
    [".env in an ancestor", async ({ root, base }) => {
      await writeFile(join(root, ".env"), sentinel);
      return base;
    }],
    ["symlink to a base with an ancestor .env", async ({ root, base }) => {
      await writeFile(join(root, ".env"), sentinel);
      const alias = join(root, "alias");
      await symlink(base, alias);
      return alias;
    }],
    ["dangling .env symlink", async ({ base }) => {
      await symlink(join(base, "absent"), join(base, ".env"));
      return base;
    }],
    ["missing directory", async ({ base }) => join(base, "absent")],
    ["regular file", async ({ base }) => join(base, "keep")],
    ["relative directory", async () => "../temporary files"],
    ["newline in directory name", async ({ base }) => {
      const path = join(base, "ambiguous\npath");
      await mkdir(path);
      return path;
    }],
  ];

  for (const [label, temporaryRoot] of unsafeRoots) {
    test(`rejects ${label} before refresh or worktree checks`, async () => {
      const result = await runPreflight({ psql: true, temporaryRoot });
      expect(result.exitCode).toBe(1);
      expect(result.stderr).toContain("pre-push: temporary-directory isolation failed:");
      expect(result.stderr).toContain("no fallback was attempted");
      expect(result.stdout).toBe("");
      expect(result.stdout + result.stderr).not.toContain(sentinel.trim());
      expect(result.trace).toEqual(["git rev-parse --local-env-vars"]);
      expect(result.probe).toBeUndefined();
      expect(result.baseEntries.filter((entry) => entry.startsWith("ironcrew-pre-push."))).toEqual([]);
    });
  }

  for (const alias of [false, true]) {
    test(`exports a private root to child temp APIs and cleans only that root (${alias ? "symlink" : "physical"} base)`, async () => {
      const result = await runPreflight({
        psql: true,
        temporaryRoot: async ({ root, repository, base }) => {
          // A checkout .env is normal; only temporary ancestry must be clean.
          await writeFile(join(repository, ".env"), sentinel);
          if (!alias) return base;
          const path = join(root, "safe alias");
          await symlink(base, path);
          return path;
        },
      });
      expect(result.exitCode).toBe(90);
      expect(result.stderr).toBe("");
      expect(result.probe?.TMPDIR).toStartWith(`${result.base}/ironcrew-pre-push.`);
      expect(result.probe?.TEMP).toBe(result.probe?.TMPDIR);
      expect(result.probe?.TMP).toBe(result.probe?.TMPDIR);
      expect(result.probe?.child).toStartWith(`${result.probe?.TMPDIR}/`);
      expect(result.probe?.mode).toBe(0o700);
      expect(result.probe?.inheritedDotenv).toBeFalse();
      expect(result.scratchRemains).toBeFalse();
      expect(result.baseEntries).toEqual(["keep"]);
    });
  }

  test("a dirty checkout also cleans the admitted private root", async () => {
    const result = await runPreflight({ psql: true, dirty: true });
    expect(result.exitCode).toBe(1);
    expect(result.stderr).toContain("reconcile every tracked and untracked change");
    expect(result.baseEntries).toEqual(["keep"]);
    expect(result.trace).not.toContain("refresh");
  });
});
