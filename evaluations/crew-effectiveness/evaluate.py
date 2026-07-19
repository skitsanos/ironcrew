#!/usr/bin/env python3
"""Run and score the IronCrew grounded crew-effectiveness evaluation."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import random
import re
import shutil
import statistics
import subprocess
import tempfile
import threading
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Iterable

from mock_openai import ContractServer, create_server


SCHEMA_VERSION = "ironcrew.crew-eval.v1"
VARIANTS = ("single", "dag", "collaborative")
PLANNED_LLM_CALLS = {"single": 1, "dag": 3, "collaborative": 4}
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
    "Exploratory live-provider evidence on a small synthetic dataset. Interpret "
    "quality, token, and latency deltas together; this is not broad proof of superiority."
)


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
    variant: str,
    repetition: int,
    model: str,
    mode: str,
    timeout_seconds: int,
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
            cwd=repo_root,
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
            "variant": variant,
            "repetition": repetition,
            "planned_llm_calls": PLANNED_LLM_CALLS[variant],
            "observed_llm_calls": observed,
            "execution_ok": False,
            "output_parse_ok": False,
            "output_schema_ok": False,
            "failure_reason": f"CLI timeout after {timeout_seconds} seconds",
            "client_duration_ms": int((time.monotonic() - started) * 1000),
            "run_duration_ms": None,
            "run_id": None,
            "run_status": None,
            "total_tokens": 0,
            "cached_tokens": 0,
            "agent_count": 0,
            "task_count": 0,
            "task_names": [],
            "task_failures": 0,
            "model_output": None,
        }
        result.update(empty_score(oracle))
        return result

    observed = None
    if mock_server and before_requests is not None:
        observed = mock_server.snapshot()["request_count"] - before_requests
    result = {
        "case_id": case["case_id"],
        "variant": variant,
        "repetition": repetition,
        "planned_llm_calls": PLANNED_LLM_CALLS[variant],
        "observed_llm_calls": observed,
        "execution_ok": False,
        "output_parse_ok": False,
        "output_schema_ok": False,
        "failure_reason": None,
        "client_duration_ms": client_duration_ms,
        "run_duration_ms": None,
        "run_id": None,
        "run_status": None,
        "total_tokens": 0,
        "cached_tokens": 0,
        "agent_count": 0,
        "task_count": 0,
        "task_names": [],
        "task_failures": 0,
        "model_output": None,
    }
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
    task_results = record.get("task_results") if isinstance(record.get("task_results"), list) else []
    result.update(
        {
            "run_id": record.get("run_id"),
            "run_status": status,
            "run_duration_ms": record.get("duration_ms"),
            "total_tokens": record.get("total_tokens", 0),
            "cached_tokens": record.get("cached_tokens", 0),
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
    if status.casefold() != "success":
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


def percentile_95(values: list[int]) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    return float(ordered[max(0, math.ceil(0.95 * len(ordered)) - 1)])


def summarize_runs(runs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    summaries: list[dict[str, Any]] = []
    for variant in VARIANTS:
        selected = [run for run in runs if run["variant"] == variant]
        answers_total = sum(run["answers_total"] for run in selected)
        correct = sum(run["answers_correct"] for run in selected)
        grounded = sum(run["grounded_correct"] for run in selected)
        citation_tp = sum(run["citation_tp"] for run in selected)
        citation_fp = sum(run["citation_fp"] for run in selected)
        citation_fn = sum(run["citation_fn"] for run in selected)
        precision = citation_tp / (citation_tp + citation_fp) if citation_tp + citation_fp else 0.0
        recall = citation_tp / (citation_tp + citation_fn) if citation_tp + citation_fn else 0.0
        f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
        durations = [
            int(run["run_duration_ms"])
            for run in selected
            if isinstance(run.get("run_duration_ms"), int)
        ]
        tokens = [int(run["total_tokens"]) for run in selected if isinstance(run.get("total_tokens"), int)]
        summaries.append(
            {
                "variant": variant,
                "run_count": len(selected),
                "execution_success_rate": sum(run["execution_ok"] for run in selected)
                / len(selected),
                "parse_success_rate": sum(run["output_parse_ok"] for run in selected)
                / len(selected),
                "schema_success_rate": sum(run["output_schema_ok"] for run in selected)
                / len(selected),
                "correctness": correct / answers_total if answers_total else 0.0,
                "grounded_correctness": grounded / answers_total if answers_total else 0.0,
                "citation_precision": precision,
                "citation_recall": recall,
                "citation_f1": f1,
                "latency_ms": {
                    "median": float(statistics.median(durations)) if durations else None,
                    "p95": percentile_95(durations),
                },
                "tokens": {
                    "median": float(statistics.median(tokens)) if tokens else None,
                    "total": sum(tokens),
                },
            }
        )
    return summaries


def pairwise_comparisons(runs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    indexed = {
        (run["case_id"], run["repetition"], run["variant"]): run for run in runs
    }
    comparisons: list[dict[str, Any]] = []
    for variant in ("dag", "collaborative"):
        deltas: list[float] = []
        wins = ties = losses = 0
        for case_id, repetition, candidate in [
            (case_id, repetition, run)
            for (case_id, repetition, run_variant), run in indexed.items()
            if run_variant == variant
        ]:
            baseline = indexed.get((case_id, repetition, "single"))
            if baseline is None:
                continue
            delta = candidate["grounded_correctness"] - baseline["grounded_correctness"]
            deltas.append(delta)
            if delta > 0:
                wins += 1
            elif delta < 0:
                losses += 1
            else:
                ties += 1
        comparisons.append(
            {
                "candidate": variant,
                "baseline": "single",
                "paired_count": len(deltas),
                "mean_grounded_correctness_delta": statistics.fmean(deltas) if deltas else None,
                "wins": wins,
                "ties": ties,
                "losses": losses,
            }
        )
    return comparisons


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
    payload = {
        "schema": schema_path.read_text(encoding="utf-8"),
        "report": report_path.read_text(encoding="utf-8"),
    }
    try:
        with tempfile.TemporaryDirectory(prefix="ironcrew-eval-schema-") as temporary:
            isolated_validator = Path(temporary) / "validate-report.lua"
            shutil.copy2(validator_path, isolated_validator)
            completed = subprocess.run(
                [
                    str(binary),
                    "run",
                    str(isolated_validator),
                    "--input",
                    json.dumps(payload, separators=(",", ":")),
                ],
                cwd=repo_root,
                env=environment,
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
    except (OSError, subprocess.SubprocessError) as error:
        return f"could not execute report schema validator: {error}"
    if completed.returncode != 0:
        detail = redact_error(completed.stderr, environment)
        return detail or f"report schema validator exited with status {completed.returncode}"
    return None


def build_parser(base_dir: Path, repo_root: Path) -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("contract", "live"), default="contract")
    parser.add_argument("--binary", type=Path, default=repo_root / "target/debug/ironcrew")
    parser.add_argument("--model", default="gpt-4.1-mini")
    parser.add_argument("--cases", type=Path, default=base_dir / "cases.v1.jsonl")
    parser.add_argument("--oracle", type=Path, default=base_dir / "oracle.v1.jsonl")
    parser.add_argument("--report", type=Path, default=base_dir / "reports/latest.json")
    parser.add_argument("--repetitions", type=int, default=1)
    parser.add_argument("--order-seed", type=int, default=20260719)
    parser.add_argument("--case-limit", type=int)
    parser.add_argument("--timeout-seconds", type=int, default=180)
    return parser


def main() -> int:
    base_dir = Path(__file__).resolve().parent
    repo_root = base_dir.parent.parent
    args = build_parser(base_dir, repo_root).parse_args()

    binary = args.binary.resolve()
    cases_path = args.cases.resolve()
    oracle_path = args.oracle.resolve()
    report_path = args.report.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise SystemExit(f"IronCrew binary is not executable: {binary}")
    if args.repetitions < 1:
        raise SystemExit("--repetitions must be at least 1")
    if args.case_limit is not None and args.case_limit < 1:
        raise SystemExit("--case-limit must be at least 1")
    if args.timeout_seconds < 1:
        raise SystemExit("--timeout-seconds must be at least 1")

    cases = load_jsonl(cases_path)
    oracle_records = load_jsonl(oracle_path)
    oracle_by_id = validate_dataset(cases, oracle_records)
    if args.case_limit is not None:
        cases = cases[: args.case_limit]

    work: list[tuple[int, dict[str, Any], str]] = [
        (repetition, case, variant)
        for repetition in range(args.repetitions)
        for case in cases
        for variant in VARIANTS
    ]
    random.Random(args.order_seed).shuffle(work)

    environment = os.environ.copy()
    environment["IRONCREW_STORE"] = "json"
    environment["IRONCREW_LOG"] = "error"

    mock_server: ContractServer | None = None
    mock_thread: threading.Thread | None = None
    if args.mode == "contract":
        mock_server = create_server(oracle_path)
        host, port = mock_server.server_address[:2]
        environment["OPENAI_API_KEY"] = "ironcrew-contract-key"
        environment["OPENAI_BASE_URL"] = f"http://{host}:{port}/v1"
        environment["IRONCREW_ALLOW_PRIVATE_IPS"] = "1"
        mock_thread = threading.Thread(target=mock_server.serve_forever, daemon=True)
        mock_thread.start()

    runs: list[dict[str, Any]] = []
    try:
        with tempfile.TemporaryDirectory(prefix="ironcrew-crew-eval-") as temporary:
            flow_dir = Path(temporary) / "flow"
            flow_dir.mkdir()
            shutil.copy2(base_dir / "crew.lua", flow_dir / "crew.lua")
            for repetition, case, variant in work:
                run = run_one(
                    binary=binary,
                    repo_root=repo_root,
                    flow_dir=flow_dir,
                    case=case,
                    oracle=oracle_by_id[case["case_id"]],
                    variant=variant,
                    repetition=repetition,
                    model=args.model,
                    mode=args.mode,
                    timeout_seconds=args.timeout_seconds,
                    base_environment=environment,
                    mock_server=mock_server,
                )
                runs.append(run)
    finally:
        if mock_server is not None:
            mock_server.shutdown()
            mock_server.server_close()
        if mock_thread is not None:
            mock_thread.join(timeout=5)

    summaries = summarize_runs(runs)
    report = {
        "schema_version": SCHEMA_VERSION,
        "mode": args.mode,
        "effectiveness_evidence": args.mode == "live",
        "notice": CONTRACT_NOTICE if args.mode == "contract" else LIVE_NOTICE,
        "generated_at": datetime.now(UTC).isoformat(),
        "revision": git_metadata(repo_root),
        "binary": {
            "path": str(binary),
            "version": command_output([str(binary), "--version"], repo_root),
            "sha256": sha256_file(binary),
        },
        "dataset": {
            "name": "grounded-decisions-options-v1",
            "answer_contract": "source-visible-single-select-v1",
            "correctness_rule": "exact-option-id-v1",
            "case_count": len(cases),
            "cases_sha256": sha256_file(cases_path),
            "oracle_sha256": sha256_file(oracle_path),
            "oracle_injected_into_prompt": False,
        },
        "provider": {
            "name": "synthetic-oracle-backed-mock"
            if args.mode == "contract"
            else "process-configured-openai-compatible",
            "model": args.model,
        },
        "configuration": {
            "repetitions": args.repetitions,
            "temperature": 0.0,
            "order_seed": args.order_seed,
            "variants": list(VARIANTS),
            "planned_llm_calls_per_run": PLANNED_LLM_CALLS,
        },
        "mock_provider_stats": mock_server.snapshot() if mock_server else None,
        "runs": sorted(runs, key=lambda run: (run["case_id"], run["repetition"], run["variant"])),
        "summary": summaries,
        "pairwise": pairwise_comparisons(runs),
    }
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    schema_error = validate_report_with_ironcrew(
        binary=binary,
        repo_root=repo_root,
        validator_path=base_dir / "validate-report.lua",
        schema_path=base_dir / "report-v1.schema.json",
        report_path=report_path,
        environment=environment,
    )
    if schema_error:
        print(f"FAIL: generated report did not match report-v1.schema.json: {schema_error}")
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
        print(f"Live exploratory evaluation completed: {len(runs)} CLI runs.")
    print(f"Report: {report_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
