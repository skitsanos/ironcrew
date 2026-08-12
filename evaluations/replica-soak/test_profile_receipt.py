import copy
import io
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


sys.path.insert(0, str(Path(__file__).resolve().parent))
from profile_receipt import finalize_report  # noqa: E402
import profiles  # noqa: E402


def source(manifest: str = "manifest-a") -> dict[str, object]:
    return {
        "revision": "abc123",
        "dirty": True,
        "changed_path_count": 1,
        "changed_paths": [
            {"path": "evaluations/replica-soak/profiles.py", "state": "file"}
        ],
        "worktree_manifest_sha256": manifest,
        "manifest_encoding": "test",
        "binary_sha256": "binary-a",
    }


def passing_report() -> dict[str, object]:
    return {
        "status": "running",
        "profiles": [
            {"id": "provider_tool", "status": "passed", "evidence": {}},
            {"id": "large_result", "status": "passed", "evidence": {}},
            {
                "id": "owner_local_conversation_boundary",
                "status": "passed",
                "evidence": {
                    "conversation_id": "conversation",
                    "identity": {
                        "conversation_id": "conversation",
                        "incarnation_id": "11111111-1111-4111-8111-111111111111",
                        "definition_fingerprint": f"sha256:{'a' * 64}",
                    },
                    "revisions": [1, 2, 3, 3],
                    "in_flight_takeover_proven": False,
                },
            },
        ],
        "mock_provider": {
            "counts": {
                "provider_tool_provider_calls": 2,
                "provider_tool_tool_effects": 1,
                "large_result_provider_calls": 1,
                "conversation_provider_calls": 2,
                "invalid_requests": 0,
            }
        },
        "topology": {"instance_ids": ["ic018-profile-a", "ic018-profile-b"]},
        "shutdown": {
            "a": {"exit_code": 0, "forced_kill": False},
            "b": {"exit_code": 0, "forced_kill": False},
        },
        "cleanup": {
            "performed": True,
            "remaining_objects": 0,
            "zero_verified": True,
            "error": None,
        },
        "runtime_logs": {"raw_content_in_report": False},
    }


class ProfileReceiptTests(unittest.TestCase):
    def test_final_verdict_passes_only_complete_post_cleanup_receipt(self) -> None:
        report = passing_report()
        beginning = source()
        self.assertEqual(finalize_report(report, beginning, copy.deepcopy(beginning)), 0)
        self.assertEqual(report["status"], "passed")
        self.assertTrue(report["pass_criteria"]["overall_passed"])
        self.assertTrue(report["pass_criteria"]["cleanup_performed"])
        self.assertTrue(report["pass_criteria"]["zero_prefix_objects"])
        self.assertTrue(report["source"]["unchanged_during_run"])

    def test_source_drift_fails_an_otherwise_passing_receipt(self) -> None:
        report = passing_report()
        self.assertEqual(finalize_report(report, source(), source("manifest-b")), 1)
        self.assertEqual(report["status"], "failed")
        self.assertFalse(report["pass_criteria"]["source_provenance_unchanged"])
        self.assertFalse(report["pass_criteria"]["overall_passed"])

    def test_cleanup_and_process_failures_are_independent_final_gates(self) -> None:
        report = passing_report()
        report["shutdown"]["a"] = {"exit_code": 1, "forced_kill": True}
        report["cleanup"] = {
            "performed": False,
            "remaining_objects": 1,
            "zero_verified": False,
            "error": "bounded cleanup failure",
        }
        self.assertEqual(finalize_report(report, source(), source()), 1)
        criteria = report["pass_criteria"]
        self.assertFalse(criteria["process_exit_zero"])
        self.assertFalse(criteria["no_forced_kill"])
        self.assertFalse(criteria["cleanup_performed"])
        self.assertFalse(criteria["zero_prefix_objects"])

    def test_missing_cleanup_cannot_be_finalised_as_passed(self) -> None:
        report = passing_report()
        report.pop("cleanup")
        self.assertEqual(finalize_report(report, source(), source()), 1)
        self.assertFalse(report["pass_criteria"]["cleanup_performed"])
        self.assertFalse(report["pass_criteria"]["zero_prefix_objects"])

    def test_runner_finalises_observer_initialisation_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "ironcrew"
            binary.write_bytes(b"profile-binary")
            report_path = root / "receipt.json"
            args = SimpleNamespace(
                database_url="postgres://db/profile",
                psql_command=None,
                postgres_container=None,
                table_prefix="ic018p_test_",
                report=report_path,
                binary=binary,
                request_timeout=1.0,
                poll_interval=0.01,
                startup_timeout=1.0,
            )
            with (
                patch.object(profiles, "capture_source", side_effect=[source(), source()]),
                patch.object(
                    profiles.soak,
                    "PostgresClient",
                    side_effect=RuntimeError("observer unavailable"),
                ),
                redirect_stdout(io.StringIO()),
            ):
                report, exit_code = profiles.execute(args)
        self.assertEqual(exit_code, 1)
        self.assertEqual(report["status"], "failed")
        self.assertFalse(report["cleanup"]["performed"])
        self.assertFalse(report["pass_criteria"]["execution_completed"])
        self.assertFalse(report["pass_criteria"]["overall_passed"])


if __name__ == "__main__":
    unittest.main()
