#!/usr/bin/env python3
"""Execute every IC-007 canary flow through a real local HTTP server."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

from http_contract import ContractClient
from mock_provider import ProviderFixture


TOKEN = "ic007-offline-runtime-token-123456"
MAX_LOG_BYTES = 1024 * 1024
POLL_ATTEMPTS = 120
POLL_SECONDS = 0.1


class SmokeError(RuntimeError):
    """A fixed-message runtime-smoke failure."""


def _reserve_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def _wait_ready(base_url: str, process: subprocess.Popen[bytes]) -> None:
    for _attempt in range(POLL_ATTEMPTS):
        if process.poll() is not None:
            raise SmokeError("IronCrew exited before readiness")
        try:
            with urllib.request.urlopen(f"{base_url}/health/ready", timeout=1) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(POLL_SECONDS)
    raise SmokeError("IronCrew readiness timed out")


def _response(record: dict[str, object]) -> dict[str, Any]:
    value = record.get("response")
    if not isinstance(value, dict):
        raise SmokeError("HTTP response did not contain a JSON object")
    return value


def _run_id(record: dict[str, object]) -> str:
    value = _response(record).get("run_id")
    if not isinstance(value, str) or not value:
        raise SmokeError("run start did not return an identifier")
    return value


def _question_id(record: dict[str, object], previous: str | None = None) -> str | None:
    questions = _response(record).get("questions")
    if not isinstance(questions, list) or not questions:
        return None
    question = questions[0]
    if not isinstance(question, dict):
        return None
    value = question.get("question_id")
    return value if isinstance(value, str) and value != previous else None


def _wait_question(
    client: ContractClient,
    flow: str,
    run_id: str,
    previous: str | None = None,
) -> str:
    for _attempt in range(POLL_ATTEMPTS):
        record = client.poll_questions(flow, run_id)
        if record.get("status") == 200:
            value = _question_id(record, previous)
            if value is not None:
                return value
        time.sleep(POLL_SECONDS)
    raise SmokeError("human-input question timed out")


def _wait_success(client: ContractClient, flow: str, run_id: str) -> None:
    for _attempt in range(POLL_ATTEMPTS):
        record = client._request_json(  # noqa: SLF001 - same-package smoke helper
            "run_read",
            "GET",
            f"/flows/{flow}/runs/{run_id}",
        )
        if record.get("status") == 200:
            status = _response(record).get("status")
            if isinstance(status, str) and status.lower() == "success":
                return
        time.sleep(POLL_SECONDS)
    raise SmokeError("run did not reach Success")


def _answer(
    client: ContractClient,
    flow: str,
    run_id: str,
    question_id: str,
    answer: str,
) -> None:
    record = client.answer_question(flow, run_id, question_id, answer)
    if record.get("status") not in {200, 202}:
        raise SmokeError("human-input answer was not accepted")


def _exercise(client: ContractClient) -> dict[str, int]:
    provider_run = _run_id(client.start_run("provider-effect", {}, "smoke-provider"))
    _wait_success(client, "provider-effect", provider_run)

    shared_run = _run_id(client.start_run("shared-control", {}, "smoke-shared"))
    first = _wait_question(client, "shared-control", shared_run)
    _answer(client, "shared-control", shared_run, first, "checkpoint-one-approved")
    second = _wait_question(client, "shared-control", shared_run, first)
    _answer(client, "shared-control", shared_run, second, "checkpoint-two-approved")
    _wait_success(client, "shared-control", shared_run)

    admission_run = _run_id(client.start_run("admission", {}, "smoke-admission"))
    admission_question = _wait_question(client, "admission", admission_run)
    _answer(client, "admission", admission_run, admission_question, "release")
    _wait_success(client, "admission", admission_run)

    unkeyed = client._request_json(  # noqa: SLF001 - intentionally no key
        "unkeyed_run_start",
        "POST",
        "/flows/unkeyed-owner/run",
        payload={},
    )
    if unkeyed.get("status") != 200:
        raise SmokeError("unkeyed run was not accepted")
    unkeyed_run = _run_id(unkeyed)
    unkeyed_question = _wait_question(client, "unkeyed-owner", unkeyed_run)
    _answer(client, "unkeyed-owner", unkeyed_run, unkeyed_question, "continue")
    _wait_success(client, "unkeyed-owner", unkeyed_run)
    return {"flows_executed": 4, "questions_answered": 4, "terminal_success": 4}


def _child_environment(root: Path, provider_base_url: str) -> dict[str, str]:
    environment = {
        "HOME": str(root),
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "PYTHONDONTWRITEBYTECODE": "1",
        "IRONCREW_STORE": "sqlite",
        "IRONCREW_STORE_PATH": str(root / "ironcrew.db"),
        "IRONCREW_API_TOKEN": TOKEN,
        "IRONCREW_API_PRINCIPAL": "platform-canary",
        "IRONCREW_REQUIRE_IDEMPOTENCY_KEY": "false",
        "IRONCREW_ALLOW_PRIVATE_IPS": "true",
        "IRONCREW_ENV_ALLOWLIST": "PLATFORM_CANARY_PROVIDER_BASE_URL",
        "PLATFORM_CANARY_PROVIDER_BASE_URL": provider_base_url,
        "IRONCREW_ASK_HUMAN_MAX_TIMEOUT": "300",
        "IRONCREW_FILE_WRITE_ROOT": str(root / "outputs"),
        "IRONCREW_MCP_ALLOWED_COMMANDS": "__disabled__",
        "IRONCREW_MCP_ALLOWED_HTTP_HOSTS": "__disabled__",
        "IRONCREW_LOG": "error",
    }
    return environment


def run_smoke(binary: Path, flow_root: Path) -> dict[str, object]:
    binary = binary.resolve(strict=True)
    flow_root = flow_root.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK) or not flow_root.is_dir():
        raise SmokeError("runtime-smoke inputs are invalid")
    with tempfile.TemporaryDirectory(prefix="ironcrew-platform-smoke-") as directory:
        root = Path(directory)
        flows = root / "flows"
        shutil.copytree(flow_root, flows)
        (root / "outputs").mkdir()
        log_path = root / "ironcrew.log"
        port = _reserve_port()
        base_url = f"http://127.0.0.1:{port}"
        with ProviderFixture() as provider, log_path.open("wb") as log:
            provider_root = provider.base_url.removesuffix("/v1")
            process = subprocess.Popen(
                [str(binary), "serve", "--host", "127.0.0.1", "--port", str(port), "--flows-dir", str(flows)],
                cwd=root,
                env=_child_environment(root, provider.base_url),
                stdout=log,
                stderr=subprocess.STDOUT,
            )
            try:
                _wait_ready(base_url, process)
                client = ContractClient(base_url, TOKEN, mock_base_url=provider_root)
                result = _exercise(client)
                counts = provider.counters.snapshot()
                expected = {"chat_completions": 4, "effect_calls": 2, "final_responses": 2, "tool_call_responses": 2}
                if counts != expected:
                    raise SmokeError("provider-effect counters were not exact")
            finally:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                    raise SmokeError("IronCrew did not stop within the deadline") from None
            if process.returncode != 0:
                raise SmokeError("IronCrew exited unsuccessfully")
        log_bytes = log_path.read_bytes()
        if len(log_bytes) > MAX_LOG_BYTES or TOKEN.encode() in log_bytes:
            raise SmokeError("IronCrew runtime log failed the bounded secrecy check")
        return {**result, "provider_counts": counts, "server_exit_code": process.returncode}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ironcrew-bin", type=Path, required=True)
    parser.add_argument("--flow-root", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = run_smoke(args.ironcrew_bin, args.flow_root)
    except (OSError, SmokeError) as error:
        parser.error(str(error))
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
