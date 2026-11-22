import { afterEach, describe, expect, test } from "bun:test";
import { chmod, copyFile, mkdir, mkdtemp, realpath, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  DEFAULT_MAX_AGE_SECONDS,
  checkPush,
  parseMaxAge,
  parsePushUpdates,
  parseReceipt,
  receiptPath,
  treeOf,
  writeReceipt,
} from "../gate_receipt";

const repository = join(import.meta.dir, "../..");
const zero = "0".repeat(40);
const roots: string[] = [];

// Synthetic repositories must not inherit the caller's repository, hooks,
// signing, or identity configuration.
function gitEnvironment(extra: Record<string, string> = {}): Record<string, string> {
  const environment = Object.fromEntries(
    Object.entries(process.env).filter(([name, value]) => value !== undefined && !name.startsWith("GIT_")),
  ) as Record<string, string>;
  return {
    ...environment,
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_AUTHOR_NAME: "Gate Fixture",
    GIT_AUTHOR_EMAIL: "gate@example.invalid",
    GIT_COMMITTER_NAME: "Gate Fixture",
    GIT_COMMITTER_EMAIL: "gate@example.invalid",
    ...extra,
  };
}

function run(cwd: string, command: string[], extra: Record<string, string> = {}) {
  const result = Bun.spawnSync(command, { cwd, env: gitEnvironment(extra), stdout: "pipe", stderr: "pipe" });
  return { exitCode: result.exitCode, stdout: result.stdout.toString().trim(), stderr: result.stderr.toString() };
}

