#!/usr/bin/env python3
"""Run and score the IronCrew grounded crew-effectiveness evaluation."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import re
import shutil
import subprocess
import tempfile
import threading
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Callable, Iterable

from corpus_loader import load_corpus
from decision_analysis import topology_decision
from evaluation_plan_v2 import VARIANTS as PLAN_VARIANTS
from evaluation_plan_v2 import load_plan as load_plan_v2
from evaluation_plan_v2 import preflight as preflight_v2
from evaluation_reporting_v3 import (
    dataset_receipt,
    domain_summaries,
    empty_usage,
    pricing_receipt,
    report_json_bytes,
    successful_run_usage,
)
from evaluation_setup import validate_run_request
from live_provider_environment import live_provider_environment, redaction_canaries
from mock_openai import ContractServer, create_server
from pairwise_analysis import pairwise_comparisons, summarize_runs
from pricing_budget import require_approved_budget
from report_schema_validation import validate_report
from source_provenance import (
    require_unchanged_provenance,
    safe_binary_path,
    worktree_provenance,
)


SCHEMA_VERSION = "ironcrew.crew-eval.v3"
DEFAULT_MODEL = "gpt-5.6-luna"
VARIANTS = tuple(PLAN_VARIANTS)
EXPECTED_TASKS = {
    "single": {"final"},
    "dag": {"extract", "challenge", "final"},
    "collaborative": {"discussion", "final"},
}
EXPECTED_AGENTS = {"single": 1, "dag": 3, "collaborative": 3}
OPTION_ID_PATTERN = re.compile(r"[a-z][a-z0-9_]*")
CONTRACT_NOTICE = (
    "Synthetic contract validation only. The mock provider reads the oracle; "
    "these scores are never evidence that one crew topology is more effective."
)
LIVE_NOTICE = (
    "Decision-grade local live-provider evidence on representative synthetic intended-use "
    "packs. Interpret quality, cost, and latency together; this is not production-sample, "
    "multi-model, deployed-platform, or broad superiority evidence."
)


class LiveProcessPacer:
    """Keep the first provider start in consecutive CLI processes separated.

    IronCrew enforces the same interval between calls inside one process. This
    evaluator additionally starts the interval when each CLI process finishes,
    including failed and timed-out runs, so process-local limiter state cannot
    reset the cross-process boundary.
    """

    def __init__(
        self,
        interval_ms: int,
        *,
        clock: Callable[[], float] = time.monotonic,
        sleeper: Callable[[float], None] = time.sleep,
    ) -> None:
        if interval_ms < 1:
            raise ValueError("live process interval must be positive")
        self._interval_seconds = interval_ms / 1_000
        self._clock = clock
        self._sleeper = sleeper
        self._last_completion: float | None = None

    def wait_before_start(self) -> None:
        if self._last_completion is None:
            return
        remaining = self._last_completion + self._interval_seconds - self._clock()
        if remaining > 0:
            self._sleeper(remaining)

    def record_completion(self) -> None:
        self._last_completion = self._clock()


def apply_live_provider_controls(
    environment: dict[str, str], plan: dict[str, Any]
) -> None:
    """Force the reviewed pre-send byte boundary and in-process pacing."""
    environment["IRONCREW_PROVIDER_MAX_REQUEST_BYTES"] = str(
        plan["limits"]["max_provider_request_body_bytes"]
    )
    environment["IRONCREW_RATE_LIMIT_MS"] = str(
        plan["rate_limit"]["minimum_provider_start_interval_ms"]
    )


def schema_validation_environment(environment: dict[str, str]) -> dict[str, str]:
    """Keep report validation independent of provider credentials and dotenv."""
    allowed = {
        "PATH",
        "LANG",
        "LANGUAGE",
        "LC_ALL",
        "LC_CTYPE",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "REQUESTS_CA_BUNDLE",
        "CURL_CA_BUNDLE",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SYSTEMROOT",
        "WINDIR",
        "IRONCREW_LOG",
    }
    return {name: value for name, value in environment.items() if name in allowed}


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8") as source:
        for line_number, raw_line in enumerate(source, 1):
            line = raw_line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                raise ValueError(f"{path}:{line_number}: invalid JSON: {error}") from error
            if not isinstance(record, dict):
                raise ValueError(f"{path}:{line_number}: each line must be a JSON object")
            records.append(record)
    if not records:
        raise ValueError(f"{path}: no records")
    return records


def index_by_case(records: Iterable[dict[str, Any]], label: str) -> dict[str, dict[str, Any]]:
    indexed: dict[str, dict[str, Any]] = {}
    for record in records:
        case_id = record.get("case_id")
        if not isinstance(case_id, str) or not re.fullmatch(r"[a-z0-9][a-z0-9-]*", case_id):
            raise ValueError(f"{label}: invalid case_id {case_id!r}")
        if case_id in indexed:
            raise ValueError(f"{label}: duplicate case_id {case_id}")
        indexed[case_id] = record
    return indexed


def nested_keys(value: Any) -> set[str]:
    keys: set[str] = set()
    if isinstance(value, dict):
        for key, child in value.items():
            keys.add(str(key))
            keys.update(nested_keys(child))
    elif isinstance(value, list):
        for child in value:
            keys.update(nested_keys(child))
    return keys


def validate_dataset(
    cases: list[dict[str, Any]],
    oracle_records: list[dict[str, Any]],
) -> dict[str, dict[str, Any]]:
    cases_by_id = index_by_case(cases, "cases")
    oracle_by_id = index_by_case(oracle_records, "oracle")
    if set(cases_by_id) != set(oracle_by_id):
        missing_oracle = sorted(set(cases_by_id) - set(oracle_by_id))
        missing_cases = sorted(set(oracle_by_id) - set(cases_by_id))
        raise ValueError(
            f"case/oracle IDs differ; missing oracle={missing_oracle}, missing cases={missing_cases}"
        )

    forbidden_case_keys = {
        "accepted_answers",
        "correct_option_ids",
        "correct_option_id",
        "citation_sets",
        "is_correct",
        "correct_answer",
        "oracle",
        "gold",
    }
    for case_id, case in cases_by_id.items():
        if set(case) != {"case_id", "evidence", "questions"}:
            raise ValueError(f"case {case_id}: missing or additional top-level fields")
        leaked = nested_keys(case) & forbidden_case_keys
        if leaked:
            raise ValueError(f"case {case_id} leaks scoring keys: {sorted(leaked)}")
        evidence = case.get("evidence")
        questions = case.get("questions")
        if not isinstance(evidence, list) or not evidence:
            raise ValueError(f"case {case_id}: evidence must be a non-empty array")
        if not isinstance(questions, list) or not questions:
            raise ValueError(f"case {case_id}: questions must be a non-empty array")
        evidence_ids: list[str] = []
        for index, item in enumerate(evidence):
            if not isinstance(item, dict) or set(item) != {"id", "text"}:
                raise ValueError(f"case {case_id}: evidence[{index}] must contain id and text")
            evidence_id = item.get("id")
            text = item.get("text")
            if not isinstance(evidence_id, str) or not evidence_id.strip():
                raise ValueError(f"case {case_id}: evidence[{index}] needs a non-empty string id")
            if not isinstance(text, str) or not text.strip():
                raise ValueError(f"case {case_id}: evidence[{index}] needs non-empty text")
            evidence_ids.append(evidence_id)

        question_ids: list[str] = []
        options_by_question: dict[str, set[str]] = {}
        for question_index, question in enumerate(questions):
            if not isinstance(question, dict) or set(question) != {"id", "prompt", "options"}:
                raise ValueError(
                    f"case {case_id}: questions[{question_index}] must contain id, prompt, and options"
                )
            question_id = question.get("id")
            prompt = question.get("prompt")
            options = question.get("options")
            if not isinstance(question_id, str) or not question_id.strip():
                raise ValueError(f"case {case_id}: questions[{question_index}] needs a string id")
            if not isinstance(prompt, str) or not prompt.strip():
                raise ValueError(f"case {case_id}/{question_id}: prompt must be non-empty")
            if not isinstance(options, list) or len(options) < 3:
                raise ValueError(f"case {case_id}/{question_id}: at least three options are required")

            option_ids: list[str] = []
            option_labels: list[str] = []
            for option_index, option in enumerate(options):
                if not isinstance(option, dict) or set(option) != {"id", "label"}:
                    raise ValueError(
                        f"case {case_id}/{question_id}: options[{option_index}] must contain id and label"
                    )
                option_id = option.get("id")
                label = option.get("label")
                if not isinstance(option_id, str) or not OPTION_ID_PATTERN.fullmatch(option_id):
                    raise ValueError(
                        f"case {case_id}/{question_id}: invalid lowercase option id {option_id!r}"
                    )
                if not isinstance(label, str) or not label.strip():
                    raise ValueError(
                        f"case {case_id}/{question_id}: option {option_id} needs a non-empty label"
                    )
                option_ids.append(option_id)
                option_labels.append(label.strip().casefold())
            if len(set(option_ids)) != len(option_ids):
                raise ValueError(f"case {case_id}/{question_id}: duplicate option id")
            if len(set(option_labels)) != len(option_labels):
                raise ValueError(f"case {case_id}/{question_id}: duplicate option label")
            question_ids.append(question_id)
            options_by_question[question_id] = set(option_ids)

        if len(set(evidence_ids)) != len(evidence_ids):
            raise ValueError(f"case {case_id}: duplicate evidence id")
        if len(set(question_ids)) != len(question_ids):
            raise ValueError(f"case {case_id}: duplicate question id")

        oracle_record = oracle_by_id[case_id]
        if set(oracle_record) != {"case_id", "answers"}:
            raise ValueError(f"oracle {case_id}: missing or additional top-level fields")
        oracle_answers = oracle_record.get("answers")
        if not isinstance(oracle_answers, list) or not oracle_answers:
            raise ValueError(f"oracle {case_id}: answers must be a non-empty array")
        oracle_question_ids: list[str] = []
        for answer in oracle_answers:
            if not isinstance(answer, dict) or set(answer) != {
                "question_id",
                "correct_option_ids",
                "citation_sets",
            }:
                raise ValueError(
                    f"oracle {case_id}: each answer must contain question_id, "
                    "correct_option_ids, and citation_sets"
                )
            question_id = answer.get("question_id")
            correct_option_ids = answer.get("correct_option_ids")
            citation_sets = answer.get("citation_sets")
            if not isinstance(question_id, str):
                raise ValueError(f"oracle {case_id}: answer missing question_id")
            if question_id not in options_by_question:
                raise ValueError(f"oracle {case_id}: unknown question_id {question_id}")
            if not isinstance(correct_option_ids, list) or not correct_option_ids or not all(
                isinstance(item, str) and item.strip() for item in correct_option_ids
            ):
                raise ValueError(f"oracle {case_id}/{question_id}: invalid correct_option_ids")
            if len(set(correct_option_ids)) != len(correct_option_ids):
                raise ValueError(f"oracle {case_id}/{question_id}: duplicate correct option id")
            unknown_options = set(correct_option_ids) - options_by_question[question_id]
            if unknown_options:
                raise ValueError(
                    f"oracle {case_id}/{question_id}: unknown correct option IDs "
                    f"{sorted(unknown_options)}"
                )
            if not isinstance(citation_sets, list) or not citation_sets or not all(
                isinstance(group, list)
                and bool(group)
                and all(isinstance(citation, str) and citation for citation in group)
                and len(group) == len(set(group))
                for group in citation_sets
            ):
                raise ValueError(f"oracle {case_id}/{question_id}: invalid citation_sets")
            for group in citation_sets:
                unknown = set(group) - set(evidence_ids)
                if unknown:
                    raise ValueError(
                        f"oracle {case_id}/{question_id}: unknown citations {sorted(unknown)}"
                    )
            oracle_question_ids.append(question_id)
        if set(oracle_question_ids) != set(question_ids):
            raise ValueError(f"case {case_id}: question IDs do not match oracle")
        if len(set(oracle_question_ids)) != len(oracle_question_ids):
            raise ValueError(f"oracle {case_id}: duplicate question id")
    return oracle_by_id


def validate_model_output(value: Any, case: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    if not isinstance(value, dict):
        return ["output must be an object"]
    if set(value) != {"case_id", "answers"}:
        errors.append("output has missing or additional fields")
    if value.get("case_id") != case["case_id"]:
        errors.append("case_id does not match")
    answers = value.get("answers")
    if not isinstance(answers, list):
        return errors + ["answers must be an array"]

    questions_by_id = {question["id"]: question for question in case["questions"]}
    expected_ids = set(questions_by_id)
    seen_ids: set[str] = set()
    for index, answer in enumerate(answers):
        if not isinstance(answer, dict):
            errors.append(f"answers[{index}] must be an object")
            continue
        if set(answer) != {"question_id", "answer", "citations"}:
            errors.append(f"answers[{index}] has missing or additional fields")
        question_id = answer.get("question_id")
        if not isinstance(question_id, str) or question_id not in expected_ids:
            errors.append(f"answers[{index}] has an unknown question_id")
        elif question_id in seen_ids:
            errors.append(f"answers[{index}] duplicates question_id {question_id}")
        else:
            seen_ids.add(question_id)
        answer_id = answer.get("answer")
        if not isinstance(answer_id, str):
            errors.append(f"answers[{index}].answer must be a string")
        elif question_id in questions_by_id:
            option_ids = {option["id"] for option in questions_by_id[question_id]["options"]}
            if answer_id not in option_ids:
                errors.append(
                    f"answers[{index}].answer must exactly match an option id for {question_id}"
                )
        citations = answer.get("citations")
        if not isinstance(citations, list) or not all(isinstance(item, str) for item in citations):
            errors.append(f"answers[{index}].citations must be a string array")
        elif len(citations) != len(set(citations)):
            errors.append(f"answers[{index}].citations contains duplicates")
    if seen_ids != expected_ids:
        errors.append("answers must contain every question exactly once")
    return errors


def score_model_output(
    value: Any,
    case: dict[str, Any],
    oracle: dict[str, Any],
) -> dict[str, Any]:
    expected_by_question = {item["question_id"]: item for item in oracle["answers"]}
    actual_by_question: dict[str, dict[str, Any]] = {}
    if isinstance(value, dict) and isinstance(value.get("answers"), list):
        for item in value["answers"]:
            if isinstance(item, dict) and isinstance(item.get("question_id"), str):
                actual_by_question.setdefault(item["question_id"], item)

    valid_evidence_ids = {item["id"] for item in case["evidence"]}
    answers_total = len(expected_by_question)
    answers_correct = 0
    grounded_correct = 0
    citation_tp = 0
    citation_fp = 0
    citation_fn = 0
    details: list[dict[str, Any]] = []

    for question_id, expected in expected_by_question.items():
        actual = actual_by_question.get(question_id, {})
        answer_id = actual.get("answer") if isinstance(actual.get("answer"), str) else ""
        raw_citations = actual.get("citations")
        citations = raw_citations if isinstance(raw_citations, list) else []
        citations = [citation for citation in citations if isinstance(citation, str)]

        correct = answer_id in expected["correct_option_ids"]
        support_sets = [set(group) for group in expected["citation_sets"]]
        cited = set(citations)
        support_satisfied = any(group.issubset(cited) for group in support_sets)
        support_union = set().union(*support_sets)
        tp = len(cited & support_union)
        fp = len(cited - support_union)
        fn = min((len(group - cited) for group in support_sets), default=0)

        answers_correct += int(correct)
        grounded_correct += int(correct and support_satisfied)
        citation_tp += tp
        citation_fp += fp
        citation_fn += fn
        details.append(
            {
                "question_id": question_id,
                "correct": correct,
                "support_satisfied": support_satisfied,
                "unknown_citations": sorted(cited - valid_evidence_ids),
            }
        )

    precision_denominator = citation_tp + citation_fp
    recall_denominator = citation_tp + citation_fn
    precision = citation_tp / precision_denominator if precision_denominator else 0.0
    recall = citation_tp / recall_denominator if recall_denominator else 0.0
    citation_f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
    return {
        "answers_total": answers_total,
        "answers_correct": answers_correct,
        "grounded_correct": grounded_correct,
        "correctness": answers_correct / answers_total if answers_total else 0.0,
        "grounded_correctness": grounded_correct / answers_total if answers_total else 0.0,
        "citation_tp": citation_tp,
        "citation_fp": citation_fp,
        "citation_fn": citation_fn,
        "citation_precision": precision,
        "citation_recall": recall,
        "citation_f1": citation_f1,
        "answer_details": details,
    }


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def command_output(command: list[str], cwd: Path) -> str | None:
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            check=True,
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return completed.stdout.strip()


def git_metadata(repo_root: Path) -> dict[str, Any]:
    revision = command_output(["git", "rev-parse", "HEAD"], repo_root)
    status = command_output(["git", "status", "--porcelain"], repo_root)
    return {"sha": revision, "dirty": status is None or bool(status)}


def redact_error(text: str, environment: dict[str, str]) -> str:
    redacted = text
    sensitive_suffixes = ("_API_KEY", "_TOKEN", "_PASSWORD", "_SECRET")
    for name, value in environment.items():
        if value and name.upper().endswith(sensitive_suffixes):
            redacted = redacted.replace(value, "<redacted>")
    return redacted[-2000:]


def empty_score(oracle: dict[str, Any]) -> dict[str, Any]:
    answers = oracle.get("answers") if isinstance(oracle.get("answers"), list) else []
    expected_citations = sum(
        min((len(group) for group in answer.get("citation_sets", [])), default=0)
        for answer in answers
        if isinstance(answer, dict)
    )
    return {
        "answers_total": len(answers),
        "answers_correct": 0,
        "grounded_correct": 0,
        "correctness": 0.0,
        "grounded_correctness": 0.0,
        "citation_tp": 0,
        "citation_fp": 0,
        "citation_fn": expected_citations,
        "citation_precision": 0.0,
        "citation_recall": 0.0,
        "citation_f1": 0.0,
        "answer_details": [],
    }


def run_one(
    *,
    binary: Path,
    repo_root: Path,
    flow_dir: Path,
    case: dict[str, Any],
    oracle: dict[str, Any],
    domain_pack: str = "synthetic-core-v1",
    variant: str,
    repetition: int,
    model: str,
    mode: str,
    timeout_seconds: int,
    planned_llm_calls: int,
    task_llm_calls: dict[str, int],
    task_maximum_output_tokens: dict[str, int],
    input_token_costing_allowance_per_request: int = 20_000,
    max_completion_tokens_per_request: int = 800,
    base_environment: dict[str, str],
    mock_server: ContractServer | None,
) -> dict[str, Any]:
    payload = {
        "variant": variant,
        "model": model,
        "case_id": case["case_id"],
        "packet_json": json.dumps(case, sort_keys=True, separators=(",", ":")),
    }
    command = [
        str(binary),
        "run",
        str(flow_dir),
        "--input",
        json.dumps(payload, sort_keys=True, separators=(",", ":")),
        "--json",
        "--tag",
        "crew-effectiveness-eval",
        "--tag",
        f"variant-{variant}",
        "--tag",
        f"mode-{mode}",
    ]
    before_requests = mock_server.snapshot()["request_count"] if mock_server else None
    started = time.monotonic()
    try:
        completed = subprocess.run(
            command,
            cwd=flow_dir,
            env=base_environment,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
            check=False,
        )
        client_duration_ms = int((time.monotonic() - started) * 1000)
    except subprocess.TimeoutExpired:
        observed = None
        if mock_server and before_requests is not None:
            observed = mock_server.snapshot()["request_count"] - before_requests
        result = {
            "case_id": case["case_id"],
            "domain_pack": domain_pack,
            "variant": variant,
            "repetition": repetition,
            "planned_llm_calls": planned_llm_calls,
            "observed_llm_calls": observed,
            "execution_ok": False,
            "output_parse_ok": False,
            "output_schema_ok": False,
            "failure_reason": f"CLI timeout after {timeout_seconds} seconds",
            "client_duration_ms": int((time.monotonic() - started) * 1000),
            "run_duration_ms": None,
            "run_id": None,
            "run_status": None,
            "agent_count": 0,
            "task_count": 0,
            "task_names": [],
            "task_failures": 0,
            "model_output": None,
        }
        result.update(empty_usage())
        result.update(empty_score(oracle))
        return result

    observed = None
    if mock_server and before_requests is not None:
        observed = mock_server.snapshot()["request_count"] - before_requests
    result = {
        "case_id": case["case_id"],
        "domain_pack": domain_pack,
        "variant": variant,
        "repetition": repetition,
        "planned_llm_calls": planned_llm_calls,
        "observed_llm_calls": observed,
        "execution_ok": False,
        "output_parse_ok": False,
        "output_schema_ok": False,
        "failure_reason": None,
        "client_duration_ms": client_duration_ms,
        "run_duration_ms": None,
        "run_id": None,
        "run_status": None,
        "agent_count": 0,
        "task_count": 0,
        "task_names": [],
        "task_failures": 0,
        "model_output": None,
    }
    result.update(empty_usage())
    result.update(empty_score(oracle))

    if completed.returncode != 0:
        result["failure_reason"] = redact_error(completed.stderr, base_environment) or (
            f"CLI exited with status {completed.returncode}"
        )
        return result

    try:
        record = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        result["failure_reason"] = f"CLI stdout was not a run record: {error}"
        return result
    if not isinstance(record, dict):
        result["failure_reason"] = "CLI stdout run record was not an object"
        return result

    status = str(record.get("status", ""))
    run_succeeded = status.casefold() == "success"
    task_results = record.get("task_results") if isinstance(record.get("task_results"), list) else []
    result.update(
        {
            "run_id": record.get("run_id"),
            "run_status": status,
            "run_duration_ms": record.get("duration_ms"),
            "agent_count": record.get("agent_count", 0),
            "task_count": record.get("task_count", 0),
            "task_names": sorted(
                task["task"]
                for task in task_results
                if isinstance(task, dict) and isinstance(task.get("task"), str)
            ),
            "task_failures": sum(
                1 for task in task_results if isinstance(task, dict) and not task.get("success", False)
            ),
        }
    )
    if run_succeeded:
        try:
            usage = successful_run_usage(
                task_results,
                task_llm_calls,
                task_maximum_output_tokens,
                input_token_costing_allowance_per_request,
                max_completion_tokens_per_request,
            )
            if (
                record.get("total_tokens") != usage["total_tokens"]
                or record.get("cached_tokens") != usage["cached_tokens"]
            ):
                raise ValueError("aggregate run and task token accounting differ")
            result.update(usage)
        except ValueError as error:
            result["failure_reason"] = f"token measurement failed: {error}"
            return result
    if not run_succeeded:
        result["failure_reason"] = f"run status was {status or 'missing'}"
        return result

    final_tasks = [
        task for task in task_results if isinstance(task, dict) and task.get("task") == "final"
    ]
    if len(final_tasks) != 1 or not final_tasks[0].get("success", False):
        result["failure_reason"] = "missing or unsuccessful final task"
        return result
    result["execution_ok"] = True

    final_output = final_tasks[0].get("output")
    if not isinstance(final_output, str):
        result["failure_reason"] = "final task output was not a string"
        return result
    try:
        model_output = json.loads(final_output)
    except json.JSONDecodeError as error:
        result["failure_reason"] = f"final task output was not JSON: {error}"
        return result
    result["output_parse_ok"] = True
    result["model_output"] = model_output
    schema_errors = validate_model_output(model_output, case)
    result["output_schema_ok"] = not schema_errors
    if schema_errors:
        result["failure_reason"] = "; ".join(schema_errors)
    result.update(score_model_output(model_output, case, oracle))
    return result


def contract_failures(runs: list[dict[str, Any]]) -> list[str]:
    failures: list[str] = []
    for run in runs:
        label = f"{run['case_id']}/{run['variant']}/r{run['repetition']}"
        if not run["execution_ok"]:
            failures.append(f"{label}: execution failed: {run['failure_reason']}")
        if not run["output_parse_ok"] or not run["output_schema_ok"]:
            failures.append(f"{label}: output contract failed: {run['failure_reason']}")
        if run["grounded_correct"] != run["answers_total"]:
            failures.append(f"{label}: synthetic oracle output did not score perfectly")
        if run["observed_llm_calls"] != run["planned_llm_calls"]:
            failures.append(
                f"{label}: expected {run['planned_llm_calls']} mock calls, "
                f"observed {run['observed_llm_calls']}"
            )
        if set(run["task_names"]) != EXPECTED_TASKS[run["variant"]]:
            failures.append(
                f"{label}: expected tasks {sorted(EXPECTED_TASKS[run['variant']])}, "
                f"observed {run['task_names']}"
            )
        if run["task_count"] != len(EXPECTED_TASKS[run["variant"]]):
            failures.append(
                f"{label}: expected task_count {len(EXPECTED_TASKS[run['variant']])}, "
                f"observed {run['task_count']}"
            )
        if run["agent_count"] != EXPECTED_AGENTS[run["variant"]]:
            failures.append(
                f"{label}: expected agent_count {EXPECTED_AGENTS[run['variant']]}, "
                f"observed {run['agent_count']}"
            )
    return failures


def validate_report_with_ironcrew(
    *,
    binary: Path,
    repo_root: Path,
    validator_path: Path,
    schema_path: Path,
    report_path: Path,
    environment: dict[str, str],
) -> str | None:
    return validate_report(
        binary=binary,
        repo_root=repo_root,
        validator_path=validator_path,
        schema_path=schema_path,
        report_path=report_path,
        environment=schema_validation_environment(environment),
        redact_error=lambda detail: redact_error(detail, environment),
    )


def build_parser(base_dir: Path, repo_root: Path) -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("contract", "live"), default="contract")
    parser.add_argument("--binary", type=Path, default=repo_root / "target/debug/ironcrew")
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument(
        "--provider-id",
        help="non-secret operator-declared provider identity required for live runs",
    )
    parser.add_argument("--cases", type=Path, default=base_dir / "cases.v1.jsonl")
    parser.add_argument("--oracle", type=Path, default=base_dir / "oracle.v1.jsonl")
    parser.add_argument(
        "--domain-pack-manifest",
        action="append",
        type=Path,
        help="versioned domain-pack manifest; defaults to every checked-in v1 pack",
    )
    parser.add_argument("--plan", type=Path, default=base_dir / "decision-plan.v2.json")
    parser.add_argument("--report", type=Path, default=base_dir / "reports/latest.json")
    parser.add_argument("--repetitions", type=int)
    parser.add_argument("--order-seed", type=int, default=20260719)
    parser.add_argument("--timeout-seconds", type=int, default=180)
    parser.add_argument("--progress-every", type=int, default=10)
    parser.add_argument(
        "--dry-run-plan",
        action="store_true",
        help="validate and print the planned provider workload without executing IronCrew",
    )
    return parser


def main() -> int:
    base_dir = Path(__file__).resolve().parent
    repo_root = base_dir.parent.parent
    args = build_parser(base_dir, repo_root).parse_args()

    binary = args.binary.resolve()
    cases_path = args.cases.resolve()
    oracle_path = args.oracle.resolve()
    plan_path = args.plan.resolve()
    report_path = args.report.resolve()
    try:
        repetitions, provider_id = validate_run_request(
            mode=args.mode,
            repetitions=args.repetitions,
            provider_id=args.provider_id,
            model=args.model,
        )
    except ValueError as error:
        raise SystemExit(str(error)) from error
    if args.timeout_seconds < 1:
        raise SystemExit("--timeout-seconds must be at least 1")
    if args.progress_every < 1:
        raise SystemExit("--progress-every must be at least 1")

    # Bind source before reading any decision-bearing corpus, oracle, plan, or
    # flow bytes. Dry-run is intentionally provider-free and leaves no receipt.
    source_start = None if args.dry_run_plan else worktree_provenance(repo_root)

    try:
        manifest_paths = args.domain_pack_manifest or sorted(
            (base_dir / "domain-packs").glob("*/manifest.v1.json")
        )
        corpus = load_corpus(
            base_cases_path=cases_path,
            base_oracle_path=oracle_path,
            manifest_paths=manifest_paths,
            validate_dataset=validate_dataset,
        )
        plan = load_plan_v2(plan_path, base_dir)
        case_sizes = [
            len(json.dumps(case, sort_keys=True, separators=(",", ":")).encode())
            for case in corpus.cases
        ]
        planned_work = preflight_v2(
            plan=plan,
            corpus_receipt=corpus.receipt,
            case_sizes=case_sizes,
            repetitions=repetitions,
            require_complete=args.mode == "live",
        )
        sanitized_dataset = dataset_receipt(repo_root, corpus.receipt)
    except ValueError as error:
        raise SystemExit(str(error)) from error
    planned_calls = {
        name: plan["flow"]["variants"][name]["planned_llm_calls"] for name in VARIANTS
    }
    planned_task_calls = {
        name: plan["flow"]["variants"][name]["task_llm_calls"] for name in VARIANTS
    }
    planned_task_output_tokens = {
        name: plan["flow"]["variants"][name]["task_maximum_output_tokens"]
        for name in VARIANTS
    }
    planned_output_tokens = {
        name: plan["flow"]["variants"][name]["maximum_output_tokens"]
        for name in VARIANTS
    }
    plan_path_label = plan_path.relative_to(repo_root).as_posix()
    plan_receipt = {
        **plan,
        "path": plan_path_label,
        "sha256": sha256_file(plan_path),
        "planned_work": planned_work,
    }

    if args.dry_run_plan:
        print(
            json.dumps(
                {
                    "mode": args.mode,
                    "model": args.model,
                    "provider_id": provider_id,
                    "dataset": sanitized_dataset,
                    "plan": plan_receipt,
                    "planned_work": planned_work,
                },
                indent=2,
                sort_keys=True,
            )
        )
        return 0

    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise SystemExit(f"IronCrew binary is not executable: {binary}")
    binary_label, binary_path_scope = safe_binary_path(repo_root, binary)
    binary_sha256 = sha256_file(binary)
    assert source_start is not None

    work: list[tuple[int, dict[str, Any], str]] = [
        (repetition, case, variant)
        for repetition in range(repetitions)
        for case in corpus.cases
        for variant in VARIANTS
    ]
    random.Random(args.order_seed).shuffle(work)

    if args.mode == "live":
        try:
            environment = live_provider_environment(repo_root, args.model)
        except ValueError as error:
            raise SystemExit(str(error)) from error
        effective_base = environment.get("OPENAI_BASE_URL", "https://api.openai.com/v1")
        if effective_base.rstrip("/") != "https://api.openai.com/v1":
            raise SystemExit("IC-009 requires the official OpenAI API base URL")
        apply_live_provider_controls(environment, plan)
    else:
        environment = {
            name: value
            for name in (
                "PATH",
                "LANG",
                "LC_ALL",
                "SSL_CERT_FILE",
                "SSL_CERT_DIR",
                "TMPDIR",
            )
            if (value := os.environ.get(name)) is not None
        }
        environment["IRONCREW_PROVIDER_MAX_REQUEST_BYTES"] = str(
            plan["limits"]["max_provider_request_body_bytes"]
        )
    environment["IRONCREW_STORE"] = "json"
    environment["IRONCREW_LOG"] = "error"

    mock_server: ContractServer | None = None
    mock_thread: threading.Thread | None = None
    runs: list[dict[str, Any]] = []
    budget_abort: str | None = None
    live_pacer = (
        LiveProcessPacer(plan["rate_limit"]["minimum_provider_start_interval_ms"])
        if args.mode == "live"
        else None
    )
    try:
        with tempfile.TemporaryDirectory(prefix="ironcrew-crew-eval-") as temporary:
            flow_dir = Path(temporary) / "flow"
            flow_dir.mkdir()
            shutil.copy2(base_dir / "crew.lua", flow_dir / "crew.lua")
            if args.mode == "contract":
                combined_oracle = Path(temporary) / "oracle.jsonl"
                combined_oracle.write_text(
                    "".join(
                        json.dumps(record, sort_keys=True) + "\n"
                        for record in corpus.oracle_records
                    ),
                    encoding="utf-8",
                )
                combined_oracle.chmod(0o600)
                mock_server = create_server(combined_oracle)
                host, port = mock_server.server_address[:2]
                environment["OPENAI_API_KEY"] = "ironcrew-contract-key"
                environment["OPENAI_BASE_URL"] = f"http://{host}:{port}/v1"
                environment["IRONCREW_ALLOW_PRIVATE_IPS"] = "1"
                mock_thread = threading.Thread(target=mock_server.serve_forever, daemon=True)
                mock_thread.start()
            for index, (repetition, case, variant) in enumerate(work, 1):
                if live_pacer is not None:
                    live_pacer.wait_before_start()
                try:
                    run = run_one(
                        binary=binary,
                        repo_root=repo_root,
                        flow_dir=flow_dir,
                        case=case,
                        oracle=corpus.oracle_by_id[case["case_id"]],
                        domain_pack=corpus.case_pack_ids[case["case_id"]],
                        variant=variant,
                        repetition=repetition,
                        model=args.model,
                        mode=args.mode,
                        timeout_seconds=args.timeout_seconds,
                        planned_llm_calls=planned_calls[variant],
                        task_llm_calls=planned_task_calls[variant],
                        task_maximum_output_tokens=planned_task_output_tokens[variant],
                        input_token_costing_allowance_per_request=plan["limits"][
                            "input_token_costing_allowance_per_request"
                        ],
                        max_completion_tokens_per_request=plan["limits"][
                            "max_completion_tokens_per_request"
                        ],
                        base_environment=environment,
                        mock_server=mock_server,
                    )
                finally:
                    if live_pacer is not None:
                        live_pacer.record_completion()
                runs.append(run)
                if (
                    args.mode == "live"
                    and str(run.get("run_status", "")).casefold() == "success"
                    and run.get("estimated_cost_upper_bound_usd") is None
                ):
                    budget_abort = (
                        "successful live run lacked complete token usage and cost accounting"
                    )
                    break
                observed_costs = [
                    item["estimated_cost_upper_bound_usd"]
                    for item in runs
                    if isinstance(item.get("estimated_cost_upper_bound_usd"), (int, float))
                ]
                try:
                    require_approved_budget(sum(observed_costs))
                except ValueError as error:
                    budget_abort = str(error)
                    break
                if index % args.progress_every == 0 or index == len(work):
                    print(
                        f"progress {index}/{len(work)}; successful="
                        f"{sum(item['execution_ok'] for item in runs)}; "
                        f"estimated_upper_bound_usd={sum(observed_costs):.6f}",
                        file=os.sys.stderr,
                        flush=True,
                    )
    finally:
        if mock_server is not None:
            mock_server.shutdown()
            mock_server.server_close()
        if mock_thread is not None:
            mock_thread.join(timeout=5)

    source_end = worktree_provenance(repo_root)
    try:
        require_unchanged_provenance(source_start, source_end)
    except ValueError as error:
        raise SystemExit(str(error)) from error
    if sha256_file(binary) != binary_sha256:
        raise SystemExit("evaluation binary changed during execution")
    summaries = summarize_runs(runs, VARIANTS)
    comparisons = pairwise_comparisons(runs, plan["uncertainty"])
    decision = topology_decision(mode=args.mode, comparisons=comparisons, plan=plan)
    packs = sorted(set(corpus.case_pack_ids.values()))
    report = {
        "schema_version": SCHEMA_VERSION,
        "mode": args.mode,
        "effectiveness_evidence": args.mode == "live",
        "notice": CONTRACT_NOTICE if args.mode == "contract" else LIVE_NOTICE,
        "generated_at": datetime.now(UTC).isoformat(),
        "revision": {"sha": source_start["revision"], "dirty": source_start["dirty"]},
        "source": {"start": source_start, "end": source_end, "unchanged": True},
        "binary": {
            "path": binary_label,
            "path_scope": binary_path_scope,
            "version": command_output([str(binary), "--version"], repo_root),
            "sha256": binary_sha256,
        },
        "dataset": sanitized_dataset,
        "provider": {
            "name": "synthetic-oracle-backed-mock"
            if args.mode == "contract"
            else provider_id,
            "identity_source": "evaluator-contract"
            if args.mode == "contract"
            else "operator-declared",
            "model": args.model,
        },
        "evaluation_plan": plan_receipt,
        "configuration": {
            "repetitions": repetitions,
            "temperature": None,
            "reasoning_effort": None,
            "provider_default_parameters": ["reasoning_effort", "temperature"],
            "order_seed": args.order_seed,
            "variants": list(VARIANTS),
            "planned_llm_calls_per_run": planned_calls,
            "planned_llm_calls_by_task": planned_task_calls,
            "planned_max_output_tokens_by_task": planned_task_output_tokens,
            "planned_max_output_tokens_per_run": planned_output_tokens,
        },
        "mock_provider_stats": mock_server.snapshot() if mock_server else None,
        "runs": sorted(runs, key=lambda run: (run["case_id"], run["repetition"], run["variant"])),
        "summary": summaries,
        "domain_summary": domain_summaries(runs, packs, VARIANTS),
        "pairwise": comparisons,
        "pricing": pricing_receipt(
            mode=args.mode,
            runs=runs,
            planned_upper_bound_usd=planned_work["planned_cost_upper_bound_usd"],
        ),
        "decision": decision,
        "execution": {
            "planned_run_count": len(work),
            "completed_run_count": len(runs),
            "budget_abort": budget_abort,
        },
    }
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_bytes = report_json_bytes(report)
    if any(canary.encode() in report_bytes for canary in redaction_canaries(environment)):
        raise SystemExit("refusing to retain a report containing a credential canary")
    report_path.write_bytes(report_bytes)

    schema_error = validate_report_with_ironcrew(
        binary=binary,
        repo_root=repo_root,
        validator_path=base_dir / "validate-report.lua",
        schema_path=base_dir / "report-v3.schema.json",
        report_path=report_path,
        environment=environment,
    )
    if schema_error:
        print(f"FAIL: generated report did not match report-v3.schema.json: {schema_error}")
        print(f"Report: {report_path}")
        return 1

    if args.mode == "contract":
        failures = contract_failures(runs)
        if failures:
            for failure in failures:
                print(f"FAIL: {failure}")
            print(f"Contract report: {report_path}")
            return 1
        total_answers = sum(run["answers_total"] for run in runs)
        request_count = sum(run["observed_llm_calls"] or 0 for run in runs)
        print(
            f"Contract smoke passed: {len(runs)} CLI runs, {request_count} mock requests, "
            f"{total_answers}/{total_answers} grounded answers."
        )
    else:
        print(f"Live evaluation completed: {len(runs)} CLI runs; decision={decision['status']}.")
    print(f"Report: {report_path}")
    return 1 if budget_abort else 0


if __name__ == "__main__":
    raise SystemExit(main())
