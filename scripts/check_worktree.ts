#!/usr/bin/env bun

import { lstat } from "node:fs/promises";
import { resolve } from "node:path";

type CommandResult = {
  exitCode: number;
  output: string;
};

const whitespacePolicy = "blank-at-eol,blank-at-eof,space-before-tab";

async function git(repository: string, args: string[]): Promise<CommandResult> {
  const process = Bun.spawn(["git", "-c", `core.whitespace=${whitespacePolicy}`, ...args], {
    cwd: repository,
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(process.stdout).text(),
    new Response(process.stderr).text(),
    process.exited,
  ]);
  return { exitCode, output: `${stdout}${stderr}` };
}

function untrackedPaths(status: string): string[] {
  return status
    .split("\0")
    .filter((entry) => entry.startsWith("?? "))
    .map((entry) => entry.slice(3));
}

async function isTextFile(path: string): Promise<boolean> {
  const metadata = await lstat(path);
  if (!metadata.isFile()) return false;
  const prefix = new Uint8Array(await Bun.file(path).slice(0, 8192).arrayBuffer());
  if (prefix.includes(0)) return false;
  try {
    new TextDecoder("utf-8", { fatal: true }).decode(prefix);
    return true;
  } catch {
    return false;
  }
}

export async function checkWorktree(repository: string): Promise<string[]> {
  const errors: string[] = [];
  const configuredBase = process.env.IRONCREW_POLICY_BASE_SHA?.trim();
  if (configuredBase && !/^0+$/.test(configuredBase)) {
    if (!/^[0-9a-f]{40,64}$/i.test(configuredBase)) {
      errors.push("IRONCREW_POLICY_BASE_SHA must be a full Git commit SHA");
    } else {
      const verify = await git(repository, [
        "rev-parse",
        "--verify",
        "--quiet",
        `${configuredBase}^{commit}`,
      ]);
      if (verify.exitCode !== 0) {
        errors.push(`IRONCREW_POLICY_BASE_SHA commit ${configuredBase} is unavailable`);
      } else {
        const committed = await git(repository, [
          "diff",
          "--check",
          `${configuredBase}..HEAD`,
          "--",
        ]);
        if (committed.exitCode !== 0) {
          errors.push(
            committed.output.trim() || "trusted-base committed whitespace check failed",
          );
        }
      }
    }
  }
  for (const args of [["diff", "--check"], ["diff", "--cached", "--check"]]) {
    const result = await git(repository, args);
    if (result.exitCode !== 0) {
      errors.push(result.output.trim() || `git ${args.join(" ")} failed`);
    }
  }

  const status = await git(repository, ["status", "--porcelain=v1", "-z", "--untracked-files=all"]);
  if (status.exitCode !== 0) {
    errors.push(status.output.trim() || "git status failed");
    return errors;
  }

  for (const relativePath of untrackedPaths(status.output)) {
    const path = resolve(repository, relativePath);
    if (!(await isTextFile(path))) continue;
    const result = await git(repository, [
      "diff",
      "--no-index",
      "--check",
      "--",
      "/dev/null",
      relativePath,
    ]);
    // A clean no-index comparison exits 1 because the file is new. Whitespace
    // errors produce diagnostics; execution failures use a code above 1.
    if (result.exitCode > 1 || result.output.trim()) {
      errors.push(result.output.trim() || `could not inspect untracked file ${relativePath}`);
    }
  }
  return errors;
}

if (import.meta.main) {
  const repository = resolve(Bun.argv[2] ?? ".");
  const errors = await checkWorktree(repository);
  if (errors.length) {
    for (const error of errors) console.error(`worktree validation: ${error}`);
    process.exit(1);
  }
  console.log(
    "Worktree whitespace validation passed for trusted-base, staged, and untracked text files.",
  );
}
