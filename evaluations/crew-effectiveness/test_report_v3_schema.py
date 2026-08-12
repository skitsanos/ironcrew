#!/usr/bin/env python3
"""Structural regressions for the strict report-v3 JSON Schema."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any


BASE_DIR = Path(__file__).resolve().parent
REPO_ROOT = BASE_DIR.parent.parent
BINARY = REPO_ROOT / "target/debug/ironcrew"


def _walk(value: Any):
    yield value
    if isinstance(value, dict):
        for child in value.values():
            yield from _walk(child)
    elif isinstance(value, list):
        for child in value:
            yield from _walk(child)


class ReportV3SchemaTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.schema = json.loads((BASE_DIR / "report-v3.schema.json").read_text())

    def test_every_local_reference_resolves(self) -> None:
        references = {
            value["$ref"]
            for value in _walk(self.schema)
            if isinstance(value, dict) and isinstance(value.get("$ref"), str)
        }
        self.assertTrue(references)
        for reference in references:
            self.assertRegex(reference, r"^#/\$defs/[A-Za-z0-9_-]+$")
            definition = reference.removeprefix("#/$defs/")
            self.assertIn(definition, self.schema["$defs"], reference)

    def test_source_revision_accepts_sha1_and_sha256_only(self) -> None:
        pattern = re.compile(self.schema["$defs"]["git_revision"]["pattern"])
        self.assertIsNotNone(pattern.fullmatch("a" * 40))
        self.assertIsNotNone(pattern.fullmatch("b" * 64))
        self.assertIsNone(pattern.fullmatch("c" * 39))
        self.assertIsNone(pattern.fullmatch("d" * 41))
        self.assertIsNone(pattern.fullmatch("G" * 40))

    def test_v3_contract_fields_and_plan_caps_are_required(self) -> None:
        required = set(self.schema["required"])
        self.assertTrue({"source", "domain_summary", "pricing", "execution"} <= required)
        configuration = self.schema["properties"]["configuration"]
        self.assertIn("planned_llm_calls_by_task", configuration["required"])
        self.assertEqual(
            configuration["properties"]["planned_llm_calls_by_task"]["const"],
            {
                "single": {"final": 1},
                "dag": {"extract": 1, "challenge": 1, "final": 1},
                "collaborative": {"discussion": 3, "final": 1},
            },
        )
        self.assertIn("planned_max_output_tokens_by_task", configuration["required"])
        self.assertEqual(
            configuration["properties"]["planned_max_output_tokens_by_task"]["const"],
            {
                "single": {"final": 800},
                "dag": {"extract": 500, "challenge": 500, "final": 800},
                "collaborative": {"discussion": 1_500, "final": 800},
            },
        )
        self.assertIsInstance(configuration["properties"]["variants"]["items"], list)
        self.assertFalse(configuration["properties"]["variants"]["additionalItems"])

        run_required = set(self.schema["$defs"]["run"]["required"])
        self.assertTrue(
            {
                "domain_pack",
                "prompt_tokens",
                "completion_tokens",
                "cached_tokens",
                "estimated_cost_upper_bound_usd",
            }
            <= run_required
        )
        limits = self.schema["$defs"]["evaluation_plan"]["properties"]["limits"][
            "const"
        ]
        self.assertEqual(limits["max_case_count"], 12)
        self.assertEqual(limits["max_repetitions"], 5)
        self.assertEqual(limits["max_cli_runs"], 180)
        self.assertEqual(limits["max_planned_llm_calls"], 480)
        self.assertEqual(limits["max_planned_input_tokens"], 9_600_000)
        self.assertEqual(limits["input_token_costing_allowance_per_request"], 20_000)
        self.assertEqual(limits["max_provider_request_body_bytes"], 18_000)
        self.assertEqual(limits["max_completion_tokens_per_request"], 800)
        self.assertEqual(limits["max_planned_output_tokens"], 294_000)
        self.assertEqual(limits["max_case_input_bytes"], 65_536)
        self.assertEqual(limits["max_single_case_input_bytes"], 4_096)
        self.assertNotIn("max_input_tokens_per_request", limits)

        plan = self.schema["$defs"]["evaluation_plan"]
        self.assertIn("rate_limit", plan["required"])
        self.assertEqual(
            plan["properties"]["rate_limit"]["const"],
            {
                "minimum_provider_start_interval_ms": 3_200,
                "rolling_window_seconds": 60,
                "maximum_provider_starts_per_window": 19,
                "maximum_token_allowance_per_window": 395_200,
                "reference_rpm": 500,
                "reference_tpm": 500_000,
                "reference": "openai-gpt-5.6-luna-tier-1-2026-08-12",
            },
        )
        self.assertEqual(plan["properties"]["planned_cost_upper_bound_usd"]["const"], 2.7528)
        planned_work = plan["properties"]["planned_work"]
        self.assertIn("input_token_costing_allowance", planned_work["required"])
        self.assertNotIn("maximum_input_tokens", planned_work["properties"])
        self.assertEqual(
            planned_work["properties"]["input_token_costing_allowance"]["maximum"],
            9_600_000,
        )

        flow_variants = plan["properties"]["flow"]["const"]["variants"]
        self.assertEqual(flow_variants["single"]["task_llm_calls"], {"final": 1})
        self.assertEqual(
            flow_variants["single"]["task_maximum_output_tokens"], {"final": 800}
        )
        self.assertEqual(
            flow_variants["dag"]["task_llm_calls"],
            {"extract": 1, "challenge": 1, "final": 1},
        )
        self.assertEqual(
            flow_variants["dag"]["task_maximum_output_tokens"],
            {"extract": 500, "challenge": 500, "final": 800},
        )
        self.assertEqual(
            flow_variants["collaborative"]["task_llm_calls"],
            {"discussion": 3, "final": 1},
        )
        self.assertEqual(
            flow_variants["collaborative"]["task_maximum_output_tokens"],
            {"discussion": 1_500, "final": 800},
        )

        dataset = plan["properties"]["dataset"]
        self.assertEqual(
            dataset["properties"]["aggregate_sha256"]["const"],
            "bb73ad0d4835a407e22bc35de1562a9f600e33583ec219e40eba2b7b4b0c45cf",
        )
        self.assertEqual(
            [
                pack["properties"]["pack_id"]["const"]
                for pack in dataset["properties"]["packs"]["items"]
            ],
            ["synthetic-core-v1", "security-operations", "software-delivery"],
        )
        packs = dataset["properties"]["packs"]
        self.assertIsInstance(packs["items"], list)
        self.assertFalse(packs["additionalItems"])

        task_usage = self.schema["$defs"]["task_usage"]
        self.assertTrue(
            {
                "task",
                "planned_llm_calls",
                "prompt_token_costing_allowance",
                "completion_token_limit",
            }
            <= set(task_usage["required"])
        )
        self.assertEqual(task_usage["properties"]["planned_llm_calls"]["enum"], [1, 3])
        self.assertEqual(
            task_usage["properties"]["prompt_token_costing_allowance"]["maximum"],
            60_000,
        )
        self.assertEqual(
            task_usage["properties"]["completion_token_limit"]["maximum"], 1_500
        )
        self.assertEqual(task_usage["properties"]["prompt_tokens"]["maximum"], 60_000)
        self.assertEqual(task_usage["properties"]["completion_tokens"]["maximum"], 1_500)
        self.assertEqual(task_usage["properties"]["total_tokens"]["maximum"], 61_500)
        self.assertEqual(
            task_usage["oneOf"],
            [
                {
                    "properties": {
                        "task": {"const": "discussion"},
                        "planned_llm_calls": {"const": 3},
                        "prompt_token_costing_allowance": {"const": 60_000},
                        "completion_token_limit": {"const": 1_500},
                    }
                },
                {
                    "properties": {
                        "task": {"const": "final"},
                        "planned_llm_calls": {"const": 1},
                        "prompt_token_costing_allowance": {"const": 20_000},
                        "completion_token_limit": {"const": 800},
                    }
                },
                {
                    "properties": {
                        "task": {"enum": ["extract", "challenge"]},
                        "planned_llm_calls": {"const": 1},
                        "prompt_token_costing_allowance": {"const": 20_000},
                        "completion_token_limit": {"const": 500},
                    }
                },
            ],
        )

        pricing = self.schema["properties"]["pricing"]
        self.assertNotIn("within_budget", pricing["required"])
        self.assertNotIn("within_budget", pricing["properties"])
        self.assertTrue(
            {"planned_bound_within_budget", "observed_estimate_within_budget"}
            <= set(pricing["required"])
        )
        self.assertEqual(
            self.schema["properties"]["provider"]["properties"]["model"]["const"],
            "gpt-5.6-luna",
        )
        live_provider = self.schema["allOf"][1]["then"]["properties"]["provider"][
            "properties"
        ]
        self.assertEqual(live_provider["name"]["const"], "openai-api")

    @unittest.skipUnless(
        BINARY.is_file() and os.access(BINARY, os.X_OK),
        "target/debug/ironcrew is required for report-schema integration",
    )
    def test_real_contract_report_validates_through_ironcrew(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ironcrew-report-v3-test-") as temporary:
            report = Path(temporary) / "contract.json"
            completed = subprocess.run(
                [
                    sys.executable,
                    "-B",
                    str(BASE_DIR / "evaluate.py"),
                    "--mode",
                    "contract",
                    "--binary",
                    str(BINARY),
                    "--report",
                    str(report),
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                timeout=60,
            )
            self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
            receipt = json.loads(report.read_text())
            self.assertEqual(receipt["schema_version"], "ironcrew.crew-eval.v3")
            self.assertTrue(receipt["source"]["unchanged"])
            self.assertEqual(len(receipt["runs"]), 36)

            schema = BASE_DIR / "report-v3.schema.json"
            validator = BASE_DIR / "validate-report.lua"
            environment = {
                name: value
                for name in ("PATH", "LANG", "LC_ALL", "TMPDIR")
                if (value := os.environ.get(name)) is not None
            }
            sys.path.insert(0, str(BASE_DIR))
            from evaluate import validate_report_with_ironcrew

            mutations = []
            variants = json.loads(json.dumps(receipt))
            variants["configuration"]["variants"] = ["evil", "evil", "evil"]
            mutations.append(variants)

            wrong_pack = json.loads(json.dumps(receipt))
            wrong_pack["evaluation_plan"]["dataset"]["packs"][0]["pack_id"] = "evil"
            mutations.append(wrong_pack)

            reversed_packs = json.loads(json.dumps(receipt))
            reversed_packs["evaluation_plan"]["dataset"]["packs"].reverse()
            mutations.append(reversed_packs)

            wrong_task_plan = json.loads(json.dumps(receipt))
            wrong_task_plan["configuration"]["planned_llm_calls_by_task"][
                "collaborative"
            ]["discussion"] = 2
            mutations.append(wrong_task_plan)

            wrong_cost_allowance = json.loads(json.dumps(receipt))
            wrong_cost_allowance["evaluation_plan"]["limits"][
                "input_token_costing_allowance_per_request"
            ] = 19_999
            mutations.append(wrong_cost_allowance)

            wrong_rate_limit = json.loads(json.dumps(receipt))
            wrong_rate_limit["evaluation_plan"]["rate_limit"][
                "maximum_provider_starts_per_window"
            ] = 20
            mutations.append(wrong_rate_limit)

            wrong_planned_work = json.loads(json.dumps(receipt))
            wrong_planned_work["evaluation_plan"]["planned_work"][
                "input_token_costing_allowance"
            ] = 9_600_001
            mutations.append(wrong_planned_work)

            generic_budget_gate = json.loads(json.dumps(receipt))
            generic_budget_gate["pricing"]["within_budget"] = generic_budget_gate[
                "pricing"
            ].pop("planned_bound_within_budget")
            mutations.append(generic_budget_gate)

            wrong_discussion_call_count = json.loads(json.dumps(receipt))
            collaborative_run = next(
                run
                for run in wrong_discussion_call_count["runs"]
                if run["variant"] == "collaborative" and run["task_usage"]
            )
            discussion_usage = next(
                usage
                for usage in collaborative_run["task_usage"]
                if usage["task"] == "discussion"
            )
            discussion_usage["planned_llm_calls"] = 1
            mutations.append(wrong_discussion_call_count)

            excessive_task_prompt_allowance = json.loads(json.dumps(receipt))
            run_with_usage = next(
                run for run in excessive_task_prompt_allowance["runs"] if run["task_usage"]
            )
            run_with_usage["task_usage"][0][
                "prompt_token_costing_allowance"
            ] = 60_001
            mutations.append(excessive_task_prompt_allowance)

            excessive_discussion_completion_limit = json.loads(json.dumps(receipt))
            collaborative_run = next(
                run
                for run in excessive_discussion_completion_limit["runs"]
                if run["variant"] == "collaborative" and run["task_usage"]
            )
            discussion_usage = next(
                usage
                for usage in collaborative_run["task_usage"]
                if usage["task"] == "discussion"
            )
            discussion_usage["completion_token_limit"] = 2_400
            mutations.append(excessive_discussion_completion_limit)

            for index, mutation in enumerate(mutations):
                mutated_report = Path(temporary) / f"mutated-{index}.json"
                mutated_report.write_text(json.dumps(mutation), encoding="utf-8")
                error = validate_report_with_ironcrew(
                    binary=BINARY,
                    repo_root=REPO_ROOT,
                    validator_path=validator,
                    schema_path=schema,
                    report_path=mutated_report,
                    environment=environment,
                )
                self.assertIsNotNone(error, f"mutation {index} unexpectedly validated")


if __name__ == "__main__":
    unittest.main()
