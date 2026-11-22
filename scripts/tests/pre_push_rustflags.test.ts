import { describe, expect, test } from "bun:test";
import { join } from "node:path";

// IC-060: the local pre-push gate must deny Rust warnings exactly as CI does.
const repository = join(import.meta.dir, "../..");

// GitHub CI is authoritative for these runners; the local gate cannot run them.
const platformOnlyJobs = new Set(["test-macos", "windows-check"]);
const rustFlagVariables = ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"];

type Step = { run?: string; env?: Record<string, unknown> };
type Workflow = {
  env?: Record<string, unknown>;
  jobs: Record<string, { env?: Record<string, unknown>; steps?: Step[] }>;
};

const compileCommand = /\bcargo\s+(?:fmt|build|check|clippy|test|doc|run|audit\s+--deny)\b/;

function isRustStep(command: string) {
  return compileCommand.test(command) ||
    (command.includes("scripts/check-postgres.ts") && !command.includes("--check-only"));
}

function unquote(value: string) {
  return value.trim().replace(/^(['"])(.*)\1$/, "$2");
}

function rustflagsParityErrors(ciSource: string, gateSource: string): string[] {
  const errors: string[] = [];
  const workflow = Bun.YAML.parse(ciSource) as Workflow;
  const ciFlags = workflow.env?.RUSTFLAGS;
  if (typeof ciFlags !== "string" || ciFlags.trim() === "") {
    return ["ci.yml: workflow-level RUSTFLAGS is missing"];
  }

  // Every CI Rust step must inherit the workflow value unchanged.
  const ciRustCommands: string[] = [];
  for (const [jobName, job] of Object.entries(workflow.jobs)) {
    const scopes = [job.env, ...(job.steps ?? []).map((step) => step.env)];
    for (const name of rustFlagVariables) {
      if (scopes.some((scope) => scope && name in scope)) {
        errors.push(`ci.yml: job ${jobName} overrides ${name}`);
      }
    }
    for (const step of job.steps ?? []) {
      const command = step.run ?? "";
      if (rustFlagVariables.some((name) => command.includes(name))) {
        errors.push(`ci.yml: job ${jobName} changes Rust flags inside a step`);
      }
      if (!platformOnlyJobs.has(jobName) && isRustStep(command)) {
        ciRustCommands.push(...command.split("\n").map((line) => line.trim()).filter(isRustStep));
      }
    }
  }

  const lines = gateSource.split("\n").map((line) => line.trim());
  const code = lines.map((line, index) => ({ line, index })).filter(({ line }) => !line.startsWith("#"));
  const exports = code.filter(({ line }) => /^export\s+RUSTFLAGS=/.test(line));
  if (exports.length !== 1) {
    errors.push(`pre-push-check.sh: expected one RUSTFLAGS export, found ${exports.length}`);
    return errors;
  }
  const exported = exports[0];
  const gateFlags = unquote(exported.line.replace(/^export\s+RUSTFLAGS=/, ""));
  if (gateFlags !== ciFlags.trim()) {
    errors.push(`pre-push-check.sh: RUSTFLAGS ${gateFlags} differs from CI ${ciFlags.trim()}`);
  }

  // CARGO_ENCODED_RUSTFLAGS takes precedence over RUSTFLAGS in Cargo.
  const unsetsEncoded = code.some(({ line, index }) =>
    index < exported.index && /^unset\s.*\bCARGO_ENCODED_RUSTFLAGS\b/.test(line));
  if (!unsetsEncoded) {
    errors.push("pre-push-check.sh: CARGO_ENCODED_RUSTFLAGS is not cleared before the export");
  }

  const refresh = code.find(({ line }) => line.includes("./scripts/refresh-develop-dependencies.sh"));
  if (!refresh || refresh.index > exported.index) {
    errors.push("pre-push-check.sh: RUSTFLAGS must be exported after the refresh");
  }

  for (const { line, index } of code) {
    if (index > exported.index && rustFlagVariables.some((name) => line.includes(name))) {
      errors.push(`pre-push-check.sh: line ${index + 1} changes Rust flags after the export`);
    }
    if (index < exported.index && isRustStep(line)) {
      errors.push(`pre-push-check.sh: line ${index + 1} runs a Rust step before the RUSTFLAGS export`);
    }
  }

  const afterExport = lines.slice(exported.index + 1).join("\n");
  for (const command of new Set(ciRustCommands)) {
    if (!afterExport.includes(command)) {
      errors.push(`pre-push-check.sh: CI Rust step is not covered after the export: ${command}`);
    }
  }
  return errors;
}

async function sources() {
  return {
    ci: await Bun.file(join(repository, ".github/workflows/ci.yml")).text(),
    gate: await Bun.file(join(repository, "scripts/pre-push-check.sh")).text(),
  };
}

describe("pre-push RUSTFLAGS parity with CI (IC-060)", () => {
  test("the repository gate and CI deny warnings with the same RUSTFLAGS", async () => {
    const { ci, gate } = await sources();
    expect(rustflagsParityErrors(ci, gate)).toEqual([]);
    expect(gate).toContain('run "Rust no-default-features build" cargo build --no-default-features');
  });

  test("a gate without the export is rejected", async () => {
    const { ci, gate } = await sources();
    const stripped = gate.replace(/^export RUSTFLAGS=.*$/m, "");
    expect(rustflagsParityErrors(ci, stripped)).toEqual([
      "pre-push-check.sh: expected one RUSTFLAGS export, found 0",
    ]);
  });

  test("a different gate value is rejected", async () => {
    const { ci, gate } = await sources();
    const weakened = gate.replace(/^export RUSTFLAGS=.*$/m, "export RUSTFLAGS=-Wwarnings");
    expect(rustflagsParityErrors(ci, weakened)).toContain(
      "pre-push-check.sh: RUSTFLAGS -Wwarnings differs from CI -Dwarnings",
    );
  });

  test("a changed CI value is rejected until the gate follows", async () => {
    const { ci, gate } = await sources();
    const stricter = ci.replace("RUSTFLAGS: -Dwarnings", "RUSTFLAGS: -Dwarnings -Dunsafe_code");
    expect(rustflagsParityErrors(stricter, gate)).toContain(
      "pre-push-check.sh: RUSTFLAGS -Dwarnings differs from CI -Dwarnings -Dunsafe_code",
    );
  });

  test("a Rust step before the export is rejected", async () => {
    const { ci, gate } = await sources();
    const late = gate
      .replace(/^export RUSTFLAGS=.*$/m, "")
      .replace(/^(run "locked release build".*)$/m, "$1\nexport RUSTFLAGS=-Dwarnings");
    const errors = rustflagsParityErrors(ci, late);
    expect(errors.some((error) => error.includes("before the RUSTFLAGS export"))).toBe(true);
    expect(errors.some((error) => error.includes("not covered after the export"))).toBe(true);
  });

  test("an encoded-flags override or a per-step reset is rejected", async () => {
    const { ci, gate } = await sources();
    const encoded = gate.replace(/^unset CARGO_ENCODED_RUSTFLAGS /m, "unset ");
    expect(rustflagsParityErrors(ci, encoded)).toContain(
      "pre-push-check.sh: CARGO_ENCODED_RUSTFLAGS is not cleared before the export",
    );
    const reset = gate.replace(
      'run "Rust no-default-features build" cargo build',
      'run "Rust no-default-features build" env -u RUSTFLAGS cargo build',
    );
    expect(rustflagsParityErrors(ci, reset).some((error) =>
      error.includes("changes Rust flags after the export"))).toBe(true);
  });

  test("a CI job that overrides the workflow value is rejected", async () => {
    const { ci, gate } = await sources();
    const overridden = ci.replace(
      "  rust-default:\n    name: Format, Clippy, and tests\n",
      "  rust-default:\n    name: Format, Clippy, and tests\n    env:\n      RUSTFLAGS: ''\n",
    );
    expect(rustflagsParityErrors(overridden, gate)).toContain(
      "ci.yml: job rust-default overrides RUSTFLAGS",
    );
  });
});
