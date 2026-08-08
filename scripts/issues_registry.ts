#!/usr/bin/env bun

import { basename, join } from "node:path";

export type IssueStatus = "open" | "in-progress" | "resolved";

export type Issue = {
  id: string;
  title: string;
  priority: string;
  status: IssueStatus;
  area: string;
  resolved?: string;
  path: string;
  body: string;
};

type YamlMap = Record<string, unknown>;

const repository = join(import.meta.dir, "..");
const issuesDirectory = join(repository, "docs/issues");
const registryPath = join(issuesDirectory, "README.md");
const compatibilityPath = join(repository, "ISSUES.md");
const auditEvidencePath = join(issuesDirectory, "AUDIT_EVIDENCE.md");
const highWaterMarkPath = join(issuesDirectory, "HIGH_WATER_MARK");
const issuePattern = /^IC-(?!000)\d{3}$/;
const allowedFields = new Set(["id", "title", "priority", "status", "area", "resolved"]);

function asMap(value: unknown): YamlMap | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
  return value as YamlMap;
}

function stringField(map: YamlMap, key: string): string | undefined {
  const value = map[key];
  return typeof value === "string" && value.trim() ? value.trim() : undefined;
}

function validIsoDate(value: string | undefined): boolean {
  if (!value || !/^\d{4}-\d{2}-\d{2}$/.test(value)) return false;
  const date = new Date(`${value}T00:00:00Z`);
  return !Number.isNaN(date.valueOf()) && date.toISOString().slice(0, 10) === value;
}

function validateFrontmatterKeys(path: string, source: string): string[] {
  const errors: string[] = [];
  const seen = new Set<string>();
  for (const [index, line] of source.split(/\r?\n/).entries()) {
    if (!line.trim() || line.trimStart().startsWith("#") || /^\s/.test(line)) continue;
    const match = line.match(/^([A-Za-z_][A-Za-z0-9_-]*):(?:\s|$)/);
    if (!match) {
      errors.push(
        `${path}: frontmatter line ${index + 1} must use one plain top-level mapping key`,
      );
      continue;
    }
    const key = match[1];
    if (seen.has(key)) errors.push(`${path}: duplicate frontmatter field '${key}'`);
    seen.add(key);
  }
  return errors;
}

export function parseHighWaterMark(source: string): { id?: string; errors: string[] } {
  const id = source.trim();
  if (!issuePattern.test(id)) {
    return {
      errors: ["docs/issues/HIGH_WATER_MARK: expected one issued id in IC-NNN format"],
    };
  }
  return { id, errors: [] };
}

export function validateIssueSequence(issueIds: string[], highWaterId: string): string[] {
  const errors: string[] = [];
  const ids = new Set(issueIds);
  const highWater = Number(highWaterId.slice(3));
  for (let index = 1; index <= highWater; index += 1) {
    const id = `IC-${String(index).padStart(3, "0")}`;
    if (!ids.has(id)) errors.push(`docs/issues: missing ${id} below persisted high-water mark`);
  }
  for (const id of ids) {
    if (Number(id.slice(3)) > highWater) {
      errors.push(
        `docs/issues/HIGH_WATER_MARK: ${highWaterId} is behind issued finding ${id}`,
      );
    }
  }
  return errors;
}

export function validateHighWaterProgression(
  currentHighWaterId: string,
  trustedBaseHighWaterId: string,
): string[] {
  const current = Number(currentHighWaterId.slice(3));
  const trustedBase = Number(trustedBaseHighWaterId.slice(3));
  if (current >= trustedBase) return [];
  return [
    `docs/issues/HIGH_WATER_MARK: ${currentHighWaterId} is behind trusted base ${trustedBaseHighWaterId}`,
  ];
}

async function highWaterMarkAtTrustedBase(): Promise<{
  id?: string;
  errors: string[];
}> {
  const configuredBase = process.env.IRONCREW_POLICY_BASE_SHA?.trim();
  const base = configuredBase || "HEAD";
  if (/^0+$/.test(base)) return { errors: [] };
  if (base !== "HEAD" && !/^[0-9a-f]{40,64}$/i.test(base)) {
    return {
      errors: ["IRONCREW_POLICY_BASE_SHA: expected a full Git commit SHA"],
    };
  }

  const verify = Bun.spawn(
    ["git", "rev-parse", "--verify", "--quiet", `${base}^{commit}`],
    { cwd: repository, stdout: "ignore", stderr: "ignore" },
  );
  if (await verify.exited !== 0) {
    return configuredBase
      ? { errors: [`IRONCREW_POLICY_BASE_SHA: commit ${base} is unavailable`] }
      : { errors: [] };
  }

  const object = `${base}:docs/issues/HIGH_WATER_MARK`;
  const exists = Bun.spawn(
    ["git", "cat-file", "-e", object],
    { cwd: repository, stdout: "ignore", stderr: "ignore" },
  );
  if (await exists.exited !== 0) return { errors: [] };

  const show = Bun.spawn(
    ["git", "show", object],
    { cwd: repository, stdout: "pipe", stderr: "ignore" },
  );
  const source = await new Response(show.stdout).text();
  if (await show.exited !== 0) {
    return { errors: [`docs/issues/HIGH_WATER_MARK: cannot read trusted base ${base}`] };
  }
  const parsed = parseHighWaterMark(source);
  return {
    id: parsed.id,
    errors: parsed.errors.map((error) => `${error} at trusted base ${base}`),
  };
}

