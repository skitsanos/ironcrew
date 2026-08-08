#!/usr/bin/env bun

import { basename, dirname, join } from "node:path";

type YamlMap = Record<string, unknown>;

const repository = join(import.meta.dir, "..");
const skillGlob = new Bun.Glob(".agents/skills/*/SKILL.md");
const errors: string[] = [];
let count = 0;

function asMap(value: unknown, label: string): YamlMap | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    errors.push(`${label} must be a YAML mapping`);
    return undefined;
  }
  return value as YamlMap;
}

function nonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

function parseYaml(source: string, label: string): YamlMap | undefined {
  try {
    return asMap(Bun.YAML.parse(source), label);
  } catch (error) {
    errors.push(`${label} is invalid YAML: ${String(error)}`);
    return undefined;
  }
}

export function validatePlainMappingKeys(source: string, indentation: number): string[] {
  const keyErrors: string[] = [];
  const seen = new Set<string>();
  for (const [index, line] of source.split(/\r?\n/).entries()) {
    if (!line.trim() || line.trimStart().startsWith("#")) continue;
    const leadingSpaces = line.match(/^ */)?.[0].length ?? 0;
    if (leadingSpaces !== indentation) continue;
    const match = line.slice(indentation).match(/^([A-Za-z_][A-Za-z0-9_-]*):(?:\s|$)/);
    if (!match) {
      keyErrors.push(`line ${index + 1} must use one plain mapping key`);
      continue;
    }
    const key = match[1];
    if (seen.has(key)) keyErrors.push(`line ${index + 1} duplicates mapping key '${key}'`);
    seen.add(key);
  }
  return keyErrors;
}

function validateSkill(path: string, source: string): string | undefined {
  const match = source.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]+)$/);
  if (!match) {
    errors.push(`${path}: expected YAML frontmatter and a non-empty body`);
    return undefined;
  }

  for (const error of validatePlainMappingKeys(match[1], 0)) {
    errors.push(`${path} frontmatter: ${error}`);
  }

  const frontmatter = parseYaml(match[1], `${path} frontmatter`);
  if (!frontmatter) return undefined;

  const unexpected = Object.keys(frontmatter).filter(
    (key) => key !== "name" && key !== "description",
  );
  if (unexpected.length > 0) {
    errors.push(`${path}: unsupported frontmatter fields: ${unexpected.join(", ")}`);
  }

  const expectedName = basename(dirname(path));
  const name = frontmatter.name;
  if (!nonEmptyString(name) || !/^[a-z0-9-]{1,64}$/.test(name)) {
    errors.push(`${path}: name must use 1-64 lowercase letters, digits, or hyphens`);
    return undefined;
  }
  if (name !== expectedName) {
    errors.push(`${path}: name '${name}' must match directory '${expectedName}'`);
  }
  if (!nonEmptyString(frontmatter.description)) {
    errors.push(`${path}: description must be a non-empty string`);
  }
  if (source.includes("[TODO") || source.includes("TODO:")) {
    errors.push(`${path}: unresolved scaffold TODO`);
  }
  return name;
}

async function validateInterface(skillPath: string, name: string): Promise<void> {
  const metadataPath = join(dirname(skillPath), "agents", "openai.yaml");
  const file = Bun.file(join(repository, metadataPath));
  if (!(await file.exists())) return;

  const source = await file.text();
  for (const indentation of [0, 2]) {
    for (const error of validatePlainMappingKeys(source, indentation)) {
      errors.push(`${metadataPath}: ${error}`);
    }
  }
  const metadata = parseYaml(source, metadataPath);
  if (!metadata) return;
  const interfaceMap = asMap(metadata.interface, `${metadataPath} interface`);
  if (!interfaceMap) return;

  if (!nonEmptyString(interfaceMap.display_name)) {
    errors.push(`${metadataPath}: interface.display_name must be a non-empty string`);
  }
  const summary = interfaceMap.short_description;
  if (!nonEmptyString(summary) || summary.length < 25 || summary.length > 64) {
    errors.push(`${metadataPath}: short_description must contain 25-64 characters`);
  }
  const prompt = interfaceMap.default_prompt;
  if (!nonEmptyString(prompt) || !prompt.includes(`$${name}`)) {
    errors.push(`${metadataPath}: default_prompt must mention $${name}`);
  }
}

async function main(): Promise<void> {
  const paths: string[] = [];
  for await (const path of skillGlob.scan({ cwd: repository, dot: true, onlyFiles: true })) {
    paths.push(path);
  }
  paths.sort();

  for (const path of paths) {
    count += 1;
    const name = validateSkill(path, await Bun.file(join(repository, path)).text());
    if (name) await validateInterface(path, name);
  }

  if (count === 0) errors.push("no repository skills found under .agents/skills");

  if (errors.length > 0) {
    for (const error of errors) console.error(`skill validation: ${error}`);
    process.exit(1);
  }

  console.log(`Skill validation passed: ${count} repository skills.`);
}

if (import.meta.main) await main();
