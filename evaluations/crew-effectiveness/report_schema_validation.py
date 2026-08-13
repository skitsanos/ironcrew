"""Bounded file transport for IronCrew-backed report schema validation."""

from __future__ import annotations

import base64
import json
import subprocess
import tempfile
from pathlib import Path
from typing import Callable


MODULE_CHUNK_BYTES = 64 * 1024
MAX_MODULES = 128
MAX_DOCUMENT_BYTES = 2 * 1024 * 1024


def _read_document(path: Path, label: str) -> bytes:
    if not path.is_file():
        raise ValueError(f"report {label} must be a regular file")
    with path.open("rb") as source:
        document = source.read(MAX_DOCUMENT_BYTES + 1)
    if len(document) > MAX_DOCUMENT_BYTES:
        raise ValueError(f"report {label} exceeds the validator document limit")
    return document


def _write_document_modules(directory: Path, label: str, document: bytes) -> int:
    encoded = base64.b64encode(document).decode("ascii")
    chunks = [
        encoded[offset : offset + MODULE_CHUNK_BYTES]
        for offset in range(0, len(encoded), MODULE_CHUNK_BYTES)
    ] or [""]
    if len(chunks) > MAX_MODULES:
        raise ValueError(f"report {label} exceeds the validator transport limit")
    for index, chunk in enumerate(chunks, 1):
        module = directory / f"validator_{label}_{index:04d}.lua"
        module.write_text(f"return {json.dumps(chunk)}\n", encoding="utf-8")
    return len(chunks)


def validate_report(
    *,
    binary: Path,
    repo_root: Path,
    validator_path: Path,
    schema_path: Path,
    report_path: Path,
    environment: dict[str, str],
    redact_error: Callable[[str], str],
) -> str | None:
    """Validate a bounded report without putting report bytes in process argv."""
    try:
        root = repo_root.resolve(strict=True)
        resolved_validator = validator_path.resolve(strict=True)
        resolved_schema = schema_path.resolve(strict=True)
        for label, source in (
            ("validator", resolved_validator),
            ("schema", resolved_schema),
        ):
            if not source.is_file() or not source.is_relative_to(root):
                raise ValueError(
                    f"report {label} must be a regular file inside the repository"
                )

        validator_source = _read_document(resolved_validator, "validator").decode("utf-8")
        schema_document = _read_document(resolved_schema, "schema")
        report_document = _read_document(report_path, "document")

        with tempfile.TemporaryDirectory(prefix="ironcrew-eval-schema-") as temporary:
            isolated_validator = Path(temporary) / "validate-report.lua"
            isolated_validator.write_text(validator_source, encoding="utf-8")
            module_directory = isolated_validator.parent / "_lib"
            module_directory.mkdir()
            payload = {
                "schema_chunks": _write_document_modules(
                    module_directory, "schema", schema_document
                ),
                "report_chunks": _write_document_modules(
                    module_directory, "report", report_document
                ),
            }
            completed = subprocess.run(
                [
                    str(binary),
                    "run",
                    str(isolated_validator),
                    "--input",
                    json.dumps(payload, separators=(",", ":")),
                ],
                cwd=isolated_validator.parent,
                env=environment,
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        return f"could not execute report schema validator: {error}"
    if completed.returncode != 0:
        detail = redact_error(completed.stderr)
        return detail or f"report schema validator exited with status {completed.returncode}"
    return None
