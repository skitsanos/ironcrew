import { describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const repository = join(import.meta.dir, "../..");
type Step = { name: string; run: string; env?: Record<string, string> };
type Job = {
  needs?: string | string[];
  if?: string;
  environment?: string;
  permissions?: Record<string, string>;
  env?: Record<string, string>;
  steps: Step[];
};
type Workflow = {
  concurrency: { group: string; "cancel-in-progress": boolean };
  jobs: Record<string, Job>;
};

async function workflow(name: string): Promise<Workflow> {
  return Bun.YAML.parse(await Bun.file(
    join(repository, ".github/workflows", name),
  ).text()) as Workflow;
}

async function executeHandoff(overrides: Record<string, string> = {}) {
  const fixture = await mkdtemp(join(tmpdir(), "ironcrew-promotion-handoff-"));
  try {
    const argsPath = join(fixture, "gh-args");
    const summaryPath = join(fixture, "summary.md");
    await writeFile(summaryPath, "");
    await writeFile(join(fixture, "gh"), [
      "#!/usr/bin/env bash",
      'printf "%s\\n" "$@" >> "$GH_ARGS_FILE"',
      'exit "${GH_STUB_EXIT:-0}"',
      "",
    ].join("\n"), { mode: 0o700 });
    const release = await workflow("release.yml");
    const handoff = release.jobs["docker-promotion"].steps[0];
    const process = Bun.spawn(["bash", "-e", "-o", "pipefail", "-c", handoff.run], {
      cwd: fixture,
      env: {
        PATH: `${fixture}:${Bun.env.PATH}`,
        HOME: fixture,
        GH_ARGS_FILE: argsPath,
        GH_TOKEN: "offline-handoff-fixture",
        GITHUB_EVENT_NAME: "repository_dispatch",
        DISPATCH_ACTION: "ironcrew_release_v1",
        GITHUB_REPOSITORY: "skitsanos/ironcrew",
        GITHUB_REF: "refs/heads/main",
        GITHUB_WORKFLOW_REF: "skitsanos/ironcrew/.github/workflows/release.yml@refs/heads/main",
        GITHUB_SHA: "a".repeat(40),
        GITHUB_WORKFLOW_SHA: "a".repeat(40),
        GITHUB_STEP_SUMMARY: summaryPath,
        RELEASE_TAG: "v9.8.7",
        ...overrides,
      },
      stdout: "ignore",
      stderr: "ignore",
    });
    const code = await process.exited;
    const args = await Bun.file(argsPath).exists()
      ? (await Bun.file(argsPath).text()).trimEnd().split("\n")
      : [];
    return { code, args, summary: await Bun.file(summaryPath).text() };
  } finally {
    await rm(fixture, { recursive: true, force: true });
  }
}

describe("release Docker promotion handoff", () => {
  test("only successful publish mode can request the protected publisher", async () => {
    const release = await workflow("release.yml");
    const handoff = release.jobs["docker-promotion"];
    expect(handoff.needs).toEqual(["guard", "release"]);
    expect(handoff.if).toBe(
      "needs.guard.outputs.mode == 'publish' && needs.release.result == 'success'",
    );
    expect(release.jobs.release.environment).toBe("release");
    expect(handoff.permissions).toEqual({ contents: "write" });
    expect(handoff.env).toEqual({ RELEASE_TAG: "${{ needs.guard.outputs.tag }}" });
    expect(handoff.steps).toHaveLength(1);
    expect(handoff.steps[0].env).toEqual({
      GH_TOKEN: "${{ github.token }}",
      DISPATCH_ACTION: "${{ github.event.action }}",
    });
    expect(JSON.stringify(handoff)).not.toContain("secrets.");

    const docker = await workflow("docker-publish.yml");
    expect(docker.jobs.publish.environment).toBe("release");
    expect(docker.jobs.publish.permissions).toEqual({ contents: "read" });
    expect(docker.concurrency).toEqual(release.concurrency);
    expect(release.concurrency["cancel-in-progress"]).toBeFalse();
  });

  test("dispatches the validated tag once and ends without waiting on the shared group", async () => {
    const result = await executeHandoff();
    expect(result.code).toBe(0);
    expect(result.args).toEqual([
      "api", "--method", "POST", "repos/skitsanos/ironcrew/dispatches",
      "-f", "event_type=ironcrew_docker_publish_v1",
      "-f", "client_payload[tag]=v9.8.7",
      "-f", "client_payload[mode]=publish",
    ]);
    expect(result.summary).toContain("v9.8.7");
    expect(result.summary).toContain("Approve Docker Publish");
    expect(result.summary).toContain("verify version/latest digests");
  });

  for (const [name, values] of [
    ["different event", { GITHUB_EVENT_NAME: "push" }],
    ["different dispatch", { DISPATCH_ACTION: "ironcrew_docker_publish_v1" }],
    ["off-main ref", { GITHUB_REF: "refs/heads/feature" }],
    ["tag-selected workflow", {
      GITHUB_WORKFLOW_REF: "skitsanos/ironcrew/.github/workflows/release.yml@refs/tags/v9.8.7",
    }],
    ["workflow/source drift", { GITHUB_WORKFLOW_SHA: "b".repeat(40) }],
  ] as Array<[string, Record<string, string>]>) {
    test(`rejects ${name} before sending a dispatch`, async () => {
      const result = await executeHandoff(values);
      expect(result.code).not.toBe(0);
      expect(result.args).toEqual([]);
      expect(result.summary).toBe("");
    });
  }

  test("dispatch failure fails the retryable handoff without reporting completion", async () => {
    const result = await executeHandoff({ GH_STUB_EXIT: "23" });
    expect(result.code).toBe(23);
    expect(result.args).toContain("event_type=ironcrew_docker_publish_v1");
    expect(result.summary).toBe("");
  });

  test("the release procedure requires Docker completion under the release authorization", async () => {
    const skill = await Bun.file(
      join(repository, ".agents/skills/release-ironcrew/SKILL.md"),
    ).text();
    expect(skill).toContain("A standard release includes GitHub publication and Docker Hub promotion");
    expect(skill).toContain("Do not report a standard release complete");
    expect(skill).toContain("linux/amd64");
    expect(skill).toContain("linux/arm64");
    expect(skill).toContain("manifest/config digests");
    expect(skill).toContain("retry only its job");
  });
});