export function parseIssuePage(path: string, source: string): { issue?: Issue; errors: string[] } {
  const errors: string[] = [];
  const match = source.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]+)$/);
  if (!match) return { errors: [`${path}: expected YAML frontmatter and a non-empty body`] };

  errors.push(...validateFrontmatterKeys(path, match[1]));

  let metadata: YamlMap | undefined;
  try {
    metadata = asMap(Bun.YAML.parse(match[1]));
  } catch (error) {
    errors.push(`${path}: invalid YAML frontmatter: ${String(error)}`);
  }
  if (!metadata) return { errors: [...errors, `${path}: frontmatter must be a mapping`] };

  const unexpected = Object.keys(metadata).filter((key) => !allowedFields.has(key));
  if (unexpected.length) errors.push(`${path}: unsupported fields: ${unexpected.join(", ")}`);

  const id = stringField(metadata, "id");
  const title = stringField(metadata, "title");
  const priority = stringField(metadata, "priority");
  const status = stringField(metadata, "status") as IssueStatus | undefined;
  const area = stringField(metadata, "area");
  const resolved = stringField(metadata, "resolved");
  const expectedId = basename(path, ".md");

  if (!id || !issuePattern.test(id)) errors.push(`${path}: id must match IC-NNN`);
  if (id && id !== expectedId) errors.push(`${path}: id ${id} must match filename ${expectedId}`);
  if (!title) errors.push(`${path}: title must be a non-empty string`);
  if (!priority || !/^P[0-3]$/.test(priority)) errors.push(`${path}: priority must be P0, P1, P2, or P3`);
  if (!status || !["open", "in-progress", "resolved"].includes(status)) {
    errors.push(`${path}: status must be open, in-progress, or resolved`);
  }
  if (!area) errors.push(`${path}: area must be a non-empty string`);
  if (status === "resolved" && !validIsoDate(resolved)) {
    errors.push(`${path}: resolved issues require a valid ISO resolved date`);
  }
  if (status !== "resolved" && resolved) errors.push(`${path}: unresolved issues cannot set resolved`);

  const body = match[2].trimEnd() + "\n";
  if (id && title && !body.startsWith(`# ${id} — ${title}\n`)) {
    errors.push(`${path}: body heading must match id and title`);
  }
  const requiredHeading = status === "resolved"
    ? "## Outcome and validation"
    : "## Required outcome and acceptance";
  if (!body.includes(`\n${requiredHeading}\n`)) {
    errors.push(`${path}: body must contain '${requiredHeading}'`);
  }
  if (body.length < 200) errors.push(`${path}: issue record is too short to preserve evidence`);

  if (errors.length || !id || !title || !priority || !status || !area) return { errors };
  return { issue: { id, title, priority, status, area, resolved, path, body }, errors };
}

function displayStatus(status: IssueStatus): string {
  if (status === "in-progress") return "In progress";
  return status[0].toUpperCase() + status.slice(1);
}

function tableCell(value: string): string {
  return value
    .replace(/\s+/g, " ")
    .trim()
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll("\\", "\\\\")
    .replaceAll("|", "\\|");
}

function row(issue: Issue, prefix = "."): string {
  return `| [${issue.id}](${prefix}/${issue.id}.md) | ${tableCell(issue.priority)} | ${tableCell(displayStatus(issue.status))} | ${tableCell(issue.area)} | ${tableCell(issue.title)} |`;
}

export function renderRegistry(issues: Issue[]): string {
  const active = issues.filter((issue) => issue.status !== "resolved").length;
  return `# IronCrew issue registry

This is the canonical registry for IronCrew engineering findings. Each finding
has a stable page whose frontmatter is the source of truth for status,
priority, area, and title. The registry is generated with
\`bun run scripts/issues_registry.ts generate\` and verified with
\`bun run scripts/issues_registry.ts check\`.

- Total findings: ${issues.length}
- Active findings: ${active}
- Issued-through marker: [HIGH_WATER_MARK](./HIGH_WATER_MARK)
- Historical audit evidence: [AUDIT_EVIDENCE.md](./AUDIT_EVIDENCE.md)

| ID | Priority | Status | Area | Summary |
|---|---:|---|---|---|
${issues.map((issue) => row(issue)).join("\n")}
`;
}

