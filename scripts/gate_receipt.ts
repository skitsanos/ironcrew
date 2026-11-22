#!/usr/bin/env bun
// IC-059: the complete pre-push gate takes longer than a hosting provider keeps
// an idle SSH push session open, so running it inside `git push` lost passing
// runs. `scripts/pre-push-check.sh` therefore records a receipt for the exact
// clean tree it validated, and `.githooks/pre-push` only checks that a fresh
// receipt covers every tree being pushed.

import { rename, writeFile } from "node:fs/promises";
import { resolve } from "node:path";

export const RECEIPT_FILE = "ironcrew-gate-receipt.json";
export const RECEIPT_VERSION = 1;
export const DEFAULT_MAX_AGE_SECONDS = 24 * 60 * 60;
export const MAX_AGE_VARIABLE = "IRONCREW_GATE_RECEIPT_MAX_AGE_SECONDS";
const MAX_RECEIPT_BYTES = 4096;
const OBJECT_ID = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;
const ZERO_ID = /^0+$/;

export type Receipt = {
  version: typeof RECEIPT_VERSION;
  tree: string;
  commit: string;
  started: string;
  completed: string;
};

export type PushUpdate = { localRef: string; localObject: string; remoteRef: string };

export type Verdict =
  | { accepted: true; detail: string }
  | { accepted: false; reason: string };

/** Variables that pin git to one repository (`git rev-parse --local-env-vars`).
 *  Git exports some of them to hooks, so child commands drop them and
 *  rediscover the repository from their working directory. */
const REPOSITORY_SCOPED_GIT_VARIABLES = new Set([
  "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_COMMON_DIR", "GIT_CONFIG", "GIT_CONFIG_COUNT",
  "GIT_CONFIG_PARAMETERS", "GIT_DIR", "GIT_GRAFT_FILE", "GIT_IMPLICIT_WORK_TREE",
  "GIT_INDEX_FILE", "GIT_NO_REPLACE_OBJECTS", "GIT_OBJECT_DIRECTORY", "GIT_PREFIX",
  "GIT_REPLACE_REF_BASE", "GIT_SHALLOW_FILE", "GIT_WORK_TREE",
]);

function git(repository: string, args: string[]): string {
  const env = Object.fromEntries(Object.entries(process.env).filter(
    ([name, value]) => value !== undefined && !REPOSITORY_SCOPED_GIT_VARIABLES.has(name),
  )) as Record<string, string>;
  const result = Bun.spawnSync(["git", ...args], { cwd: repository, env, stdout: "pipe", stderr: "pipe" });
  if (result.exitCode !== 0) {
    const detail = result.stderr.toString().trim();
    throw new Error(detail || `git ${args[0]} failed`);
  }
  return result.stdout.toString().trim();
}

/** Per-worktree Git directory: never tracked, survives `cargo clean`. */
export function receiptPath(repository: string): string {
  return resolve(repository, git(repository, ["rev-parse", "--git-path", RECEIPT_FILE]));
}

export function treeOf(repository: string, object: string): string {
  if (!OBJECT_ID.test(object)) throw new Error("expected a full hexadecimal object id");
  return git(repository, ["rev-parse", "--verify", "--quiet", `${object}^{tree}`]);
}

/** The tree of HEAD, refusing any tracked or untracked change. */
export function cleanTree(repository: string): { commit: string; tree: string } {
  if (git(repository, ["status", "--porcelain=v1", "--untracked-files=all"]) !== "") {
    throw new Error("the worktree has uncommitted or untracked changes");
  }
  const commit = git(repository, ["rev-parse", "--verify", "HEAD"]);
  return { commit, tree: treeOf(repository, commit) };
}

/** Record a receipt only when the clean tree at the end of the gate is the tree
 *  captured before its first check. The write is atomic within the Git dir. */
export async function writeReceipt(
  repository: string,
  startTree: string,
  started: Date,
  now: Date = new Date(),
): Promise<Receipt> {
  if (!OBJECT_ID.test(startTree)) throw new Error("the start tree must be a full hexadecimal object id");
  const { commit, tree } = cleanTree(repository);
  if (tree !== startTree) {
    throw new Error(`the tree changed during the gate from ${short(startTree)} to ${short(tree)}; rerun the gate`);
  }
  if (Number.isNaN(started.getTime()) || started > now) throw new Error("invalid gate start time");
  const receipt: Receipt = {
    version: RECEIPT_VERSION, tree, commit, started: started.toISOString(), completed: now.toISOString(),
  };
  const path = receiptPath(repository);
  const temporary = `${path}.${process.pid}.tmp`;
  await writeFile(temporary, `${JSON.stringify(receipt, null, 2)}\n`, { mode: 0o600 });
  await rename(temporary, path);
  return receipt;
}

