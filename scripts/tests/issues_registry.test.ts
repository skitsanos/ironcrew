import { describe, expect, test } from "bun:test";
import {
  parseHighWaterMark,
  parseIssuePage,
  renderCompatibilityIndex,
  renderRegistry,
  type Issue,
  validateHighWaterProgression,
  validateIssueSequence,
} from "../issues_registry";

const resolvedPage = `---
id: IC-001
title: Bounded example finding
priority: P2
status: resolved
area: Test policy
resolved: 2026-08-04
---
# IC-001 — Bounded example finding

## Outcome and validation

The bounded implementation outcome is recorded here with enough detail to
explain the contract. Focused negative and positive regression tests passed,
and no live service behavior was changed by this example finding.
`;

function issue(overrides: Partial<Issue> = {}): Issue {
  return {
    id: "IC-001",
    title: "Bounded example finding",
    priority: "P2",
    status: "resolved",
    area: "Test policy",
    resolved: "2026-08-04",
    path: "docs/issues/IC-001.md",
    body: resolvedPage,
    ...overrides,
  };
}

describe("issue registry", () => {
  test("parses a resolved issue with canonical metadata and evidence", () => {
    const parsed = parseIssuePage("docs/issues/IC-001.md", resolvedPage);
    expect(parsed.errors).toEqual([]);
    expect(parsed.issue).toMatchObject({
      id: "IC-001",
      priority: "P2",
      status: "resolved",
      resolved: "2026-08-04",
    });
  });

  test("rejects filename, lifecycle, date, and contract-heading drift", () => {
    const source = resolvedPage
      .replace("status: resolved", "status: in-progress")
      .replace("resolved: 2026-08-04", "resolved: 2026-02-30")
      .replace("## Outcome and validation", "## Notes");
    const parsed = parseIssuePage("docs/issues/IC-002.md", source);
    expect(parsed.errors).toContain(
      "docs/issues/IC-002.md: id IC-001 must match filename IC-002",
    );
    expect(parsed.errors).toContain(
      "docs/issues/IC-002.md: unresolved issues cannot set resolved",
    );
    expect(parsed.errors).toContain(
      "docs/issues/IC-002.md: body must contain '## Required outcome and acceptance'",
    );
  });

  test("rejects an impossible resolved date", () => {
    const parsed = parseIssuePage(
      "docs/issues/IC-001.md",
      resolvedPage.replace("2026-08-04", "2026-02-30"),
    );
    expect(parsed.errors).toContain(
      "docs/issues/IC-001.md: resolved issues require a valid ISO resolved date",
    );
  });

  test("rejects duplicate YAML frontmatter fields before parsing", () => {
    const parsed = parseIssuePage(
      "docs/issues/IC-001.md",
      resolvedPage.replace("status: resolved", "status: open\nstatus: resolved"),
    );
    expect(parsed.errors).toContain(
      "docs/issues/IC-001.md: duplicate frontmatter field 'status'",
    );
  });

  test("persists the issued-id high-water mark against deletion or rollback", () => {
    expect(parseHighWaterMark("IC-003\n")).toEqual({ id: "IC-003", errors: [] });
    expect(parseHighWaterMark("IC-000\n").errors).not.toEqual([]);
    expect(validateIssueSequence(["IC-001", "IC-003"], "IC-003")).toContain(
      "docs/issues: missing IC-002 below persisted high-water mark",
    );
    expect(validateIssueSequence(["IC-001", "IC-002"], "IC-003")).toContain(
      "docs/issues: missing IC-003 below persisted high-water mark",
    );
    expect(validateIssueSequence(["IC-001", "IC-002", "IC-003"], "IC-002")).toContain(
      "docs/issues/HIGH_WATER_MARK: IC-002 is behind issued finding IC-003",
    );

    const coordinatedRollback = ["IC-001", "IC-002"];
    expect(validateIssueSequence(coordinatedRollback, "IC-002")).toEqual([]);
    expect(validateHighWaterProgression("IC-002", "IC-003")).toContain(
      "docs/issues/HIGH_WATER_MARK: IC-002 is behind trusted base IC-003",
    );
    expect(validateHighWaterProgression("IC-004", "IC-003")).toEqual([]);
  });

  test("renders complete history but only active root findings", () => {
    const active = issue({
      id: "IC-002",
      title: "Active finding",
      status: "in-progress",
      resolved: undefined,
      path: "docs/issues/IC-002.md",
    });
    const registry = renderRegistry([issue(), active]);
    const compatibility = renderCompatibilityIndex([issue(), active]);

    expect(registry).toContain("Total findings: 2");
    expect(registry).toContain("[HIGH_WATER_MARK](./HIGH_WATER_MARK)");
    expect(registry).toContain("[IC-001](./IC-001.md)");
    expect(registry).toContain("[IC-002](./IC-002.md)");
    expect(compatibility).not.toContain("| [IC-001]");
    expect(compatibility).toContain("[IC-002](docs/issues/IC-002.md)");
    expect(compatibility).toContain("Never lower the marker or delete a historical page");
    expect(compatibility).toContain("required all-target Rust gates");
  });

  test("escapes generated Markdown table cells", () => {
    const active = issue({
      id: "IC-002",
      title: "Active | <finding>\nwith context",
      area: "Runtime | safety",
      status: "open",
      resolved: undefined,
      path: "docs/issues/IC-002.md",
    });
    const compatibility = renderCompatibilityIndex([active]);

    expect(compatibility).toContain("Runtime \\| safety");
    expect(compatibility).toContain("Active \\| &lt;finding&gt; with context");
    expect(compatibility).not.toContain("Active | <finding>");
  });
});