export function renderCompatibilityIndex(issues: Issue[]): string {
  const active = issues.filter((issue) => issue.status !== "resolved");
  const rows = active.length
    ? active.map((issue) => row(issue, "docs/issues")).join("\n")
    : "| — | — | — | — | No active findings |";
  return `# IronCrew engineering issues

The canonical engineering ledger is maintained in
[\`docs/issues/README.md\`](docs/issues/README.md). Individual findings use
stable paths such as [\`docs/issues/IC-001.md\`](docs/issues/IC-001.md).

## Active findings

| ID | Priority | Status | Area | Summary |
|---|---:|---|---|---|
${rows}

## Working agreement

1. Select one issue, or one tightly coupled pair, from the highest-priority
   active group and set its frontmatter status to \`in-progress\`.
2. Confirm the live code still supports the finding, then add focused
   regression coverage for the original defect or missing contract.
3. Align implementation, current documentation, Lua examples, evaluations,
   and deployment guidance affected by the issue.
4. Run focused tests while iterating and the required all-target Rust gates
   before completion. Use live PostgreSQL only when the contract requires it.
5. Set an issue to \`resolved\` only after its acceptance criteria pass. Record
   the outcome, boundary, exact validation evidence, ISO completion date, and
   commit or PR when applicable.
6. Allocate the next never-reused ID and advance \`docs/issues/HIGH_WATER_MARK\`
   when adding a finding. Never lower the marker or delete a historical page.
7. Regenerate the indexes and run \`bun run scripts/issues_registry.ts check\`.

Historical audit baselines and cross-issue evidence are retained in
[\`docs/issues/AUDIT_EVIDENCE.md\`](docs/issues/AUDIT_EVIDENCE.md). Other plans
and product roadmaps are not engineering-status evidence.
`;
}

export async function loadIssues(): Promise<{ issues: Issue[]; errors: string[] }> {
  const glob = new Bun.Glob("IC-*.md");
  const paths: string[] = [];
  for await (const path of glob.scan({ cwd: issuesDirectory, onlyFiles: true })) paths.push(path);
  paths.sort();

  const issues: Issue[] = [];
  const errors: string[] = [];
  const ids = new Set<string>();
  for (const relativePath of paths) {
    const path = `docs/issues/${relativePath}`;
    const parsed = parseIssuePage(path, await Bun.file(join(issuesDirectory, relativePath)).text());
    errors.push(...parsed.errors);
    if (!parsed.issue) continue;
    if (ids.has(parsed.issue.id)) errors.push(`${path}: duplicate id ${parsed.issue.id}`);
    ids.add(parsed.issue.id);
    issues.push(parsed.issue);
  }

  issues.sort((left, right) => left.id.localeCompare(right.id));
  if (!issues.length) errors.push("docs/issues: no IC-NNN pages found");
  const highWaterFile = Bun.file(highWaterMarkPath);
  let currentHighWaterId: string | undefined;
  if (!(await highWaterFile.exists())) {
    errors.push("docs/issues/HIGH_WATER_MARK: file is missing");
  } else {
    const parsedHighWater = parseHighWaterMark(await highWaterFile.text());
    errors.push(...parsedHighWater.errors);
    if (parsedHighWater.id) {
      currentHighWaterId = parsedHighWater.id;
      errors.push(...validateIssueSequence([...ids], parsedHighWater.id));
    }
  }
  const trustedBase = await highWaterMarkAtTrustedBase();
  errors.push(...trustedBase.errors);
  if (currentHighWaterId && trustedBase.id) {
    errors.push(...validateHighWaterProgression(currentHighWaterId, trustedBase.id));
  }
  return { issues, errors };
}

async function checkGenerated(path: string, expected: string, errors: string[]): Promise<void> {
  const file = Bun.file(path);
  if (!(await file.exists()) || await file.text() !== expected) {
    errors.push(`${path.replace(repository + "/", "")}: generated content is stale; run the generate command`);
  }
}

async function run(command: string): Promise<void> {
  const { issues, errors } = await loadIssues();
  const registry = renderRegistry(issues);
  const compatibility = renderCompatibilityIndex(issues);

  if (command === "generate") {
    if (errors.length) throw new Error(errors.join("\n"));
    await Bun.write(registryPath, registry);
    await Bun.write(compatibilityPath, compatibility);
    console.log(`Generated issue indexes for ${issues.length} findings.`);
    return;
  }
  if (command !== "check") throw new Error("usage: bun scripts/issues_registry.ts <check|generate>");

  if (!(await Bun.file(auditEvidencePath).exists())) {
    errors.push("docs/issues/AUDIT_EVIDENCE.md: file is missing");
  }
  await checkGenerated(registryPath, registry, errors);
  await checkGenerated(compatibilityPath, compatibility, errors);
  if (errors.length) throw new Error(errors.join("\n"));
  console.log(`Issue registry validation passed: ${issues.length} findings.`);
}

if (import.meta.main) {
  run(Bun.argv[2] ?? "").catch((error) => {
    console.error(`issue registry: ${error instanceof Error ? error.message : String(error)}`);
    process.exit(1);
  });
}
