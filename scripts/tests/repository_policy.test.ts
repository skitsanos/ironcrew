import { describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { validatePlainMappingKeys } from "../validate_skills";

const repository = join(import.meta.dir, "../..");
const trustedReleaseActions = new Set([
  "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
  "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
  "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
  "anchore/sbom-action@57aae528053a48a3f6235f2d9461b05fbcb7366d",
  "docker/login-action@dbcb813823bdd20940b903addbd779551569679f",
  "docker/setup-buildx-action@bb05f3f5519dd87d3ba754cc423b652a5edd6d2c",
  "docker/setup-qemu-action@96fe6ef7f33517b61c61be40b68a1882f3264fb8",
  "dtolnay/rust-toolchain@01ba1edad32c6f80dbcce879d3e0fa5a00b2a84e",
  "sigstore/cosign-installer@6f9f17788090df1f26f669e9d70d6ae9567deba6",
  "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6",
]);

describe("repository integration policy", () => {
  test("skill metadata rejects duplicate mapping keys before YAML parsing", () => {
    const source = "name: first\ndescription: valid\nname: shadowed\n";
    expect(validatePlainMappingKeys(source, 0)).toEqual([
      "line 3 duplicates mapping key 'name'",
    ]);
  });

  test("CI retains pull-request coverage and least privilege", async () => {
    const source = await Bun.file(join(repository, ".github/workflows/ci.yml")).text();
    const workflow = Bun.YAML.parse(source) as {
      on: {
        push: { branches: string[] };
        pull_request: { branches: string[] };
      };
      permissions: Record<string, string>;
    };

    expect(workflow.on.push.branches).toEqual(["main", "develop"]);
    expect(workflow.on.pull_request.branches).toEqual(["main", "develop"]);
    expect(workflow.permissions).toEqual({ contents: "read" });
  });

  test("CI runs the repository policy and workflow lint", async () => {
    const source = await Bun.file(join(repository, ".github/workflows/ci.yml")).text();
    const workflow = Bun.YAML.parse(source) as {
      jobs: Record<string, {
        env?: Record<string, string>;
        steps: Array<{
          name?: string;
          uses?: string;
          run?: string;
          with?: Record<string, unknown>;
          env?: Record<string, string>;
        }>;
      }>;
    };
    const policy = workflow.jobs["repository-policy"];
    expect(policy).toBeDefined();
    expect(
      policy.steps.some(
        (step) => step.uses === "oven-sh/setup-bun@v2" && step.with?.["bun-version"] === "1.3.14",
      ),
    ).toBeTrue();
    expect(
      policy.steps.find((step) => step.uses === "actions/checkout@v7")?.with,
    ).toEqual({ "fetch-depth": 0 });
    const registry = policy.steps.find(
      (step) => step.name === "Validate issue registry against trusted history",
    );
    expect(registry).toBeDefined();
    expect(policy.env?.IRONCREW_POLICY_BASE_SHA).toBe(
      "${{ github.event.pull_request.base.sha || github.event.before }}",
    );
    const commands = policy.steps.map((step) => step.run ?? "").join("\n");
    expect(commands).toContain("python3 -B scripts/check_module_size.py");
    expect(commands).toContain(
      "python3 -B -m unittest discover -s scripts/tests -p 'test_*.py'",
    );
    expect(commands).toContain("bun run scripts/validate_skills.ts");
    expect(commands).toContain("bun run scripts/issues_registry.ts check");
    expect(commands).toContain("bun test scripts/tests/*.test.ts");
    expect(commands).toContain("bun run scripts/check_worktree.ts");
    expect(commands).toMatch(/actionlint"? \.github\/workflows\/\*\.yml/);
  });

  test("worktree validation covers untracked, staged, and committed whitespace", async () => {
    const fixture = await mkdtemp(join(tmpdir(), "ironcrew-worktree-policy-"));
    const checker = join(repository, "scripts/check_worktree.ts");
    const emptyGitConfig = join(fixture, "empty.gitconfig");
    const gitEnvironment = Object.fromEntries(
      Object.entries(process.env).filter(
        ([key]) => !key.startsWith("GIT_CONFIG_") && key !== "IRONCREW_POLICY_BASE_SHA",
      ),
    );
    gitEnvironment.GIT_CONFIG_NOSYSTEM = "1";
    gitEnvironment.GIT_CONFIG_GLOBAL = emptyGitConfig;

    async function check(extraEnvironment: Record<string, string> = {}): Promise<number> {
      return await Bun.spawn(["bun", "run", checker, fixture], {
        cwd: fixture,
        env: { ...gitEnvironment, ...extraEnvironment },
        stdout: "ignore",
        stderr: "ignore",
      }).exited;
    }

    async function git(...args: string[]): Promise<number> {
      return await Bun.spawn(["git", ...args], {
        cwd: fixture,
        env: gitEnvironment,
        stdout: "ignore",
        stderr: "ignore",
      }).exited;
    }

    async function head(): Promise<string> {
      const process = Bun.spawn(["git", "rev-parse", "HEAD"], {
        cwd: fixture,
        env: gitEnvironment,
        stdout: "pipe",
        stderr: "ignore",
      });
      const output = await new Response(process.stdout).text();
      expect(await process.exited).toBe(0);
      return output.trim();
    }

    try {
      await writeFile(emptyGitConfig, "");
      expect(await git("init", "-b", "main")).toBe(0);
      expect(await git("config", "user.name", "IronCrew Policy Test")).toBe(0);
      expect(await git("config", "user.email", "policy-test@invalid.example")).toBe(0);
      await writeFile(join(fixture, "new.md"), "clean\n");
      expect(await check()).toBe(0);

      await writeFile(join(fixture, "new.md"), "trailing space \n");
      expect(await check()).not.toBe(0);

      expect(await git("add", "new.md")).toBe(0);
      expect(await check()).not.toBe(0);

      await writeFile(join(fixture, "new.md"), "clean\n");
      expect(await git("add", "new.md")).toBe(0);
      expect(await git("commit", "-m", "clean baseline")).toBe(0);
      const trustedBase = await head();
      expect(await check({ IRONCREW_POLICY_BASE_SHA: trustedBase })).toBe(0);

      await writeFile(join(fixture, "committed.md"), "committed trailing space \n");
      expect(await git("add", "committed.md")).toBe(0);
      expect(await git("commit", "-m", "bad committed whitespace")).toBe(0);
      expect(await check({ IRONCREW_POLICY_BASE_SHA: trustedBase })).not.toBe(0);
      expect(await check({
        IRONCREW_POLICY_BASE_SHA: trustedBase,
        GIT_CONFIG_COUNT: "1",
        GIT_CONFIG_KEY_0: "core.whitespace",
        GIT_CONFIG_VALUE_0: "-trailing-space",
      })).not.toBe(0);
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });

  test("CI pins Rust and preserves the exact all-target gates", async () => {
    const source = await Bun.file(join(repository, ".github/workflows/ci.yml")).text();
    const toolchain = await Bun.file(join(repository, "rust-toolchain.toml")).text();
    expect(source).not.toContain("dtolnay/rust-toolchain@stable");

    const workflow = Bun.YAML.parse(source) as {
      jobs: Record<string, { steps: Array<{ uses?: string; run?: string }> }>;
    };
    const commands = workflow.jobs["rust-default"].steps
      .map((step) => step.run ?? "")
      .join("\n");
    expect(commands).toContain("cargo fmt --all -- --check");
    expect(commands).toContain("cargo clippy --all-targets -- -D warnings");
    expect(commands).toContain("cargo test --all-targets");
    expect(commands).toContain("cargo test --doc");
    expect(source).toContain("dtolnay/rust-toolchain@1.96.0");
    expect(toolchain).toContain('channel = "1.96.0"');
    expect(toolchain).toContain('components = ["clippy", "rustfmt"]');
  });

  test("dependency audit warnings fail closed and the fixed transitive stays selected", async () => {
    const workflow = await Bun.file(join(repository, ".github/workflows/ci.yml")).text();
    const lockfile = await Bun.file(join(repository, "Cargo.lock")).text();

    expect(workflow).toContain("cargo audit --deny warnings");
    expect(workflow).toContain("cargo-audit --version 0.22.1 --locked");
    expect(lockfile).toContain('name = "event-listener"\nversion = "5.4.2"');
    expect(lockfile).not.toContain('name = "event-listener"\nversion = "5.4.1"');
  });

  test("PostgreSQL 15 process and soak gates remain in CI", async () => {
    const source = await Bun.file(join(repository, ".github/workflows/ci.yml")).text();
    const agents = await Bun.file(join(repository, "AGENTS.md")).text();
    const soakGuide = await Bun.file(
      join(repository, "evaluations/replica-soak/README.md"),
    ).text();
    const storeTest = await Bun.file(
      join(repository, "tests/postgres_store_test.rs"),
    ).text();
    const workflow = Bun.YAML.parse(source) as {
      jobs: Record<string, {
        services?: Record<string, { image?: string }>;
        steps: Array<{ run?: string }>;
      }>;
    };
    const postgres = workflow.jobs["postgres-integration"];
    expect(postgres.services?.postgres.image).toBe("postgres:15");
    const commands = postgres.steps.map((step) => step.run ?? "").join("\n");
    expect(commands).toContain("--test two_process_replica_acceptance_test");
    expect(commands).toContain("evaluations/replica-soak/soak.py");
    expect(agents).toContain("Pull the moving `postgres:15` tag");
    expect(agents).toContain("Do not substitute `postgres:latest`");
    expect(agents).toContain("Never run a global Docker system, image, builder");
    expect(soakGuide).toContain("docker pull postgres:15");
    expect(soakGuide).toContain("postgres:15");
    expect(soakGuide).toContain("docker run --rm -d");
    expect(soakGuide).toContain("ironcrew_pg_container_id=$(docker run");
    expect(soakGuide).toContain('docker stop "$ironcrew_pg_container_id"');
    expect(soakGuide).toContain('docker inspect "$ironcrew_pg_container_id"');
    expect(storeTest).toContain("docker pull postgres:15");
    expect(storeTest).toContain("postgres:15");
    expect(`${soakGuide}\n${storeTest}`).not.toContain("postgres:17");
    expect(`${soakGuide}\n${storeTest}`).not.toContain("postgres:latest");
  });

  test("Lua validation includes the platform-canary runtime smoke", async () => {
    const luaGate = await Bun.file(
      join(repository, "scripts/check-lua-examples.sh"),
    ).text();
    const canaryGuide = await Bun.file(
      join(repository, "evaluations/platform-canary/README.md"),
    ).text();

    expect(luaGate).toContain("evaluations/platform-canary/runtime_smoke.py");
    expect(luaGate).toContain('"flows_executed":4');
    expect(canaryGuide).toContain("Static Lua validation does not execute");
  });

  test("OpenShift dual-stack policy avoids rejected IPv4-mapped exclusions", async () => {
    const manifest = await Bun.file(join(repository, "deploy/openshift.yaml")).text();
    const cloudGuide = await Bun.file(
      join(repository, "docs/cloud-deployment.md"),
    ).text();

    expect(manifest).toContain('cidr: "::/0"');
    expect(manifest).not.toContain('::ffff:0:0/96');
    expect(manifest).toContain("kubernetes.io/metadata.name: openshift-dns");
    expect(manifest.match(/port: 5353/g)).toHaveLength(2);
    expect(cloudGuide).toContain("UDP/TCP 53 and 5353 in `openshift-dns`");
    expect(cloudGuide).not.toContain(
      "\nIRONCREW_INSTANCE_ID=${{RAILWAY_REPLICA_ID}}\nIRONCREW_STORE=postgres",
    );
    expect(cloudGuide).toContain(
      "Railway injects `RAILWAY_REPLICA_ID` only into each running replica",
    );
  });

  test("journal write timing stays explicit across deployment and canary contracts", async () => {
    const [manifest, canaryConfig, configContract, canaryGuide, cliGuide] = await Promise.all([
      Bun.file(join(repository, "deploy/openshift.yaml")).text(),
      Bun.file(join(repository, "evaluations/platform-canary/canary_config.py")).text(),
      Bun.file(join(repository, "evaluations/platform-canary/config_contract.py")).text(),
      Bun.file(join(repository, "evaluations/platform-canary/README.md")).text(),
      Bun.file(join(repository, "docs/cli.md")).text(),
    ]);

    expect(manifest).toMatch(
      /- name: IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS\n\s+value: "5000"/,
    );
    expect(canaryConfig).toContain(
      '("IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS", "5000")',
    );
    expect(configContract).toContain('"IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS"');
    expect(canaryGuide).toContain("not the application's 1500 ms default");
    expect(cliGuide).toContain("flush/terminal acknowledgement is bounded by `3W + 650 ms`");
  });

  test("sole-owner issue requests dispatch only fixed trusted workflows", async () => {
    const source = await Bun.file(
      join(repository, ".github/workflows/release-request.yml"),
    ).text();
    const workflow = Bun.YAML.parse(source) as {
      on: { issues: { types: string[] } };
      permissions: Record<string, string>;
      concurrency: { group: string; "cancel-in-progress": boolean };
      jobs: Record<string, {
        if?: string;
        needs?: string;
        "timeout-minutes"?: number;
        env?: Record<string, string>;
        outputs?: Record<string, string>;
        permissions?: Record<string, string>;
        steps: Array<{
          name?: string;
          if?: string;
          uses?: string;
          run?: string;
          env?: Record<string, string>;
          with?: Record<string, string | boolean>;
        }>;
      }>;
    };

    expect(Object.keys(workflow.on)).toEqual(["issues"]);
    expect(workflow.on.issues.types).toEqual(["labeled"]);
    expect(workflow.permissions).toEqual({});
    expect(workflow.concurrency).toEqual({
      group: "release-request-${{ github.event.issue.number }}",
      "cancel-in-progress": false,
    });
    expect(workflow.jobs.guard.permissions).toEqual({
      contents: "read",
      issues: "read",
    });
    expect(workflow.jobs.guard["timeout-minutes"]).toBe(5);
    expect(workflow.jobs.guard.outputs).toEqual({
      relevant: "${{ steps.request.outputs.relevant }}",
      target: "${{ steps.request.outputs.target }}",
      tag: "${{ steps.request.outputs.tag }}",
      mode: "${{ steps.request.outputs.mode }}",
    });
    expect(workflow.jobs.guard.steps[0]?.env).toEqual({
      EVENT_ACTION: "${{ github.event.action }}",
      DEFAULT_BRANCH: "${{ github.event.repository.default_branch }}",
    });
    const guardCommands = workflow.jobs.guard.steps
      .map((step) => step.run ?? "")
      .join("\n");
    expect(guardCommands).toContain('test "$GITHUB_EVENT_NAME" = issues');
    expect(guardCommands).toContain('test "$EVENT_ACTION" = labeled');
    expect(guardCommands).toContain('test "$DEFAULT_BRANCH" = main');
    expect(guardCommands).toContain('test "$GITHUB_REF" = refs/heads/main');
    expect(guardCommands).toContain(
      "$GITHUB_REPOSITORY/.github/workflows/release-request.yml@refs/heads/main",
    );
    expect(guardCommands).toContain('test "$GITHUB_WORKFLOW_SHA" = "$GITHUB_SHA"');
    expect(guardCommands).toContain("scripts/validate_release_request.py");
    expect(guardCommands).toContain('--actor "$GITHUB_ACTOR"');
    expect(guardCommands).toContain('--triggering-actor "$TRIGGERING_ACTOR"');
    expect(guardCommands).toContain("--owner skitsanos");
    const requestStep = workflow.jobs.guard.steps.find(
      (step) => step.name === "Validate exact issue request",
    );
    expect(requestStep?.env).toEqual({
      TRIGGERING_ACTOR: "${{ github.triggering_actor }}",
    });
    const checkout = workflow.jobs.guard.steps.find((step) => step.uses);
    expect(checkout?.uses).toBe(
      "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
    );
    expect(checkout?.with).toEqual({
      "persist-credentials": false,
      ref: "${{ github.sha }}",
    });

    const dispatch = workflow.jobs.dispatch;
    expect(dispatch.needs).toBe("guard");
    expect(dispatch.if).toBe("needs.guard.outputs.relevant == 'true'");
    expect(dispatch["timeout-minutes"]).toBe(5);
    expect(dispatch.permissions).toEqual({ contents: "write" });
    expect(dispatch.env).toEqual({
      RELEASE_TAG: "${{ needs.guard.outputs.tag }}",
      RELEASE_MODE: "${{ needs.guard.outputs.mode }}",
    });
    expect(dispatch.steps).toHaveLength(2);
    expect(dispatch.steps.every((step) => !step.uses)).toBeTrue();
    expect(dispatch.steps.every((step) => step.env?.GH_TOKEN === "${{ github.token }}"))
      .toBeTrue();
    const release = dispatch.steps[0];
    expect(release?.if).toBe("needs.guard.outputs.target == 'release'");
    expect(release?.run?.trim()).toBe(
      'gh api --method POST "repos/${GITHUB_REPOSITORY}/dispatches" \\\n' +
        "  -f event_type=ironcrew_release_v1 \\\n" +
        '  -f "client_payload[tag]=${RELEASE_TAG}" \\\n' +
        '  -f "client_payload[mode]=${RELEASE_MODE}"',
    );
    const docker = dispatch.steps[1];
    expect(docker?.if).toBe("needs.guard.outputs.target == 'docker'");
    expect(docker?.run?.trim()).toBe(
      'gh api --method POST "repos/${GITHUB_REPOSITORY}/dispatches" \\\n' +
        "  -f event_type=ironcrew_docker_publish_v1 \\\n" +
        '  -f "client_payload[tag]=${RELEASE_TAG}" \\\n' +
        '  -f "client_payload[mode]=${RELEASE_MODE}"',
    );
    expect(source).not.toContain("secrets.");
    expect(source).not.toContain("issues: write");
    expect(source).not.toContain("workflow_dispatch");
    expect(source).not.toContain("gh release");
    expect(source).not.toContain("docker build");
    expect(source).not.toContain("docker push");
  });

  test("release receipt base image matches the runtime Dockerfile", async () => {
    const workflowSource = await Bun.file(
      join(repository, ".github/workflows/release.yml"),
    ).text();
    const workflow = Bun.YAML.parse(workflowSource) as {
      env: {
        BASE_IMAGE_REFERENCE: string;
        BASE_IMAGE_INDEX_DIGEST: string;
      };
    };
    const dockerfile = await Bun.file(
      join(repository, "docker/runtime.Dockerfile"),
    ).text();
    const baseLines = dockerfile
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.startsWith("FROM "));

    expect(baseLines).toEqual([
      `FROM ${workflow.env.BASE_IMAGE_REFERENCE}@${workflow.env.BASE_IMAGE_INDEX_DIGEST}`,
    ]);
  });

  test("release workflow gates builds and scopes publication authority", async () => {
    const source = await Bun.file(
      join(repository, ".github/workflows/release.yml"),
    ).text();
    const workflow = Bun.YAML.parse(source) as {
      on: { repository_dispatch: { types: string[] } };
      permissions: Record<string, string>;
      concurrency: { group: string; "cancel-in-progress": boolean };
      jobs: Record<string, {
        if?: string;
        needs?: string | string[];
        environment?: string;
        env?: Record<string, string>;
        outputs?: Record<string, string>;
        permissions?: Record<string, string>;
        steps: Array<{
          name?: string;
          uses?: string;
          run?: string;
          env?: Record<string, string>;
          with?: Record<string, string>;
        }>;
      }>;
    };
    const guardCommands = workflow.jobs.guard.steps
      .map((step) => step.run ?? "")
      .join("\n");

    expect(workflow.permissions).toEqual({});
    expect(Object.keys(workflow.on)).toEqual(["repository_dispatch"]);
    expect(workflow.on.repository_dispatch.types).toEqual(["ironcrew_release_v1"]);
    expect(workflow.concurrency).toEqual({
      group: "release-publication",
      "cancel-in-progress": false,
    });
    expect(workflow.jobs.guard.if).toBeUndefined();
    const firstGuardStep = workflow.jobs.guard.steps[0];
    expect(firstGuardStep?.name).toBe("Bind event to the default-branch workflow");
    expect(firstGuardStep?.env).toEqual({
      DISPATCH_ACTION: "${{ github.event.action }}",
      DEFAULT_BRANCH: "${{ github.event.repository.default_branch }}",
    });
    expect(firstGuardStep?.run).toContain(
      'test "$GITHUB_EVENT_NAME" = repository_dispatch',
    );
    expect(firstGuardStep?.run).toContain(
      'test "$DISPATCH_ACTION" = ironcrew_release_v1',
    );
    expect(workflow.jobs.guard.permissions).toEqual({ contents: "read" });
    expect(workflow.jobs.guard.outputs).toEqual({
      tag: "${{ steps.source.outputs.tag }}",
      mode: "${{ steps.dispatch.outputs.mode }}",
      commit_sha: "${{ steps.source.outputs.commit_sha }}",
      source_date_epoch: "${{ steps.source.outputs.source_date_epoch }}",
    });
    expect(workflow.jobs.build.permissions).toEqual({ contents: "read" });
    expect(workflow.jobs.build.if).toBe("needs.guard.outputs.mode == 'publish'");
    expect(workflow.jobs.image.permissions).toEqual({ contents: "read" });
    expect(workflow.jobs.image.needs).toEqual(["guard", "build"]);
    expect(workflow.jobs.image.if).toBe(
      "needs.guard.outputs.mode == 'publish' && needs.build.result == 'success'",
    );
    expect(workflow.jobs.validate.environment).toBe("release");
    expect(workflow.jobs.validate.if).toBe("needs.guard.outputs.mode == 'validate'");
    expect(workflow.jobs.validate.permissions).toEqual({ contents: "read" });
    const validationCommands = workflow.jobs.validate.steps
      .map((step) => step.run ?? "")
      .join("\n");
    expect(validationCommands).not.toContain("secrets.");
    expect(validationCommands).not.toContain("gh release");
    expect(validationCommands).not.toContain("cosign");
    expect(workflow.jobs.release.environment).toBe("release");
    expect(workflow.jobs.release.if).toBe(
      "needs.guard.outputs.mode == 'publish' && needs.build.result == 'success' && needs.image.result == 'success'",
    );
    expect(workflow.jobs.release.permissions).toEqual({
      contents: "write",
      "id-token": "write",
    });
    expect(workflow.jobs.build.needs).toBe("guard");
    expect(workflow.jobs.release.needs).toEqual(["guard", "build", "image"]);
    expect(workflow.jobs.release.env).toEqual({
      RELEASE_TAG: "${{ needs.guard.outputs.tag }}",
      RELEASE_COMMIT: "${{ needs.guard.outputs.commit_sha }}",
      SOURCE_DATE_EPOCH: "${{ needs.guard.outputs.source_date_epoch }}",
    });
    expect(guardCommands).toContain('test "$GITHUB_REF" = refs/heads/main');
    expect(guardCommands).toContain(
      'test "$GITHUB_WORKFLOW_SHA" = "$GITHUB_SHA"',
    );
    expect(guardCommands).toContain(
      '"$GITHUB_REPOSITORY/.github/workflows/release.yml@refs/heads/main"',
    );
    expect(guardCommands).toContain(
      '"refs/heads/main:refs/remotes/origin/main"',
    );
    expect(guardCommands).toContain("scripts/validate_release_dispatch.py");
    expect(guardCommands).toContain("--event-type ironcrew_release_v1");
    expect(guardCommands).toContain('--repository "$GITHUB_REPOSITORY"');
    expect(guardCommands).toContain('--actor "$GITHUB_ACTOR"');
    expect(guardCommands).toContain(
      "../scripts/verify_release_source.sh \"$RELEASE_TAG\" refs/remotes/origin/main",
    );
    expect(guardCommands).toContain('echo "commit_sha=$commit_sha"');

    const trustedCheckouts = workflow.jobs.release.steps.filter(
      (step) => step.name === "Check out trusted release controls",
    );
    expect(trustedCheckouts).toHaveLength(1);
    expect(trustedCheckouts[0]?.with).toEqual({
      "fetch-depth": 0,
      "persist-credentials": false,
      ref: "${{ github.sha }}",
    });
    const tagCheckout = workflow.jobs.release.steps.find(
      (step) => step.name === "Check out authorized tag source separately",
    );
    expect(tagCheckout?.with).toEqual({
      "fetch-depth": 0,
      path: "tag-source",
      "persist-credentials": false,
      ref: "${{ needs.guard.outputs.commit_sha }}",
    });
    const binding = workflow.jobs.release.steps.find(
      (step) => step.name === "Revalidate the tag-to-commit binding",
    );
    expect(binding?.run).toContain(
      'test "$(git rev-parse "refs/tags/${RELEASE_TAG}^{commit}")" = "$RELEASE_COMMIT"',
    );
    const immutableAssets = workflow.jobs.release.steps.find(
      (step) => step.name === "Refuse release-asset replacement",
    );
    expect(immutableAssets?.run).toContain(
      'scripts/verify_release_absent.py --repository "$GITHUB_REPOSITORY" --tag "$RELEASE_TAG"',
    );
    const imageBuild = workflow.jobs.image.steps.find(
      (step) => step.name === "Build tag-owned multi-platform OCI archive once",
    );
    expect(imageBuild?.run).toContain("--file tag-source/docker/runtime.Dockerfile");
    expect(imageBuild?.run).toContain("--label \"org.opencontainers.image.revision=${RELEASE_COMMIT}\"");
    expect(imageBuild?.run?.trimEnd().endsWith("tag-source")).toBeTrue();
    const receipt = workflow.jobs.image.steps.find(
      (step) => step.name === "Create and verify strict image receipt",
    );
    expect(receipt?.run).toContain('--commit-sha "$RELEASE_COMMIT"');
    expect(receipt?.run).toContain('--tag "$RELEASE_TAG"');
    expect(receipt?.run).toContain("--dockerfile tag-source/docker/runtime.Dockerfile");
    const releaseCommands = workflow.jobs.release.steps
      .map((step) => step.run ?? "")
      .join("\n");
    expect(releaseCommands).not.toContain("docker buildx");
    expect(releaseCommands).not.toContain("release_image_receipt.py generate");
    expect(releaseCommands).not.toContain("sbom-action");
    expect(releaseCommands).toContain("scripts/verify_release_image.py");
    expect(releaseCommands).toContain("scripts/verify_release_bindings.py");
    expect(releaseCommands).toContain('--commit-sha "$RELEASE_COMMIT"');
    expect(releaseCommands).toContain("--dockerfile tag-source/docker/runtime.Dockerfile");
    expect(releaseCommands).toContain("ironcrew-release-files.expected");
    expect(releaseCommands).toContain("ironcrew-published-files.expected");
    expect(releaseCommands).not.toContain('[ -f "$artifact" ] || continue');
    for (const job of Object.values(workflow.jobs)) {
      for (const step of job.steps) {
        if (step.uses?.startsWith("actions/checkout@")) {
          expect(step.with?.["persist-credentials"]).toBe(false);
        }
        if (step.uses) {
          expect(step.uses).toMatch(/@[0-9a-f]{40}$/);
          expect(trustedReleaseActions.has(step.uses)).toBeTrue();
        }
      }
    }
    expect(source).not.toContain("Create or update release");
    expect(source).not.toContain("workflow_dispatch");
    expect(source).not.toContain("github.event.client_payload");
    expect(source).not.toContain("GITHUB_REF_NAME");

    const notes = workflow.jobs.release.steps.find(
      (step) => step.name === "Write release notes from tag annotation",
    );
    expect(notes?.run).toContain("${RUNNER_TEMP}/ironcrew-release-body.md");
    expect(notes?.run).not.toContain("GITHUB_OUTPUT");
    expect(notes?.run).not.toContain("RELEASE_EOF");
    expect(notes?.run).toContain(
      "https://github.com/${GITHUB_REPOSITORY}/.github/workflows/release.yml@refs/heads/main",
    );
    expect(notes?.run).toContain("--certificate-identity '$CERTIFICATE_IDENTITY'");
    const publisher = workflow.jobs.release.steps.find(
      (step) => step.name === "Create release once",
    );
    expect(publisher?.run).toContain('gh release create "$RELEASE_TAG"');
    expect(publisher?.run).toContain('--notes-file "$RUNNER_TEMP/ironcrew-release-body.md"');
    expect(publisher?.run).toContain("--verify-tag");
    expect(source).not.toContain("softprops/action-gh-release");
  });

  test("release source verification rejects lightweight, version-drift, and off-main tags", async () => {
    const fixture = await mkdtemp(join(tmpdir(), "ironcrew-release-policy-"));
    const verifier = join(repository, "scripts/verify_release_source.sh");
    const emptyGitConfig = join(fixture, "empty.gitconfig");
    const gitEnvironment = Object.fromEntries(
      Object.entries(process.env).filter(([key]) => !key.startsWith("GIT_CONFIG_")),
    );
    gitEnvironment.GIT_CONFIG_NOSYSTEM = "1";
    gitEnvironment.GIT_CONFIG_GLOBAL = emptyGitConfig;

    async function git(...args: string[]): Promise<number> {
      return await Bun.spawn(["git", ...args], {
        cwd: fixture,
        env: gitEnvironment,
        stdout: "ignore",
        stderr: "ignore",
      }).exited;
    }

    async function verify(tag: string): Promise<number> {
      return await Bun.spawn([verifier, tag, "refs/heads/main"], {
        cwd: fixture,
        env: gitEnvironment,
        stdout: "ignore",
        stderr: "ignore",
      }).exited;
    }

    try {
      await writeFile(emptyGitConfig, "");
      expect(await git("init", "-b", "main")).toBe(0);
      expect(await git("config", "user.name", "IronCrew Policy Test")).toBe(0);
      expect(await git("config", "user.email", "policy-test@invalid.example")).toBe(0);
      await writeFile(
        join(fixture, "Cargo.toml"),
        '[package]\nname = "fixture"\nversion = "1.2.3"\n',
      );
      expect(await git("add", "Cargo.toml")).toBe(0);
      expect(await git("commit", "-m", "main release")).toBe(0);
      expect(await git("tag", "v1.2.3")).toBe(0);
      expect(await verify("v1.2.3")).not.toBe(0);
      expect(await git("tag", "--delete", "v1.2.3")).toBe(0);
      expect(await git("tag", "-a", "v1.2.3", "-m", "release")).toBe(0);
      expect(await verify("v1.2.3")).toBe(0);

      expect(await git("tag", "-a", "nested-target", "-m", "nested target")).toBe(0);
      expect(await git("tag", "--delete", "v1.2.3")).toBe(0);
      expect(await git("tag", "-a", "v1.2.3", "nested-target", "-m", "nested release")).toBe(0);
      expect(await verify("v1.2.3")).not.toBe(0);
      expect(await git("tag", "--delete", "v1.2.3")).toBe(0);
      expect(await git("tag", "-a", "v1.2.3", "-m", "release")).toBe(0);

      expect(await git("switch", "-c", "feature")).toBe(0);
      await writeFile(
        join(fixture, "Cargo.toml"),
        '[package]\nname = "fixture"\nversion = "1.2.4"\n',
      );
      expect(await git("add", "Cargo.toml")).toBe(0);
      expect(await git("commit", "-m", "unreviewed release")).toBe(0);
      expect(await git("tag", "-a", "v1.2.4", "-m", "off-main")).toBe(0);
      expect(await verify("v1.2.4")).not.toBe(0);
      expect(await verify("v1.2.3")).not.toBe(0);
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });

  test("Docker publication promotes signed receipts with immutable version and bounded latest policy", async () => {
    const source = await Bun.file(
      join(repository, ".github/workflows/docker-publish.yml"),
    ).text();
    const workflow = Bun.YAML.parse(source) as {
      on: { repository_dispatch: { types: string[] } };
      permissions: Record<string, string>;
      concurrency: { group: string; "cancel-in-progress": boolean };
      jobs: Record<string, {
        if?: string;
        needs?: string;
        environment?: string;
        outputs?: Record<string, string>;
        permissions?: Record<string, string>;
        steps: Array<{
          name?: string;
          run?: string;
          env?: Record<string, string>;
          with?: Record<string, string>;
        }>;
      }>;
    };

    expect(Object.keys(workflow.on)).toEqual(["repository_dispatch"]);
    expect(workflow.on.repository_dispatch.types).toEqual([
      "ironcrew_docker_publish_v1",
    ]);
    expect(workflow.permissions).toEqual({});
    expect(workflow.concurrency).toEqual({
      group: "release-publication",
      "cancel-in-progress": false,
    });
    expect(workflow.jobs.guard.if).toBeUndefined();
    const firstGuardStep = workflow.jobs.guard.steps[0];
    expect(firstGuardStep?.name).toBe("Bind event to the default-branch workflow");
    expect(firstGuardStep?.env).toEqual({
      DISPATCH_ACTION: "${{ github.event.action }}",
      DEFAULT_BRANCH: "${{ github.event.repository.default_branch }}",
    });
    expect(firstGuardStep?.run).toContain(
      'test "$GITHUB_EVENT_NAME" = repository_dispatch',
    );
    expect(firstGuardStep?.run).toContain(
      'test "$DISPATCH_ACTION" = ironcrew_docker_publish_v1',
    );
    expect(workflow.jobs.guard.outputs).toEqual({
      tag: "${{ steps.dispatch.outputs.tag }}",
      mode: "${{ steps.dispatch.outputs.mode }}",
    });
    expect(workflow.jobs.validate.if).toBe("needs.guard.outputs.mode == 'validate'");
    expect(workflow.jobs.validate.environment).toBe("release");
    expect(workflow.jobs.validate.permissions).toEqual({ contents: "read" });
    const validationCommands = workflow.jobs.validate.steps
      .map((step) => step.run ?? "")
      .join("\n");
    expect(validationCommands).not.toContain("secrets.");
    expect(validationCommands).not.toContain("docker");
    expect(validationCommands).not.toContain("skopeo");
    expect(workflow.jobs.publish.needs).toBe("guard");
    expect(workflow.jobs.publish.if).toBe("needs.guard.outputs.mode == 'publish'");
    expect(workflow.jobs.publish.environment).toBe("release");
    expect(workflow.jobs.publish.permissions).toEqual({
      contents: "read",
    });

    const promotion = workflow.jobs.publish.steps.find(
      (step) => step.name === "Promote signed release image and reconcile latest",
    );
    expect(promotion?.env?.RELEASE_TAG_INPUT).toBe("${{ needs.guard.outputs.tag }}");
    expect(promotion?.run).not.toContain("client_payload");
    expect(promotion?.run).toContain("scripts/promote_release_image.py");
    expect(promotion?.run).toContain("--authorize-latest-reconciliation");
    expect(promotion?.run).toContain("--max-latest-attempts 3");
    expect(promotion?.env?.DOCKERHUB_USERNAME).toBe(
      "${{ secrets.DOCKERHUB_USERNAME }}",
    );
    expect(promotion?.env?.DOCKERHUB_TOKEN).toBe("${{ secrets.DOCKERHUB_TOKEN }}");
    expect(promotion?.env?.GH_TOKEN).toBe("${{ github.token }}");

    expect(source).not.toContain("docker/build-push-action");
    expect(source).not.toContain("docker buildx");
    expect(source).not.toContain("docker build ");
    expect(source).not.toContain("docker/runtime.Dockerfile");
    expect(source).not.toContain("workflow_dispatch");
    expect(source).not.toContain("github.event.client_payload");
    expect(source).toContain("scripts/validate_release_dispatch.py");
    expect(source).toContain("--event-type ironcrew_docker_publish_v1");
    expect(source).toContain('--repository "$GITHUB_REPOSITORY"');
    expect(source).toContain('--actor "$GITHUB_ACTOR"');
    expect(source).toContain(
      "$GITHUB_REPOSITORY/.github/workflows/docker-publish.yml@refs/heads/main",
    );
    for (const job of Object.values(workflow.jobs)) {
      for (const step of job.steps) {
        if (step.uses?.startsWith("actions/checkout@")) {
          expect(step.with?.["persist-credentials"]).toBe(false);
        }
        if (step.uses) {
          expect(step.uses).toMatch(/@[0-9a-f]{40}$/);
          expect(trustedReleaseActions.has(step.uses)).toBeTrue();
        }
      }
    }

    const helper = await Bun.file(
      join(repository, "scripts/promote_release_image.py"),
    ).text();
    const protocol = await Bun.file(
      join(repository, "scripts/release_promotion_protocol.py"),
    ).text();
    const immutability = await Bun.file(
      join(repository, "scripts/dockerhub_immutability.py"),
    ).text();
    expect(helper).toContain("ironcrew-{tag}-linux-oci.tar");
    expect(helper).toContain("ironcrew-{tag}-image-receipt.v1.json");
    expect(helper).toContain("cosign\", \"verify-blob");
    expect(helper).toContain("release.yml@refs/heads/main");
    expect(helper).not.toContain("release.yml@refs/tags/{tag}");
    expect(helper).toContain("verify_release_image.py");
    expect(helper).toContain('"--preserve-digests"');
    expect(protocol).toContain("immutable version tag points to a different digest");
    expect(protocol).toContain("bounded reconciliation loop");
    expect(immutability).toContain("immutable_tags_settings");
    expect(immutability).toContain("SEMVER_IMMUTABILITY_RULE");
  });

  test("release guidance preserves approval and manual publication boundaries", async () => {
    const sources = await Promise.all([
      Bun.file(join(repository, ".agents/skills/release-ironcrew/SKILL.md")).text(),
      Bun.file(join(repository, ".claude/skills/release-workflow/SKILL.md")).text(),
    ]);
    for (const source of sources) {
      expect(source).not.toContain("git reset --hard");
      expect(source).not.toContain("git commit -am");
      expect(source).not.toContain("git push --force");
      expect(source).toContain("GITHUB_TOKEN");
      expect(source).toContain("docker-publish.yml");
      expect(source).toMatch(/owner may create (?:protected )?`v\*` tags/);
    }
  });
});
