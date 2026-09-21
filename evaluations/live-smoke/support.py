"""Reuse the existing secret/provenance/receipt contracts, without live imports."""
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / "evaluations/crew-effectiveness"))
sys.path.insert(1, str(ROOT / "evaluations/replica-soak"))

from live_provider_environment import live_provider_environment, redaction_canaries  # noqa: E402
from source_provenance import worktree_provenance, require_unchanged_provenance  # noqa: E402
from source_provenance import safe_binary_path  # noqa: E402
from soak_runtime_directory import ExternalRuntimeDirectory  # noqa: E402
from usage_receipts import costing_counts, decimal_count  # noqa: E402


class SmokeError(RuntimeError):
    """Only fixed, secret-free codes escape into reports or console output."""
    def __init__(self, code: str):
        self.code = code
        super().__init__(code)


def classify_failure(raw: bytes) -> str:
    text = raw.decode("utf-8", "replace").lower()
    if any(s in text for s in ("401", "403", "invalid_api_key", "insufficient_quota")):
        return "provider_credentials_or_quota"
    if any(s in text for s in ("429", "500", "502", "503", "504", "connection reset")):
        return "provider_unavailable"
    if "run token budget exhausted" in text:
        return "budget_exhausted"
    if "input token counting failed" in text:
        return "admission_preflight_failed"  # Cause is intentionally not guessed.
    if any(s in text for s in ("400", "model_not_found", "unsupported")):
        return "provider_contract"
    return "runtime_contract"


def reject_secrets(raw: bytes, canaries: tuple[str, ...]) -> None:
    if any(value.encode() in raw for value in canaries if value):
        raise SmokeError("secret_output_rejected")
