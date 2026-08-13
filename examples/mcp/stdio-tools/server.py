#!/usr/bin/env python3
"""Dependency-free MCP 2026-07-28 stdio server used by the example and tests."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any

PROTOCOL_VERSION = "2026-07-28"
SERVER_INFO_KEY = "io.modelcontextprotocol/serverInfo"
PROTOCOL_VERSION_KEY = "io.modelcontextprotocol/protocolVersion"
CLIENT_INFO_KEY = "io.modelcontextprotocol/clientInfo"
CLIENT_CAPABILITIES_KEY = "io.modelcontextprotocol/clientCapabilities"
ECHO_STATE = "opaque:\x00π\\\"\n"

TOOLS = [
    {
        "name": "echo",
        "description": "Return the supplied text.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": False,
        },
    },
    {
        "name": "stateful_echo",
        "description": "Complete after one state-only MRTR retry.",
        "inputSchema": {"type": "object", "additionalProperties": False},
    },
    {
        "name": "empty_state",
        "description": "Complete after echoing an empty requestState.",
        "inputSchema": {"type": "object", "additionalProperties": False},
    },
    {
        "name": "loop_forever",
        "description": "Return state-only input_required forever.",
        "inputSchema": {"type": "object", "additionalProperties": False},
    },
    {
        "name": "oversized_state",
        "description": "Return requestState one byte over IronCrew's default cap.",
        "inputSchema": {"type": "object", "additionalProperties": False},
    },
    {
        "name": "input_request",
        "description": "Return an unadvertised elicitation request.",
        "inputSchema": {"type": "object", "additionalProperties": False},
    },
    {
        "name": "empty_input",
        "description": "Return an invalid effective-empty input_required result.",
        "inputSchema": {"type": "object", "additionalProperties": False},
    },
    {
        "name": "task",
        "description": "Return an unadvertised Tasks extension result.",
        "inputSchema": {"type": "object", "additionalProperties": False},
    },
]


def log_request(request: dict[str, Any]) -> None:
    path = os.environ.get("MCP_FIXTURE_LOG_FILE")
    if not path:
        return
    params = request.get("params") or {}
    entry = {
        "method": request.get("method"),
        "name": params.get("name"),
        "has_request_state": "requestState" in params,
    }
    with Path(path).open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(entry, ensure_ascii=False) + "\n")


def validate_request(request: dict[str, Any], *, first: bool) -> None:
    if first and request.get("method") != "server/discover":
        raise ValueError("first request must be server/discover")
    if request.get("method") in {"initialize", "notifications/initialized"}:
        raise ValueError("legacy lifecycle is not supported")

    params = request.get("params")
    if not isinstance(params, dict):
        raise ValueError("request params must be an object")
    meta = params.get("_meta")
    if not isinstance(meta, dict):
        raise ValueError("request _meta is required")
    if meta.get(PROTOCOL_VERSION_KEY) != PROTOCOL_VERSION:
        raise ValueError("request protocol version must be 2026-07-28")
    client_info = meta.get(CLIENT_INFO_KEY)
    if not isinstance(client_info, dict) or client_info.get("name") != "ironcrew":
        raise ValueError("request clientInfo must identify IronCrew")
    capabilities = meta.get(CLIENT_CAPABILITIES_KEY)
    if not isinstance(capabilities, dict):
        raise ValueError("request clientCapabilities must be an object")
    if capabilities:
        raise ValueError("fixture expects no optional client capabilities")


def result(request_id: Any, payload: dict[str, Any]) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "result": payload}


def error(request_id: Any, message: str) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": -32600, "message": message},
    }


def method_not_found(request_id: Any, message: str) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": -32601, "message": message},
    }


def complete_text(request_id: Any, text: str) -> dict[str, Any]:
    return result(
        request_id,
        {
            "resultType": "complete",
            "content": [{"type": "text", "text": text}],
            "isError": False,
        },
    )


def handle(request: dict[str, Any]) -> dict[str, Any]:
    request_id = request.get("id")
    method = request.get("method")
    params = request.get("params") or {}

    if method == "server/discover":
        if os.environ.get("MCP_FIXTURE_LEGACY_ONLY") == "1":
            return method_not_found(request_id, "server/discover is unavailable")
        supported_version = os.environ.get(
            "MCP_FIXTURE_SUPPORTED_VERSION", PROTOCOL_VERSION
        )
        return result(
            request_id,
            {
                "resultType": "complete",
                "supportedVersions": [supported_version],
                "capabilities": {"tools": {}},
                "_meta": {
                    SERVER_INFO_KEY: {
                        "name": "ironcrew-mcp-2026-fixture",
                        "version": "1.0.0",
                    }
                },
                "instructions": "Deterministic IronCrew protocol fixture.",
                "ttlMs": 60_000,
                "cacheScope": "private",
            },
        )
    if method == "tools/list":
        return result(
            request_id,
            {
                "resultType": "complete",
                "tools": TOOLS,
                "ttlMs": 60_000,
                "cacheScope": "private",
            },
        )
    if method != "tools/call":
        return error(request_id, f"unsupported method: {method}")

    name = params.get("name")
    if name == "echo":
        arguments = params.get("arguments") or {}
        return complete_text(request_id, str(arguments.get("text", "")))
    if name == "stateful_echo":
        if "requestState" not in params:
            return result(
                request_id,
                {"resultType": "input_required", "requestState": ECHO_STATE},
            )
        if params.get("requestState") != ECHO_STATE:
            return error(request_id, "requestState was not echoed exactly")
        if "inputResponses" in params:
            return error(request_id, "state-only retry must not include inputResponses")
        return complete_text(request_id, "state-echo-ok")
    if name == "empty_state":
        if "requestState" not in params:
            return result(
                request_id,
                {"resultType": "input_required", "requestState": ""},
            )
        if params.get("requestState") != "":
            return error(request_id, "empty requestState was not echoed exactly")
        return complete_text(request_id, "empty-state-ok")
    if name == "loop_forever":
        return result(
            request_id,
            {"resultType": "input_required", "requestState": "retry-later"},
        )
    if name == "oversized_state":
        return result(
            request_id,
            {"resultType": "input_required", "requestState": "x" * 65_537},
        )
    if name == "input_request":
        return result(
            request_id,
            {
                "resultType": "input_required",
                "inputRequests": {
                    "approval": {
                        "method": "elicitation/create",
                        "params": {
                            "mode": "form",
                            "message": "Approve fixture action?",
                            "requestedSchema": {
                                "type": "object",
                                "properties": {"approved": {"type": "boolean"}},
                                "required": ["approved"],
                            },
                        },
                    }
                },
            },
        )
    if name == "empty_input":
        return result(
            request_id,
            {"resultType": "input_required", "inputRequests": {}},
        )
    if name == "task":
        return result(
            request_id,
            {
                "resultType": "task",
                "taskId": "fixture-task",
                "status": "working",
                "createdAt": "2026-08-13T00:00:00Z",
                "lastUpdatedAt": "2026-08-13T00:00:00Z",
                "ttlMs": 60_000,
                "pollIntervalMs": 100,
            },
        )
    return error(request_id, f"unknown tool: {name}")


def main() -> int:
    pid_file = os.environ.get("MCP_FIXTURE_PID_FILE")
    if pid_file:
        Path(pid_file).write_text(str(os.getpid()), encoding="ascii")

    first = True
    for raw_line in sys.stdin.buffer:
        request: Any = None
        try:
            request = json.loads(raw_line)
            if not isinstance(request, dict):
                raise ValueError("request must be an object")
            log_request(request)
            validate_request(request, first=first)
            first = False
            response = handle(request)
        except Exception as exc:  # fixture must turn malformed input into protocol evidence
            request_id = request.get("id") if isinstance(request, dict) else None
            response = error(request_id, str(exc))
        sys.stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