function git(cwd: string, ...args: string[]): string {
  const result = run(cwd, ["git", ...args]);
  if (result.exitCode !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
  return result.stdout;
}

async function fixture(): Promise<string> {
  const root = await realpath(await mkdtemp(join(tmpdir(), "ironcrew-gate-receipt-")));
  roots.push(root);
  const checkout = join(root, "checkout");
  await mkdir(checkout);
  git(checkout, "init", "--quiet", "--initial-branch=main");
  await writeFile(join(checkout, "file.txt"), "one\n");
  git(checkout, "add", "-A");
  git(checkout, "commit", "--quiet", "-m", "initial");
  return checkout;
}

async function commitContent(checkout: string, content: string): Promise<string> {
  await writeFile(join(checkout, "file.txt"), content);
  git(checkout, "commit", "--quiet", "-am", `content ${content.trim()}`);
  return git(checkout, "rev-parse", "HEAD");
}

function update(object: string, ref = "refs/heads/main") {
  return { localRef: ref, localObject: object, remoteRef: ref };
}

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("gate receipt parsing", () => {
  const valid = {
    version: 1,
    tree: "a".repeat(40),
    commit: "b".repeat(40),
    started: "2026-11-20T10:00:00.000Z",
    completed: "2026-11-20T10:40:00.000Z",
  };

  test("accepts a complete receipt", () => {
    expect(parseReceipt(JSON.stringify(valid))).toEqual(valid);
  });

  for (const [label, text, message] of [
    ["unsupported version", JSON.stringify({ ...valid, version: 2 }), "unsupported receipt version"],
    ["abbreviated tree", JSON.stringify({ ...valid, tree: "abc123" }), "tree must be a full object id"],
    ["missing commit", JSON.stringify({ ...valid, commit: undefined }), "commit must be a full object id"],
    ["invalid timestamp", JSON.stringify({ ...valid, completed: "yesterday" }), "completed must be a timestamp"],
    ["completion before start", JSON.stringify({ ...valid, started: valid.completed, completed: valid.started }),
      "completed before it started"],
    ["array", "[]", "not a JSON object"],
    ["null", "null", "not a JSON object"],
    ["oversized", JSON.stringify({ ...valid, padding: "x".repeat(5000) }), "too large"],
  ] as const) {
    test(`rejects ${label}`, () => {
      expect(() => parseReceipt(text)).toThrow(message);
    });
  }

  test("parses pre-push input and drops deletions", () => {
    const sha = "c".repeat(40);
    expect(parsePushUpdates(`refs/heads/main ${sha} refs/heads/main ${zero}\n`
      + `(delete) ${zero} refs/heads/old ${sha}\n\n`)).toEqual([update(sha)]);
    expect(parsePushUpdates("")).toEqual([]);
    expect(() => parsePushUpdates("refs/heads/main HEAD refs/heads/main 0\n")).toThrow("unexpected pre-push input");
    expect(() => parsePushUpdates(`refs/heads/main ${sha}\n`)).toThrow("unexpected pre-push input");
  });

  test("parses the receipt age limit fail-closed", () => {
    expect(parseMaxAge(undefined)).toBe(DEFAULT_MAX_AGE_SECONDS);
    expect(parseMaxAge("")).toBe(DEFAULT_MAX_AGE_SECONDS);
    expect(parseMaxAge(" 3600 ")).toBe(3600);
    expect(parseMaxAge("0")).toBe(0);
    for (const raw of ["-1", "1h", "1e3", "1.5", "9999999999"]) {
      expect(() => parseMaxAge(raw)).toThrow("must be a non-negative integer");
    }
  });
});

describe("gate receipt tree binding", () => {
  test("records the clean starting tree in the private Git directory", async () => {
    const checkout = await fixture();
    const head = git(checkout, "rev-parse", "HEAD");
    const tree = treeOf(checkout, head);
    const receipt = await writeReceipt(checkout, tree, new Date(Date.now() - 1000));
    expect(receipt).toMatchObject({ version: 1, tree, commit: head });
    const path = receiptPath(checkout);
    expect(path).toBe(join(checkout, ".git", "ironcrew-gate-receipt.json"));
    expect((await stat(path)).mode & 0o777).toBe(0o600);
    expect(git(checkout, "status", "--porcelain=v1", "--untracked-files=all")).toBe("");
    expect(await checkPush(checkout, [update(head)])).toMatchObject({ accepted: true });
  });

  test("refuses a receipt when the tree changed during the run", async () => {
    const checkout = await fixture();
    const startTree = git(checkout, "rev-parse", "HEAD^{tree}");
    await commitContent(checkout, "committed during the gate\n");
    await expect(writeReceipt(checkout, startTree, new Date())).rejects.toThrow("tree changed during the gate");
    expect(await Bun.file(receiptPath(checkout)).exists()).toBe(false);
  });

  test("refuses tracked and untracked changes", async () => {
    const checkout = await fixture();
    const startTree = git(checkout, "rev-parse", "HEAD^{tree}");
    await writeFile(join(checkout, "untracked.txt"), "new\n");
    await expect(writeReceipt(checkout, startTree, new Date())).rejects.toThrow("uncommitted or untracked");
    await rm(join(checkout, "untracked.txt"));
    await writeFile(join(checkout, "file.txt"), "edited\n");
    await expect(writeReceipt(checkout, startTree, new Date())).rejects.toThrow("uncommitted or untracked");
  });

  test("keeps a message-only amend and rejects any content change", async () => {
    const checkout = await fixture();
    await writeReceipt(checkout, git(checkout, "rev-parse", "HEAD^{tree}"), new Date());
    git(checkout, "commit", "--quiet", "--amend", "-m", "reworded");
    expect(await checkPush(checkout, [update(git(checkout, "rev-parse", "HEAD"))])).toMatchObject({ accepted: true });
    const changed = await commitContent(checkout, "two\n");
    const verdict = await checkPush(checkout, [update(changed)]);
    expect(verdict.accepted).toBe(false);
    if (!verdict.accepted) expect(verdict.reason).toContain("but the gate validated tree");
  });
});

describe("pre-push receipt decision", () => {
  test("blocks a missing receipt and allows deletion-only pushes", async () => {
    const checkout = await fixture();
    const head = git(checkout, "rev-parse", "HEAD");
    expect(await checkPush(checkout, [update(head)])).toEqual({
      accepted: false, reason: "no gate receipt exists for this worktree",
    });
    expect(await checkPush(checkout, [])).toMatchObject({ accepted: true });
  });

  test("blocks stale, future-dated, unreadable, and unresolved receipts", async () => {
    const checkout = await fixture();
    const head = git(checkout, "rev-parse", "HEAD");
    const completed = new Date("2026-11-20T12:00:00.000Z");
    await writeReceipt(checkout, treeOf(checkout, head), new Date(completed.getTime() - 60_000), completed);
    const at = (seconds: number) => new Date(completed.getTime() + seconds * 1000);
    expect(await checkPush(checkout, [update(head)], at(3600), 3600)).toMatchObject({ accepted: true });
    expect(await checkPush(checkout, [update(head)], at(3601), 3600)).toEqual({
      accepted: false, reason: "the gate receipt is 3601s old; the limit is 3600s",
    });
    expect(await checkPush(checkout, [update(head)], at(-5))).toEqual({
      accepted: false, reason: "the gate receipt is dated in the future",
    });
    expect(await checkPush(checkout, [update("d".repeat(40))], at(1))).toMatchObject({
      accepted: false, reason: "refs/heads/main does not resolve to a tree",
    });
    await writeFile(receiptPath(checkout), "{not json");
    const verdict = await checkPush(checkout, [update(head)], at(1));
    expect(verdict.accepted).toBe(false);
    if (!verdict.accepted) expect(verdict.reason).toContain("the gate receipt is unreadable");
  });

  test("requires every pushed ref to carry the validated tree", async () => {
    const checkout = await fixture();
    const validated = git(checkout, "rev-parse", "HEAD");
    await writeReceipt(checkout, treeOf(checkout, validated), new Date());
    git(checkout, "tag", "-a", "v0.0.1", "-m", "annotated tag of the validated commit");
    const tag = git(checkout, "rev-parse", "v0.0.1");
    expect(await checkPush(checkout, [update(validated), update(tag, "refs/tags/v0.0.1")]))
      .toMatchObject({ accepted: true });
    const other = await commitContent(checkout, "three\n");
    const verdict = await checkPush(checkout, [update(validated), update(other, "refs/heads/topic")]);
    expect(verdict.accepted).toBe(false);
    if (!verdict.accepted) expect(verdict.reason).toStartWith("refs/heads/topic carries tree");
  });
});

describe("tracked pre-push hook", () => {
  async function hookFixture() {
    const checkout = await fixture();
    const remote = join(checkout, "..", "remote.git");
    git(checkout, "init", "--quiet", "--bare", remote);
    git(checkout, "remote", "add", "origin", remote);
    await mkdir(join(checkout, ".githooks"));
    await mkdir(join(checkout, "scripts"));
    for (const file of [".githooks/pre-push", "scripts/gate_receipt.ts"]) {
      await copyFile(join(repository, file), join(checkout, file));
    }
    await chmod(join(checkout, ".githooks/pre-push"), 0o755);
    git(checkout, "add", "-A");
    git(checkout, "commit", "--quiet", "-m", "hook");
    const push = (refspec: string[], extra: Record<string, string> = {}) =>
      run(checkout, ["git", "-c", "core.hooksPath=.githooks", "push", "--quiet", "origin", ...refspec], extra);
    const recordGate = () => run(checkout, [
      "bun", "--no-env-file", "scripts/gate_receipt.ts", "write",
      git(checkout, "rev-parse", "HEAD^{tree}"), new Date().toISOString(),
    ]);
    const remoteHead = (ref: string) => run(remote, ["git", "rev-parse", "--verify", "--quiet", ref]).stdout;
    return { checkout, push, recordGate, remoteHead };
  }

  test("a real push needs a receipt for the pushed tree and never runs the gate", async () => {
    const { checkout, push, recordGate, remoteHead } = await hookFixture();
    const blocked = push(["main"]);
    expect(blocked.exitCode).not.toBe(0);
    expect(blocked.stderr).toContain("pre-push: push blocked: no gate receipt exists for this worktree.");
    expect(blocked.stderr).toContain("run ./scripts/pre-push-check.sh");
    expect(remoteHead("refs/heads/main")).toBe("");

    const recorded = recordGate();
    expect(recorded.exitCode).toBe(0);
    expect(recorded.stdout).toContain("pre-push: gate receipt recorded for tree");
    const accepted = push(["main"]);
    expect(accepted.exitCode).toBe(0);
    expect(accepted.stdout + accepted.stderr).toContain("pre-push: gate receipt accepted: tree");
    expect(remoteHead("refs/heads/main")).toBe(git(checkout, "rev-parse", "HEAD"));

    const validated = git(checkout, "rev-parse", "HEAD");
    await commitContent(checkout, "unvalidated\n");
    const changed = push(["main"]);
    expect(changed.exitCode).not.toBe(0);
    expect(changed.stderr).toContain("but the gate validated tree");
    expect(remoteHead("refs/heads/main")).toBe(validated);

    const malformedLimit = push([`${validated}:refs/heads/copy`], { IRONCREW_GATE_RECEIPT_MAX_AGE_SECONDS: "1d" });
    expect(malformedLimit.exitCode).not.toBe(0);
    expect(malformedLimit.stderr).toContain("must be a non-negative integer");
    expect(remoteHead("refs/heads/copy")).toBe("");

    expect(push([`${validated}:refs/heads/stale-branch`]).exitCode).toBe(0);
    const deletion = push([":refs/heads/stale-branch"]);
    expect(deletion.exitCode).toBe(0);
    expect(remoteHead("refs/heads/stale-branch")).toBe("");
  });
});

describe("gate wiring", () => {
  test("the gate records the receipt and the hook only verifies it", async () => {
    const gate = await Bun.file(join(repository, "scripts/pre-push-check.sh")).text();
    const hook = await Bun.file(join(repository, ".githooks/pre-push")).text();
    const start = gate.indexOf("gate_start_tree=$(git rev-parse --verify 'HEAD^{tree}')");
    const write = gate.indexOf('scripts/gate_receipt.ts" write "$gate_start_tree" "$gate_started"');
    expect(start).toBeGreaterThan(gate.indexOf("commit or otherwise reconcile every tracked and untracked change"));
    expect(start).toBeLessThan(gate.indexOf('run "latest-stable tool and dependency refresh"'));
    expect(write).toBeGreaterThan(gate.indexOf('cmp -s "$scratch_root/worktree-before"'));
    expect(write).toBeGreaterThan(gate.indexOf('run "locked release build"'));
    expect(write).toBeLessThan(gate.indexOf("every locally reproducible CI gate passed"));
    expect(hook).toContain('exec bun --no-env-file "$hook_root/scripts/gate_receipt.ts" verify-push');
    expect(hook).not.toMatch(/^[^#]*pre-push-check\.sh/m);
  });
});
