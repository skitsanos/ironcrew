import { describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { validatePlainMappingKeys } from "../validate_skills";

const repository = join(import.meta.dir, "../..");

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

  test("release workflow gates builds and scopes publication authority", async () => {
    const source = await Bun.file(
      join(repository, ".github/workflows/release.yml"),
    ).text();
    const workflow = Bun.YAML.parse(source) as {
      permissions: Record<string, string>;
      concurrency: { group: string; "cancel-in-progress": boolean };
      jobs: Record<string, {
        needs?: string;
        permissions?: Record<string, string>;
        steps: Array<{ name?: string; uses?: string; run?: string; with?: Record<string, string> }>;
      }>;
    };
    const guardCommands = workflow.jobs.guard.steps
      .map((step) => step.run ?? "")
      .join("\n");

    expect(workflow.permissions).toEqual({});
    expect(workflow.concurrency).toEqual({
      group: "release-publication",
      "cancel-in-progress": false,
    });
    expect(workflow.jobs.guard.permissions).toEqual({ contents: "read" });
    expect(workflow.jobs.build.permissions).toEqual({ contents: "read" });
    expect(workflow.jobs.release.permissions).toEqual({
      contents: "write",
      "id-token": "write",
    });
    expect(workflow.jobs.build.needs).toBe("guard");
    expect(guardCommands).toContain("git fetch --no-tags origin main");
    expect(guardCommands).toContain(
      "./scripts/verify_release_source.sh \"$TAG_NAME\" refs/remotes/origin/main",
    );
    const immutableAssets = workflow.jobs.release.steps.find(
      (step) => step.name === "Refuse release-asset replacement",
    );
    expect(immutableAssets?.run).toContain(
      'scripts/verify_release_absent.py --repository "$GITHUB_REPOSITORY" --tag "$GITHUB_REF_NAME"',
    );
    expect(source).not.toContain("Create or update release");

    const notes = workflow.jobs.release.steps.find(
      (step) => step.name === "Write release notes from tag annotation",
    );
    expect(notes?.run).toContain("${RUNNER_TEMP}/ironcrew-release-body.md");
    expect(notes?.run).not.toContain("GITHUB_OUTPUT");
    expect(notes?.run).not.toContain("RELEASE_EOF");
    expect(notes?.run).toContain(
      "https://github.com/${GITHUB_REPOSITORY}/.github/workflows/release.yml@refs/tags/${GITHUB_REF_NAME}",
    );
    expect(notes?.run).toContain("--certificate-identity '$CERTIFICATE_IDENTITY'");
    const publisher = workflow.jobs.release.steps.find(
      (step) => step.name === "Create release once",
    );
    expect(publisher?.run).toContain('gh release create "$GITHUB_REF_NAME"');
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
      on: {
        workflow_dispatch: {
          inputs: Record<string, { required?: boolean; type?: string }>;
        };
      };
      permissions: Record<string, string>;
      concurrency: { group: string; "cancel-in-progress": boolean };
      jobs: Record<string, {
        if?: string;
        permissions?: Record<string, string>;
        steps: Array<{
          name?: string;
          run?: string;
          env?: Record<string, string>;
          with?: Record<string, string>;
        }>;
      }>;
    };

    expect(Object.keys(workflow.on)).toEqual(["workflow_dispatch"]);
    expect(workflow.on.workflow_dispatch.inputs.tag).toEqual({
      description: "Exact stable release tag to promote (for example v2.24.0).",
      required: true,
      type: "string",
    });
    expect(workflow.on.workflow_dispatch.inputs.authorize_latest_reconciliation).toEqual({
      description:
        "Authorize exact version promotion and bounded reconciliation of latest to GitHub's current signed stable release.",
      required: true,
      type: "boolean",
      default: false,
    });
    expect(workflow.permissions).toEqual({});
    expect(workflow.concurrency).toEqual({
      group: "release-publication",
      "cancel-in-progress": false,
    });
    expect(workflow.jobs.publish.if).toBe(
      "github.ref == format('refs/heads/{0}', github.event.repository.default_branch) && inputs.authorize_latest_reconciliation == true",
    );
    expect(workflow.jobs.publish.permissions).toEqual({
      contents: "read",
    });

    const promotion = workflow.jobs.publish.steps.find(
      (step) => step.name === "Promote signed release image and reconcile latest",
    );
    expect(promotion?.env?.RELEASE_TAG_INPUT).toBe("${{ inputs.tag }}");
    expect(promotion?.run).not.toContain("${{ inputs.tag }}");
    expect(promotion?.run).not.toContain("github.event.inputs.tag");
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
    expect(helper).toContain("release.yml@refs/tags/{tag}");
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
      expect(source).toContain("protected `v*` tag rules");
    }
  });
});
