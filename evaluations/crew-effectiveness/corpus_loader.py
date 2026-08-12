"""Load the versioned crew-effectiveness corpus without prompt metadata leakage."""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable


MANIFEST_SCHEMA = "ironcrew.crew-eval-domain-pack.v1"
BASE_PACK_ID = "synthetic-core-v1"
MAX_MANIFESTS = 32
MAX_JSONL_BYTES = 16 * 1024 * 1024
MAX_RECORDS = 10_000
SHA256_PATTERN = re.compile(r"[a-f0-9]{64}")
SLUG_PATTERN = re.compile(r"[a-z0-9][a-z0-9-]{0,63}")
DATE_PATTERN = re.compile(r"\d{4}-\d{2}-\d{2}")

DatasetValidator = Callable[
    [list[dict[str, Any]], list[dict[str, Any]]], dict[str, dict[str, Any]]
]


@dataclass(frozen=True)
class LoadedCorpus:
    cases: list[dict[str, Any]]
    oracle_records: list[dict[str, Any]]
    oracle_by_id: dict[str, dict[str, Any]]
    case_pack_ids: dict[str, str]
    receipt: dict[str, Any]


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise ValueError(f"could not hash corpus file {path}: {error}") from error
    return digest.hexdigest()


def _load_jsonl(path: Path) -> list[dict[str, Any]]:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"corpus file must be a regular non-symlink file: {path}")
    if path.stat().st_size > MAX_JSONL_BYTES:
        raise ValueError(f"corpus file exceeds {MAX_JSONL_BYTES} bytes: {path}")
    records: list[dict[str, Any]] = []
    try:
        with path.open("r", encoding="utf-8") as source:
            for line_number, raw_line in enumerate(source, 1):
                line = raw_line.strip()
                if not line:
                    continue
                if len(records) >= MAX_RECORDS:
                    raise ValueError(f"corpus file exceeds {MAX_RECORDS} records: {path}")
                try:
                    record = json.loads(line)
                except json.JSONDecodeError as error:
                    raise ValueError(f"{path}:{line_number}: invalid JSON: {error}") from error
                if not isinstance(record, dict):
                    raise ValueError(f"{path}:{line_number}: record must be an object")
                records.append(record)
    except OSError as error:
        raise ValueError(f"could not read corpus file {path}: {error}") from error
    if not records:
        raise ValueError(f"corpus file has no records: {path}")
    return records


def _expect_keys(value: Any, keys: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        raise ValueError(f"{label} must contain exactly {sorted(keys)}")
    return value


def _bounded_text(value: Any, label: str, maximum: int = 2_000) -> str:
    if not isinstance(value, str) or not value.strip() or len(value) > maximum:
        raise ValueError(f"{label} must be a non-empty string of at most {maximum} characters")
    return value


def _load_manifest(path: Path) -> tuple[dict[str, Any], Path, Path]:
    path = path.resolve(strict=True)
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"domain-pack manifest must be a regular file: {path}")
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"invalid domain-pack manifest {path}: {error}") from error
    manifest = _expect_keys(
        manifest,
        {
            "schema_version",
            "pack_id",
            "pack_version",
            "domain",
            "intended_use",
            "derivation",
            "oracle_method",
            "independent_review",
            "case_count",
            "files",
        },
        f"manifest {path}",
    )
    if manifest["schema_version"] != MANIFEST_SCHEMA:
        raise ValueError(f"manifest {path}: unsupported schema_version")
    if not isinstance(manifest["pack_id"], str) or not SLUG_PATTERN.fullmatch(
        manifest["pack_id"]
    ):
        raise ValueError(f"manifest {path}: invalid pack_id")
    if isinstance(manifest["pack_version"], bool) or not isinstance(
        manifest["pack_version"], int
    ) or not 1 <= manifest["pack_version"] <= 1_000_000:
        raise ValueError(f"manifest {path}: invalid pack_version")
    if isinstance(manifest["case_count"], bool) or not isinstance(
        manifest["case_count"], int
    ) or not 1 <= manifest["case_count"] <= MAX_RECORDS:
        raise ValueError(f"manifest {path}: invalid case_count")
    for field in ("domain", "intended_use", "derivation", "oracle_method"):
        _bounded_text(manifest[field], f"manifest {path} {field}")
    review = _expect_keys(
        manifest["independent_review"],
        {"reviewed_on", "scope", "result"},
        f"manifest {path} independent_review",
    )
    if not isinstance(review["reviewed_on"], str) or not DATE_PATTERN.fullmatch(
        review["reviewed_on"]
    ):
        raise ValueError(f"manifest {path}: invalid independent review date")
    _bounded_text(review["scope"], f"manifest {path} independent review scope")
    if review["result"] != "passed":
        raise ValueError(f"manifest {path}: independent review must have passed")

    files = _expect_keys(manifest["files"], {"cases", "oracle"}, f"manifest {path} files")
    resolved: dict[str, Path] = {}
    expected_names = {"cases": "cases.v1.jsonl", "oracle": "oracle.v1.jsonl"}
    for name, expected_name in expected_names.items():
        entry = _expect_keys(files[name], {"path", "sha256"}, f"manifest {path} {name}")
        if entry["path"] != expected_name:
            raise ValueError(f"manifest {path}: {name}.path must be {expected_name}")
        if not isinstance(entry["sha256"], str) or not SHA256_PATTERN.fullmatch(entry["sha256"]):
            raise ValueError(f"manifest {path}: invalid {name} SHA-256")
        candidate = path.parent / expected_name
        if candidate.is_symlink() or not candidate.is_file():
            raise ValueError(f"manifest {path}: {name} must be a regular non-symlink file")
        if _sha256_file(candidate) != entry["sha256"]:
            raise ValueError(f"manifest {path}: {name} SHA-256 mismatch")
        resolved[name] = candidate
    return manifest, resolved["cases"], resolved["oracle"]