export function parseReceipt(text: string): Receipt {
  if (Buffer.byteLength(text) > MAX_RECEIPT_BYTES) throw new Error("the receipt is too large");
  const value = JSON.parse(text) as Record<string, unknown>;
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("not a JSON object");
  if (value.version !== RECEIPT_VERSION) throw new Error("unsupported receipt version");
  const { tree, commit, started, completed } = value;
  for (const [name, field] of [["tree", tree], ["commit", commit]] as const) {
    if (typeof field !== "string" || !OBJECT_ID.test(field)) throw new Error(`${name} must be a full object id`);
  }
  for (const [name, field] of [["started", started], ["completed", completed]] as const) {
    if (typeof field !== "string" || Number.isNaN(Date.parse(field))) throw new Error(`${name} must be a timestamp`);
  }
  if (Date.parse(started as string) > Date.parse(completed as string)) {
    throw new Error("the gate completed before it started");
  }
  return { version: RECEIPT_VERSION, tree, commit, started, completed } as Receipt;
}

/** Parse the `<local ref> <local object> <remote ref> <remote object>` lines
 *  git writes to a pre-push hook. Deletions carry no content and are omitted. */
export function parsePushUpdates(input: string): PushUpdate[] {
  const updates: PushUpdate[] = [];
  for (const line of input.split("\n")) {
    if (line.trim() === "") continue;
    const fields = line.trim().split(/\s+/);
    if (fields.length !== 4 || !OBJECT_ID.test(fields[1]) || !OBJECT_ID.test(fields[3])) {
      throw new Error(`unexpected pre-push input line: ${JSON.stringify(line.slice(0, 200))}`);
    }
    const [localRef, localObject, remoteRef] = fields;
    if (!ZERO_ID.test(localObject)) updates.push({ localRef, localObject, remoteRef });
  }
  return updates;
}

export function parseMaxAge(raw: string | undefined): number {
  if (raw === undefined || raw.trim() === "") return DEFAULT_MAX_AGE_SECONDS;
  if (!/^\d{1,9}$/.test(raw.trim())) throw new Error(`${MAX_AGE_VARIABLE} must be a non-negative integer`);
  return Number(raw.trim());
}

/** Accept a push only when a readable, fresh receipt validated the tree of
 *  every pushed object. A message-only amend keeps its tree; content does not. */
export async function checkPush(
  repository: string,
  updates: PushUpdate[],
  now: Date = new Date(),
  maxAgeSeconds: number = DEFAULT_MAX_AGE_SECONDS,
): Promise<Verdict> {
  if (updates.length === 0) return { accepted: true, detail: "no pushed content; only deletions" };
  const path = receiptPath(repository);
  const file = Bun.file(path);
  if (!(await file.exists())) return { accepted: false, reason: "no gate receipt exists for this worktree" };
  let receipt: Receipt;
  try {
    receipt = parseReceipt(await file.text());
  } catch (error) {
    return { accepted: false, reason: `the gate receipt is unreadable: ${describe(error)}` };
  }
  const ageSeconds = Math.floor((now.getTime() - Date.parse(receipt.completed)) / 1000);
  if (ageSeconds < 0) return { accepted: false, reason: "the gate receipt is dated in the future" };
  if (ageSeconds > maxAgeSeconds) {
    return { accepted: false, reason: `the gate receipt is ${ageSeconds}s old; the limit is ${maxAgeSeconds}s` };
  }
  for (const update of updates) {
    let tree: string;
    try {
      tree = treeOf(repository, update.localObject);
    } catch {
      return { accepted: false, reason: `${update.localRef} does not resolve to a tree` };
    }
    if (tree !== receipt.tree) {
      return {
        accepted: false,
        reason: `${update.localRef} carries tree ${short(tree)}, but the gate validated tree `
          + `${short(receipt.tree)} (commit ${short(receipt.commit)})`,
      };
    }
  }
  return {
    accepted: true,
    detail: `tree ${short(receipt.tree)} passed the gate ${ageSeconds}s ago (commit ${short(receipt.commit)})`,
  };
}

function short(id: string): string {
  return id.slice(0, 12);
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function main(argv: string[]): Promise<number> {
  const repository = git(process.cwd(), ["rev-parse", "--show-toplevel"]);
  const [command, ...rest] = argv;
  if (command === "write" && rest.length === 2) {
    const receipt = await writeReceipt(repository, rest[0], new Date(rest[1]));
    console.log(`pre-push: gate receipt recorded for tree ${short(receipt.tree)} (commit ${short(receipt.commit)}).`);
    return 0;
  }
  if (command === "verify-push" && rest.length === 0) {
    const updates = parsePushUpdates(await Bun.stdin.text());
    const verdict = await checkPush(repository, updates, new Date(), parseMaxAge(process.env[MAX_AGE_VARIABLE]));
    if (verdict.accepted) {
      console.log(`pre-push: gate receipt accepted: ${verdict.detail}.`);
      return 0;
    }
    console.error(`pre-push: push blocked: ${verdict.reason}.`);
    console.error("pre-push: run ./scripts/pre-push-check.sh (or `task pre-push`) on the commit to push;");
    console.error("pre-push: a passing run records the receipt, then push again.");
    return 1;
  }
  console.error("usage: gate_receipt.ts write <start-tree> <start-time> | verify-push < pre-push input");
  return 2;
}

if (import.meta.main) {
  main(Bun.argv.slice(2)).then(
    (code) => process.exit(code),
    (error) => {
      console.error(`pre-push: gate receipt failed closed: ${describe(error)}`);
      process.exit(1);
    },
  );
}
