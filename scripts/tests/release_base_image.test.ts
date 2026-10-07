import { describe, expect, test } from "bun:test";
import { join } from "node:path";

const repository = join(import.meta.dir, "../..");

type CustomManager = {
  customType: string;
  managerFilePatterns: string[];
  matchStrings: string[];
  datasourceTemplate?: string;
  currentValueTemplate?: string;
};

function filePattern(pattern: string) {
  expect(pattern).toMatch(/^\/.+\/$/);
  return new RegExp(pattern.slice(1, -1));
}

describe("release base image refresh", () => {
  test("Renovate refreshes the release receipt digest with the runtime Dockerfile", async () => {
    const renovate = await Bun.file(join(repository, "renovate.json")).json() as {
      customManagers?: CustomManager[];
    };
    const source = await Bun.file(
      join(repository, ".github/workflows/release.yml"),
    ).text();
    const workflow = Bun.YAML.parse(source) as {
      env: { BASE_IMAGE_REFERENCE: string; BASE_IMAGE_INDEX_DIGEST: string };
    };

    const managers = (renovate.customManagers ?? []).filter((manager) =>
      manager.managerFilePatterns.some((pattern) =>
        filePattern(pattern).test(".github/workflows/release.yml")
      )
    );
    expect(managers).toHaveLength(1);
    const [manager] = managers;
    expect(manager.customType).toBe("regex");
    expect(manager.datasourceTemplate).toBe("docker");

    // The runtime Dockerfile pins the base by digest alone. Capturing no
    // currentValue gives the workflow pin the same digest-only lookup, so both
    // resolve to one new digest in the same grouped refresh.
    expect(manager.currentValueTemplate).toBeUndefined();
    expect(manager.matchStrings).toHaveLength(1);
    expect(manager.matchStrings[0]).not.toContain("(?<currentValue>");

    const matches = [...source.matchAll(new RegExp(manager.matchStrings[0], "g"))];
    expect(matches.map((match) => match.groups)).toEqual([
      {
        depName: workflow.env.BASE_IMAGE_REFERENCE,
        currentDigest: workflow.env.BASE_IMAGE_INDEX_DIGEST,
      },
    ]);
  });
});
