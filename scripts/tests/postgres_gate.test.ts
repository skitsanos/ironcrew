import { describe, expect, test } from "bun:test";
import { configuredDatabases, runPostgresGate, validateVersions } from "../check-postgres";

const environment = {
  IRONCREW_TEST_PG_FLOOR_URL: "postgres://test:secret@floor:5432/disposable",
  IRONCREW_TEST_PG_URL: "postgres://test:secret@latest:5432/disposable",
};
const expectedCommand = [
  "cargo", "test", "--locked", "--all-features",
  "--test", "postgres_store_test", "--test", "usage_storage_test",
  "--test", "app_db_pg_test", "--test", "multi_replica_http_test",
  "--test", "two_process_replica_acceptance_test", "--", "--test-threads=1",
];

function harness(options: { versions?: number[]; failProbe?: number; exit?: number; checkOnly?: boolean } = {}) {
  const probes: string[] = [];
  const calls: Array<{ command: string[]; url: string | undefined }> = [];
  const messages: string[] = [];
  const run = () => runPostgresGate(environment, 17, async (url) => {
    probes.push(url);
    if (options.failProbe === probes.length) throw new Error(`credential leak: ${url}`);
    return (options.versions ?? [170011, 180006])[probes.length - 1];
  }, async (command, env) => {
    expect(probes).toHaveLength(2);
    calls.push({ command, url: env.IRONCREW_TEST_PG_URL });
    return options.exit ?? 0;
  }, options.checkOnly, (message) => messages.push(message));
  return { probes, calls, messages, run };
}

describe("PostgreSQL floor/latest gate", () => {
  test("routes all five serial suites to floor then latest after both probes", async () => {
    const gate = harness();
    await gate.run();
    expect(gate.probes).toEqual([environment.IRONCREW_TEST_PG_FLOOR_URL, environment.IRONCREW_TEST_PG_URL]);
    expect(gate.calls).toEqual([
      { command: expectedCommand, url: environment.IRONCREW_TEST_PG_FLOOR_URL },
      { command: expectedCommand, url: environment.IRONCREW_TEST_PG_URL },
    ]);
    expect(gate.messages.join("\n")).not.toContain("secret");
    expect(environment.IRONCREW_TEST_PG_URL).toContain("@latest:");
  });

  test("admission-only probes both databases without running destructive suites", async () => {
    const gate = harness({ checkOnly: true });
    await gate.run();
    expect(gate.probes).toHaveLength(2);
    expect(gate.calls).toEqual([]);
  });

  test.each([{}, { IRONCREW_TEST_PG_URL: environment.IRONCREW_TEST_PG_URL },
    { IRONCREW_TEST_PG_FLOOR_URL: environment.IRONCREW_TEST_PG_FLOOR_URL }])(
    "missing either URL fails before probing or testing: %j", async (env) => {
      let touched = false;
      await expect(runPostgresGate(env, 17, async () => { touched = true; return 170000; },
        async () => { touched = true; return 0; })).rejects.toThrow("must name a disposable");
      expect(touched).toBeFalse();
    },
  );

  test.each([" ", "not-a-url-secret", "https://secret@example.com/db", "postgres://host"])(
    "invalid URLs are rejected without disclosure: %s", (url) => {
      try {
        configuredDatabases({ ...environment, IRONCREW_TEST_PG_URL: url });
        throw new Error("accepted invalid URL");
      } catch (error) {
        expect(String(error)).not.toContain("secret");
        expect(String(error)).toContain("IRONCREW_TEST_PG_URL");
      }
    },
  );

  test.each([[160015, 180006], [180006, 180006], [170011, 170011], [170011, 160015],
    [NaN, 180006], [170011, 180006.5]])("wrong or identical majors cannot pass: %j", async (...versions) => {
    const gate = harness({ versions });
    await expect(gate.run()).rejects.toThrow();
    expect(gate.calls).toEqual([]);
  });

  test("a later latest major does not advance the fixed floor", () => {
    expect(() => validateVersions(17, [170011, 190000])).not.toThrow();
  });

  test.each([1, 2])("failed probe %i prevents all suites and redacts driver errors", async (failProbe) => {
    const gate = harness({ failProbe });
    await expect(gate.run()).rejects.toThrow(`cannot read ${failProbe === 1 ? "floor" : "latest"} PostgreSQL version`);
    expect(gate.calls).toEqual([]);
    expect(gate.messages).toEqual([]);
  });

  test("a failed floor suite stops the gate before latest", async () => {
    const gate = harness({ exit: 42 });
    await expect(gate.run()).rejects.toThrow("floor PostgreSQL integration failed (exit 42)");
    expect(gate.calls).toHaveLength(1);
  });

  test("a failed latest suite cannot report success after the floor passes", async () => {
    let count = 0;
    await expect(runPostgresGate(environment, 17,
      async (url) => url === environment.IRONCREW_TEST_PG_FLOOR_URL ? 170011 : 180006,
      async () => ++count === 1 ? 0 : 9, false, () => {}))
      .rejects.toThrow("latest PostgreSQL integration failed (exit 9)");
    expect(count).toBe(2);
  });

  test("CLI fails closed when no URLs are configured", async () => {
    const env = { ...process.env };
    delete env.IRONCREW_TEST_PG_URL;
    delete env.IRONCREW_TEST_PG_FLOOR_URL;
    const child = Bun.spawn([process.execPath, "--no-env-file", "run", "scripts/check-postgres.ts", "--check-only"], {
      cwd: new URL("../..", import.meta.url).pathname, env, stdout: "pipe", stderr: "pipe",
    });
    expect(await child.exited).toBe(1);
    expect(await new Response(child.stderr).text()).toContain("IRONCREW_TEST_PG_FLOOR_URL must name");
  });
});