def _pack_receipt(
    *, pack_id: str, pack_version: int, cases_path: Path, oracle_path: Path,
    manifest_path: Path | None = None, manifest: dict[str, Any] | None = None,
) -> dict[str, Any]:
    return {
        "pack_id": pack_id,
        "pack_version": pack_version,
        "manifest_path": str(manifest_path) if manifest_path else None,
        "manifest_sha256": _sha256_file(manifest_path) if manifest_path else None,
        "cases_path": str(cases_path),
        "cases_sha256": _sha256_file(cases_path),
        "oracle_path": str(oracle_path),
        "oracle_sha256": _sha256_file(oracle_path),
        "case_count": manifest["case_count"] if manifest else None,
        "metadata": (
            {key: manifest[key] for key in (
                "domain", "intended_use", "derivation", "oracle_method", "independent_review"
            )}
            if manifest else None
        ),
    }


def load_corpus(
    *, base_cases_path: Path, base_oracle_path: Path,
    manifest_paths: list[Path], validate_dataset: DatasetValidator,
) -> LoadedCorpus:
    """Load base plus versioned packs while keeping pack IDs outside case prompts."""
    if len(manifest_paths) > MAX_MANIFESTS:
        raise ValueError(f"at most {MAX_MANIFESTS} domain-pack manifests are allowed")
    base_cases_path = base_cases_path.resolve(strict=True)
    base_oracle_path = base_oracle_path.resolve(strict=True)
    cases = _load_jsonl(base_cases_path)
    oracles = _load_jsonl(base_oracle_path)
    validate_dataset(cases, oracles)
    receipts = [_pack_receipt(
        pack_id=BASE_PACK_ID, pack_version=1,
        cases_path=base_cases_path, oracle_path=base_oracle_path,
    )]
    case_pack_ids = {case["case_id"]: BASE_PACK_ID for case in cases}
    seen_pack_ids = {BASE_PACK_ID}

    loaded_manifests = sorted(
        (_load_manifest(path) for path in manifest_paths), key=lambda item: item[0]["pack_id"]
    )
    for manifest, cases_path, oracle_path in loaded_manifests:
        pack_id = manifest["pack_id"]
        if pack_id in seen_pack_ids:
            raise ValueError(f"duplicate corpus pack_id {pack_id}")
        pack_cases = _load_jsonl(cases_path)
        pack_oracles = _load_jsonl(oracle_path)
        validate_dataset(pack_cases, pack_oracles)
        if len(pack_cases) != manifest["case_count"]:
            raise ValueError(f"pack {pack_id}: case_count does not match cases file")
        for case in pack_cases:
            case_id = case["case_id"]
            if case_id in case_pack_ids:
                raise ValueError(f"duplicate corpus case_id {case_id}")
            case_pack_ids[case_id] = pack_id
        seen_pack_ids.add(pack_id)
        cases.extend(pack_cases)
        oracles.extend(pack_oracles)
        receipts.append(_pack_receipt(
            pack_id=pack_id, pack_version=manifest["pack_version"],
            cases_path=cases_path, oracle_path=oracle_path,
            manifest_path=cases_path.parent / "manifest.v1.json", manifest=manifest,
        ))

    oracle_by_id = validate_dataset(cases, oracles)
    aggregate_material = [
        {key: receipt[key] for key in (
            "pack_id", "pack_version", "manifest_sha256", "cases_sha256", "oracle_sha256"
        )}
        for receipt in receipts
    ]
    aggregate_sha256 = hashlib.sha256(
        json.dumps(aggregate_material, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()
    for receipt in receipts:
        if receipt["case_count"] is None:
            receipt["case_count"] = sum(
                pack_id == receipt["pack_id"] for pack_id in case_pack_ids.values()
            )
    return LoadedCorpus(
        cases=cases,
        oracle_records=oracles,
        oracle_by_id=oracle_by_id,
        case_pack_ids=case_pack_ids,
        receipt={"case_count": len(cases), "aggregate_sha256": aggregate_sha256, "packs": receipts},
    )
