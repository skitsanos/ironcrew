import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { join } from "node:path";

const repository = join(import.meta.dir, "../..");
const reports = "evaluations/platform-canary/reports";

// Each assembly Dockerfile is retained evidence: the receipts record the exact
// bytes that assembled a historical canary image. Dependency refreshes must
// not rewrite them, or the receipts stop describing the tracked files.
const assemblyReceipts = [
  {
    dockerfile: `${reports}/ic007-v6-assembly.Dockerfile`,
    receipt: `${reports}/ic007-v6-assembly-context.json`,
    files: (receipt: any) => receipt.manifest.files,
  },
  {
    dockerfile: `${reports}/ic007-v7-assembly.Dockerfile`,
    receipt: `${reports}/ic007-v7-assembly-context.json`,
    files: (receipt: any) => receipt.manifest.files,
  },
  {
    dockerfile: "evaluations/platform-canary/ic008-openshift-assembly.Dockerfile",
    receipt: `${reports}/ic008-openshift-assembly-context.json`,
    files: (receipt: any) => receipt.files,
  },
];

const canaryAssemblyDockerfiles = [
  "evaluations/platform-canary/ic008-openshift-assembly.Dockerfile",
  `${reports}/ic007-openshift-v5-assembly.Dockerfile`,
  `${reports}/ic007-v6-assembly.Dockerfile`,
  `${reports}/ic007-v7-assembly.Dockerfile`,
];

// Renovate's `ignorePaths` is not mergeable, so the repository value replaces
// the `:ignoreModulesAndTests` list inherited through `config:best-practices`.
const renovatePresetIgnorePaths = [
  "**/node_modules/**",
  "**/bower_components/**",
  "**/vendor/**",
  "**/examples/**",
  "**/__tests__/**",
  "**/test/**",
  "**/tests/**",
  "**/__fixtures__/**",
];

async function sha256(path: string) {
  const bytes = await Bun.file(join(repository, path)).bytes();
  return createHash("sha256").update(bytes).digest("hex");
}

describe("platform canary evidence", () => {
  test("assembly Dockerfiles keep the bytes their receipts recorded", async () => {
    for (const binding of assemblyReceipts) {
      const receipt = await Bun.file(join(repository, binding.receipt)).json();
      const entries = binding.files(receipt).filter(
        (entry: { path: string }) => entry.path === "Dockerfile",
      );
      expect(entries).toHaveLength(1);

      const bytes = await Bun.file(join(repository, binding.dockerfile)).bytes();
      expect({ path: binding.dockerfile, size: bytes.length }).toEqual({
        path: binding.dockerfile,
        size: entries[0].size,
      });
      const digest = `sha256:${await sha256(binding.dockerfile)}`;
      expect({ path: binding.dockerfile, sha256: digest }).toEqual({
        path: binding.dockerfile,
        sha256: entries[0].sha256,
      });
    }

    const v7 = await Bun.file(join(repository, `${reports}/ic007-openshift-v7.json`)).json();
    expect(await sha256(`${reports}/ic007-v7-assembly.Dockerfile`)).toBe(
      v7.retained_assets["ic007-v7-assembly.Dockerfile"].file_sha256,
    );
  });

  test("Renovate leaves retained assembly Dockerfiles alone", async () => {
    const renovate = await Bun.file(join(repository, "renovate.json")).json() as {
      ignorePaths?: string[];
    };
    const ignorePaths = renovate.ignorePaths ?? [];
    const presetPaths = ignorePaths.filter((path) =>
      renovatePresetIgnorePaths.includes(path)
    );
    expect(presetPaths).toEqual(renovatePresetIgnorePaths);

    const canaryPaths = ignorePaths.filter(
      (path) => !renovatePresetIgnorePaths.includes(path),
    );
    expect(canaryPaths).toEqual([
      "evaluations/platform-canary/**/*-assembly.Dockerfile",
    ]);

    const listing = Bun.spawnSync(["git", "ls-files", "*Dockerfile"], {
      cwd: repository,
      env: Object.fromEntries(
        Object.entries(process.env).filter(([key]) => !key.startsWith("GIT_")),
      ),
    });
    expect(listing.exitCode).toBe(0);
    const tracked = listing.stdout.toString().trim().split("\n").sort();
    const ignored = tracked.filter((path) =>
      canaryPaths.some((pattern) => new Bun.Glob(pattern).match(path))
    );
    expect(ignored).toEqual([...canaryAssemblyDockerfiles].sort());
    expect(tracked).toContain("Dockerfile");
    expect(tracked).toContain("docker/runtime.Dockerfile");
  });
});
