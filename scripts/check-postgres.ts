import { SQL } from "bun";
import { join } from "node:path";

const repository = join(import.meta.dir, "..");
export const postgresSuites = [
  "postgres_store_test",
  "usage_storage_test",
  "app_db_pg_test",
  "multi_replica_http_test",
  "two_process_replica_acceptance_test",
] as const;

type Environment = Record<string, string | undefined>;
type Probe = (url: string) => Promise<number>;
type Run = (command: string[], environment: Environment) => Promise<number>;

export function configuredDatabases(environment: Environment) {
  return ["IRONCREW_TEST_PG_FLOOR_URL", "IRONCREW_TEST_PG_URL"].map((key) => {
    const value = environment[key];
    if (!value?.trim()) throw new Error(`${key} must name a disposable test database`);
    try {
      const url = new URL(value);
      if (!["postgres:", "postgresql:"].includes(url.protocol) || !url.hostname ||
          url.pathname.length < 2) throw new Error();
    } catch {
      throw new Error(`${key} must be a PostgreSQL URL with an explicit database`);
    }
    return value;
  });
}

export function validateVersions(floor: number, versions: number[]) {
  if (!Number.isSafeInteger(floor) || floor < 10 || versions.length !== 2 ||
      versions.some((version) => !Number.isSafeInteger(version) || version < 100000)) {
    throw new Error("invalid PostgreSQL support-floor or server-version response");
  }
  if (Math.floor(versions[0] / 10000) !== floor) {
    throw new Error(`floor database must run PostgreSQL ${floor}`);
  }
  if (Math.floor(versions[1] / 10000) <= floor) {
    throw new Error("latest database must run a newer major than the floor database");
  }
}

export async function probeVersion(url: string): Promise<number> {
  const sql = new SQL(url, { max: 1, connectionTimeout: 5 });
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const rows = await Promise.race([
      sql`SHOW server_version_num`,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error("version probe timed out")), 5000);
      }),
    ]);
    const value = String(rows[0]?.server_version_num ?? "");
    if (!/^\d{6,7}$/.test(value)) throw new Error("invalid server version");
    return Number(value);
  } finally {
    clearTimeout(timer);
    await sql.close({ timeout: 0 });
  }
}

export async function runPostgresGate(
  environment: Environment,
  floor: number,
  probe: Probe,
  run: Run,
  checkOnly = false,
  report: (message: string) => void = console.log,
) {
  const urls = configuredDatabases(environment);
  const versions: number[] = [];
  // Validate BOTH endpoints before any integration test can modify a database.
  for (const [index, url] of urls.entries()) {
    try {
      versions.push(await probe(url));
    } catch {
      // Driver errors can contain connection strings. Never echo them.
      throw new Error(`cannot read ${index === 0 ? "floor" : "latest"} PostgreSQL version`);
    }
  }
  validateVersions(floor, versions);
  for (const [index, version] of versions.entries()) {
    report(`postgres gate: ${index === 0 ? "floor" : "latest"} server ${Math.floor(version / 10000)}.${version % 10000}`);
  }
  if (checkOnly) return;
  const command = ["cargo", "test", "--locked", "--all-features",
    ...postgresSuites.flatMap((suite) => ["--test", suite]), "--", "--test-threads=1"];
  for (const [index, url] of urls.entries()) {
    const label = index === 0 ? "floor" : "latest";
    report(`postgres gate: running all five suites on ${label}`);
    const code = await run(command, { ...environment, IRONCREW_TEST_PG_URL: url });
    if (code !== 0) throw new Error(`${label} PostgreSQL integration failed (exit ${code})`);
  }
}

if (import.meta.main) {
  try {
    const args = process.argv.slice(2);
    if (args.length > 1 || (args.length === 1 && args[0] !== "--check-only")) {
      throw new Error("usage: bun run scripts/check-postgres.ts [--check-only]");
    }
    const source = await Bun.file(join(repository, "src/engine/pg_runtime.rs")).text();
    const floor = Number(source.match(/MINIMUM_POSTGRES_MAJOR: u32 = (\d+);/)?.[1]);
    await runPostgresGate(process.env, floor, probeVersion, async (command, env) => {
      return await Bun.spawn(command, {
        cwd: repository, env, stdout: "inherit", stderr: "inherit", stdin: "inherit",
      }).exited;
    }, args[0] === "--check-only");
  } catch (error) {
    console.error(`postgres gate: ${error instanceof Error ? error.message : "gate failed"}`);
    process.exitCode = 1;
  }
}
