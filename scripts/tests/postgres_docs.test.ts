import { describe, expect, test } from "bun:test";
import { join } from "node:path";

// Match requirements, not dated receipts such as PostgreSQL 15.18 acceptance.
function staleRequirements(text: string, floor: number): number[] {
  const patterns = [
    /PostgreSQL\s+(\d+)(?:\s*\+|\s+(?:or|and)\s+(?:newer|later))/gi,
    /\b(\d+)\+\s+(?:schema|requirement|connection|DSN)\b/gi,
  ];
  return patterns.flatMap((pattern) => [...text.matchAll(pattern)]
    .map((match) => Number(match[1])).filter((major) => major < floor));
}

describe("PostgreSQL documentation requirements", () => {
  test.each([
    "PostgreSQL 15+ required", "PostgreSQL 15 or newer is required",
    "PostgreSQL 16 or later", "PostgreSQL\n15+ schema", "The 15+ requirement",
    "PostgreSQL 16 and newer",
  ])("rejects stale requirement: %s", (text) => {
    expect(staleRequirements(text, 17).length).toBeGreaterThan(0);
  });

  test("preserves historical receipts and upgrade instructions", () => {
    expect(staleRequirements("PostgreSQL 15.18 acceptance passed on 2026-08-11. " +
      "Upgrade PostgreSQL 15/16 deployments. Requires PostgreSQL 17+.", 17)).toEqual([]);
  });

  test("current requirement docs agree with the runtime floor", async () => {
    const root = join(import.meta.dir, "../..");
    const runtime = await Bun.file(join(root, "src/engine/pg_runtime.rs")).text();
    const floor = Number(runtime.match(/MINIMUM_POSTGRES_MAJOR: u32 = (\d+);/)?.[1]);
    expect(Number.isSafeInteger(floor)).toBeTrue();
    for (const path of ["docs/storage.md", "docs/cli.md", "docs/cloud-deployment.md",
      "docs/multi-replica.md", "AGENTS.md", ".agents/skills/check-ironcrew/SKILL.md"]) {
      expect(staleRequirements(await Bun.file(join(root, path)).text(), floor), path).toEqual([]);
    }
  });
});
