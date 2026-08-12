#!/usr/bin/env python3
"""Deterministic OpenAI-compatible server for evaluator contract tests.

This fixture deliberately reads the scoring oracle and therefore cannot be
used as effectiveness evidence. Its only purpose is to prove that the real
IronCrew CLI, Lua topology, run persistence, JSON output, and scorer are wired
together correctly.
"""

from __future__ import annotations

import argparse
import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


CASE_MARKER = re.compile(r"IRONCREW_EVAL_CASE:([a-z0-9][a-z0-9-]*)")


def load_oracle(path: Path) -> dict[str, dict[str, Any]]:
    records: dict[str, dict[str, Any]] = {}
    with path.open("r", encoding="utf-8") as source:
        for line_number, raw_line in enumerate(source, 1):
            line = raw_line.strip()
            if not line:
                continue
            record = json.loads(line)
            case_id = record.get("case_id")
            if not isinstance(case_id, str) or not case_id:
                raise ValueError(f"{path}:{line_number}: missing case_id")
            if case_id in records:
                raise ValueError(f"{path}:{line_number}: duplicate case_id {case_id}")
            records[case_id] = record
    return records


def oracle_output(case_id: str, oracle: dict[str, dict[str, Any]]) -> str:
    record = oracle.get(case_id)
    if record is None:
        raise KeyError(f"unknown contract case {case_id}")
    answers = []
    for expected in record["answers"]:
        answers.append(
            {
                "question_id": expected["question_id"],
                "answer": expected["correct_option_ids"][0],
                "citations": expected["citation_sets"][0],
            }
        )
    return json.dumps(
        {"case_id": case_id, "answers": answers},
        sort_keys=True,
        separators=(",", ":"),
    )


def request_text(body: dict[str, Any]) -> str:
    chunks: list[str] = []
    for message in body.get("messages", []):
        content = message.get("content")
        if isinstance(content, str):
            chunks.append(content)
        elif isinstance(content, list):
            chunks.append(json.dumps(content, sort_keys=True))
    return "\n".join(chunks)


def classify_request(text: str) -> str:
    if "IRONCREW_EVAL_STAGE:final" in text:
        return "final"
    if "Synthesize the collaborative discussion" in text:
        return "collaboration_synthesis"
    if "IRONCREW_EVAL_STAGE:extract" in text:
        return "extract"
    if "IRONCREW_EVAL_STAGE:challenge" in text:
        return "challenge"
    if "IRONCREW_EVAL_STAGE:discussion" in text:
        return "discussion"
    return "unknown"


class ContractServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(
        self,
        address: tuple[str, int],
        oracle: dict[str, dict[str, Any]],
    ) -> None:
        super().__init__(address, ContractHandler)
        self.oracle = oracle
        self._stats_lock = threading.Lock()
        self._request_count = 0
        self._requests_by_stage: dict[str, int] = {}

    def record(self, stage: str) -> None:
        with self._stats_lock:
            self._request_count += 1
            self._requests_by_stage[stage] = self._requests_by_stage.get(stage, 0) + 1

    def snapshot(self) -> dict[str, Any]:
        with self._stats_lock:
            return {
                "request_count": self._request_count,
                "requests_by_stage": dict(self._requests_by_stage),
            }


class ContractHandler(BaseHTTPRequestHandler):
    server: ContractServer
    protocol_version = "HTTP/1.1"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def send_json(self, status: int, value: dict[str, Any]) -> None:
        payload = json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        if self.path.rstrip("/") != "/v1/chat/completions":
            self.send_json(404, {"error": {"message": "not found"}})
            return

        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self.send_json(400, {"error": {"message": "invalid content length"}})
            return
        if length <= 0 or length > 2 * 1024 * 1024:
            self.send_json(413, {"error": {"message": "request body out of bounds"}})
            return

        try:
            body = json.loads(self.rfile.read(length))
            text = request_text(body)
            marker = CASE_MARKER.search(text)
            if marker is None:
                raise ValueError("missing evaluation case marker")
            case_id = marker.group(1)
            stage = classify_request(text)
            self.server.record(stage)

            if stage == "final":
                content = oracle_output(case_id, self.server.oracle)
            elif stage == "extract":
                content = f"Contract extraction for {case_id}: candidate answers retain evidence IDs."
            elif stage == "challenge":
                content = f"Contract challenge for {case_id}: check conflicts and unsupported certainty."
            elif stage == "collaboration_synthesis":
                content = f"Contract board synthesis for {case_id}: integrate analysis and skepticism."
            elif stage == "discussion":
                content = f"Contract discussion turn for {case_id}: remain grounded in the packet."
            else:
                raise ValueError("unknown evaluation stage")

            prompt_bytes = len(json.dumps(body.get("messages", []), sort_keys=True))
            prompt_tokens = max(1, prompt_bytes // 4)
            completion_tokens = max(1, len(content.encode("utf-8")) // 4)
            response = {
                "id": f"contract-{case_id}-{stage}",
                "object": "chat.completion",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": content},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens,
                    "prompt_tokens_details": {"cached_tokens": 0},
                },
            }
            self.send_json(200, response)
        except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
            self.send_json(400, {"error": {"message": str(error)}})


def create_server(oracle_path: Path, host: str = "127.0.0.1", port: int = 0) -> ContractServer:
    return ContractServer((host, port), load_oracle(oracle_path))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle", type=Path, required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args()

    server = create_server(args.oracle, args.host, args.port)
    host, port = server.server_address[:2]
    print(f"http://{host}:{port}/v1", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
