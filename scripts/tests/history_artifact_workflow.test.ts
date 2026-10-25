import { describe, expect, test } from "bun:test";

type Step = { id?: string; uses?: string; run?: string; with?: Record<string, unknown> };
type Job = { needs?: string; env?: Record<string, string>; outputs?: Record<string, string>; steps: Step[] };
const workflow = Bun.YAML.parse(await Bun.file(new URL("../../.github/workflows/ci.yml", import.meta.url)).text()) as {
  permissions: Record<string, string>;
  jobs: Record<string, Job>;
};

describe("history reader artifact acceptance boundary", () => {
  test("feature-disabled binary is checked with denied warnings", () => {
    expect(workflow.jobs["rust-default"].steps.some((step) =>
      step.run === "cargo clippy --no-default-features --bin ironcrew -- -D warnings")).toBeTrue();
  });

  test("producer binds checkout and immutable upload to the selected head", () => {
    const job = workflow.jobs["release-build"];
    expect(job.env?.QUALIFIED_COMMIT).toBe("${{ github.event.pull_request.head.sha || github.sha }}");
    expect(job.steps[0].with?.ref).toBe("${{ env.QUALIFIED_COMMIT }}");
    expect(job.steps[0].with?.["persist-credentials"]).toBeFalse();
    expect(job.outputs?.artifact_id).toBe("${{ steps.upload.outputs.artifact-id }}");
    expect(job.outputs?.receipt_sha256).toBe("${{ steps.package.outputs.receipt_sha256 }}");
    expect(job.steps.find((step) => step.id === "upload")?.with?.["if-no-files-found"]).toBe("error");
  });

  test("consumer tests the exact uploaded ID without publication authority", () => {
    const job = workflow.jobs["history-reader-artifact"];
    expect(workflow.permissions).toEqual({ contents: "read" });
    expect(job.needs).toBe("release-build");
    const download = job.steps.find((step) => step.uses?.startsWith("actions/download-artifact@"));
    expect(download?.with?.["artifact-ids"]).toBe("${{ needs.release-build.outputs.artifact_id }}");
    expect(download?.with?.name).toBeUndefined();
    const commands = job.steps.map((step) => step.run ?? "").join("\n");
    expect(commands).toContain("--receipt-sha256 \"$RECEIPT_SHA256\"");
    expect(commands).toContain("scripts/check_history_reader_artifact.py");
    expect(commands).not.toMatch(/gh release|docker push|repository_dispatch/);
  });
});
