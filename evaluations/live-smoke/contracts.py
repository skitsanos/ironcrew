"""Assertions use state, receipts and actual effects, never generated phrasing."""
import hashlib
import json
from pathlib import Path

from plan import RUN_TOKENS
from support import SmokeError, classify_failure, costing_counts, decimal_count


class HumanDriver:
    def __init__(self):
        self.decisions = []

    def answer(self, prompt: str, kind: str) -> str:
        step = len(self.decisions)
        if step >= 4:
            raise SmokeError("unexpected_human_question")
        agent = "writer" if step < 2 else "reviewer"
        expected = "question" if step % 2 == 0 else "approval"
        if kind != expected or (f"[{agent}]" not in prompt and f"Agent '{agent}'" not in prompt):
            raise SmokeError("unexpected_human_question")
        if kind == "approval" and ("file_write(" not in prompt or f"output/{agent}.txt" not in prompt):
            raise SmokeError("unexpected_approval_target")
        decision = "confirmed" if kind == "question" else ("allow" if agent == "writer" else "deny")
        self.decisions.append({"agent": agent, "kind": kind, "decision": decision})
        return decision

    def complete(self):
        if len(self.decisions) != 4:
            raise SmokeError("missing_human_interaction")


def marker(stdout: bytes) -> dict:
    lines = [line.split(b"IC048_RESULT ", 1)[1] for line in stdout.splitlines()
             if line.startswith(b"IC048_RESULT ")]
    if len(lines) != 1:
        raise SmokeError("missing_result_marker")
    try:
        result = json.loads(lines[0])
    except (ValueError, UnicodeError):
        raise SmokeError("malformed_result_marker") from None
    if not isinstance(result, dict):
        raise SmokeError("malformed_result_marker")
    return result


def nonblank(text):
    if not isinstance(text, str) or not text.strip():
        raise SmokeError("blank_successful_output")


def verify(case: dict, payload: dict, directory: Path, live: bool, driver=None) -> dict:
    results = payload.get("results", payload.get("task_results", []))
    if any(result.get("success") is False for result in results):
        errors = json.dumps([result.get("output") for result in results]).encode()
        raise SmokeError(classify_failure(errors))
    usage = payload.get("usage")
    try:
        requests = decimal_count(usage["settled"]["requests"])
        counts = costing_counts(usage, requests)
        budget = usage["budget"]
        if not 1 <= requests <= case["max_requests"] or counts["total_tokens"] > RUN_TOKENS:
            raise ValueError("receipt limit")
        if counts["prompt_tokens"] + counts["completion_tokens"] != counts["total_tokens"]:
            raise ValueError("receipt total")
        if budget["state"] != ("active" if live else "disabled"):
            raise ValueError("budget state")
        if live and (decimal_count(budget["limit"]) != RUN_TOKENS
                     or decimal_count(budget["charged"]) != counts["total_tokens"]
                     or any(decimal_count(budget[key]) for key in ("retained", "reserved", "in_flight"))):
            raise ValueError("budget reconciliation")
    except (KeyError, TypeError, ValueError):
        raise SmokeError("incomplete_or_out_of_bounds_usage") from None
    texts = []
    detail = {}
    if case["id"] == "cli-dialog":
        turns = payload.get("turns", [])
        if (len(turns) != 2 or payload.get("reason") != "smoke_turn_cap"
                or payload.get("stopped") is not True or requests != 2
                or [turn.get("agent") for turn in turns] != ["alice", "bob"]):
            raise SmokeError("dialog_stop_contract")
        texts = [turn.get("content") for turn in turns]
        detail = {"turns": 2, "stop_reason": "smoke_turn_cap", "streaming": True}
    else:
        results = payload.get("results", payload.get("task_results", []))
        names = ["writer", "reviewer"] if "hitl" in case["id"] else ["answer"]
        if (len(results) != len(names) or [result.get("task") for result in results] != names
                or any(result.get("success") is not True for result in results)):
            raise SmokeError("task_terminal_contract")
        texts = [result.get("output") for result in results]
        if driver is not None:
            driver.complete()
            for result, expected in zip(results, (True, False)):
                try:
                    decision = json.loads(result["output"])
                except (ValueError, TypeError):
                    raise SmokeError("invalid_human_decision_output") from None
                if not isinstance(decision, dict) or decision.get("performed") is not expected:
                    raise SmokeError("untruthful_tool_outcome")
                nonblank(decision.get("summary"))
            written = directory / "output/writer.txt"
            if (not written.is_file() or written.is_symlink() or not written.stat().st_size
                    or (directory / "output/reviewer.txt").exists()):
                raise SmokeError("approval_effect_contract")
            detail = {"human_decisions": driver.decisions, "approved_write_present": True,
                      "denied_write_absent": True}
    for text in texts:
        nonblank(text)
    return {"status": "passed", "usage": usage, "generation_requests": requests,
            "conservative_cost_estimate_usd": round(counts["total_tokens"] * 3 / 1_000_000, 8),
            "outputs": [{"bytes": len(text.encode()), "sha256": hashlib.sha256(text.encode()).hexdigest()}
                        for text in texts], **detail}
