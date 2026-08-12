import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import soak  # noqa: E402
from soak_contract import IntervalRecorder, load_contract, validate_contract  # noqa: E402
from soak_contract_evaluation import evaluate_contract  # noqa: E402
from soak_retention_probe import (  # noqa: E402
    delayed_replay_probe,
    post_cleanup_inventory_sql,
    retention_anchor_sql,
)


def contract_fixture() -> dict:
    return {
        "schema_version": "ironcrew.replica-soak-contract.v1",
        "observation_interval_seconds": 30,
        "tail_intervals": 2,
        "journal": {
            "max_events_per_run": 20,
            "max_bytes_per_run": 4 * 1024 * 1024,
            "max_event_bytes": 256 * 1024,
            "retention_seconds": 60,
            "max_total_events": 100,
            "max_total_bytes": 4 * 1024 * 1024,
            "page_max_events": 20,
            "page_max_bytes": 512 * 1024,
            "poll_interval_ms": 500,
            "read_timeout_ms": 2000,
            "write_timeout_ms": 1500,
            "prune_batch": 8,
        },
        "ceilings": {
            "retained_rows": 20,
            "retained_bytes": 20_000,
            "expired_physical_rows": 8,
            "post_prune_growth_rows": 4,
            "post_prune_growth_bytes": 4_000,
            "readiness_failures": 0,
            "liveness_failures": 0,
            "rss_peak_bytes_per_replica": 1_000,
            "tail_rss_growth_bytes_per_replica": 100,
            "tail_run_events_relation_growth_bytes": 1_000,
            "prefix_relation_bytes": 10_000,
            "prefix_relation_bytes_per_success": 100,
            "wal_bytes": 1_000,
            "wal_bytes_per_success": 100,
            "tail_latency_ms": {
                "run_start": {"p95": 10, "p99": 20, "max": 30}
            },
        },
        "requirements": {
            "minimum_intervals": 4,
            "minimum_post_boundary_intervals": 2,
            "minimum_prune_intervals": 1,
            "minimum_journal_gap_events": 1,
            "allowed_journal_gap_reasons": ["retention"],
            "require_cursor_expired": True,
            "minimum_workload_seconds": 60,
            "require_duration_stop": True,
        },
    }


def observation(
    label: str,
    elapsed: float,
    retained: int,
    dropped: int,
    deleted: int,
    relation: int,
    wal: int,
    *,
    expired: int = 0,
    latency: float = 5,
) -> dict:
    return {
        "label": label,
        "elapsed_seconds": elapsed,
        "postgres": {
            "wal_bytes_from_origin": wal,
            "journal_accounting": {
                "actual_rows": retained,
                "retained_events": retained,
                "expired_physical_rows": expired,
                "accounted_bytes": retained * 1_000,
                "retained_bytes": retained * 1_000,
            },
            "retention_state": {
                "gap_runs": 1 if dropped else 0,
                "dropped_sequences": dropped,
            },
            "tables": [
                {
                    "relname": "soak_a_run_events",
                    "tuples_deleted": deleted,
                    "total_bytes": relation,
                },
                {"relname": "soak_a_runs", "total_bytes": 1_000},
            ],
        },
        "operations": {
            "run_start": {
                "count": 2,
                "errors": 0,
                "latency_ms": {"p95": latency, "p99": latency, "max": latency},
            },
            "health_readiness_probe": {"count": 2, "errors": 0},
            "health_liveness_probe": {"count": 2, "errors": 0},
        },
    }


def passing_evidence() -> tuple:
    contract = validate_contract(contract_fixture())
    observations = [
        observation("baseline", 0, 8, 0, 0, 2_000, 10_000),
        observation("interval", 30, 10, 0, 0, 2_100, 10_100),
        observation("interval", 60, 8, 4, 4, 2_200, 10_200),
        observation("interval", 90, 9, 5, 5, 2_300, 10_300),
        observation("final", 120, 10, 6, 6, 2_400, 10_400),
    ]
    resources = {
        "sampler_thread_stopped": True,
        "replicas": {
            "a": {
                "sampled_peak_rss_bytes": 800,
                "timeline": [
                    {"elapsed_s": 60, "rss_bytes": 700},
                    {"elapsed_s": 120, "rss_bytes": 750},
                ],
            },
            "b": {
                "sampled_peak_rss_bytes": 900,
                "timeline": [
                    {"elapsed_s": 60, "rss_bytes": 800},
                    {"elapsed_s": 120, "rss_bytes": 850},
                ],
            },
        }
    }
    replay = {
        "cursor_probe": {"status": 409, "code": "cursor_expired"},
        "gap_probe": {
            "count": 1,
            "reasons": ["retention"],
            "terminal": {
                "id": None,
                "status": "success",
                "journal_complete": False,
                "synthesized_from_run_record": True,
            },
        },
        "anchor": {
            "physical_rows": 0,
            "retained_events": 0,
            "dropped_through": 4,
            "eviction_reason": "retention",
        },
    }
    workload = {
        "successful_runs": 10,
        "attempted_runs": 10,
        "requested_run_cap": 100,
        "elapsed_seconds": 60,
        "stop_reason": "duration",
    }
    lifecycle = {
        "replica_shutdown": {
            "a": {"exit_code": 0, "forced_kill": False},
            "b": {"exit_code": 0, "forced_kill": False},
        },
        "cleanup": {
            "database_cleanup_requested": True,
            "database_cleanup_performed": True,
            "database_cleanup_error": None,
        },
        "post_cleanup_inventory": {"relations": 0, "functions": 0},
        "source_at_start": {"worktree_manifest_sha256": "same"},
        "source_at_finish": {"worktree_manifest_sha256": "same"},
    }
    return contract, observations, resources, replay, workload, lifecycle


def evaluate(parts: tuple) -> dict:
    contract, observations, resources, replay, workload, lifecycle = parts
    return evaluate_contract(
        contract,
        observations,
        resources,
        replay,
        contract["journal"],
        workload,
        lifecycle,
        True,
    )


class SoakContractTests(unittest.TestCase):
    def test_primary_contract_is_valid_and_hashed_from_exact_bytes(self) -> None:
        path = HERE / "contracts" / "provider-free-retention.json"
        contract, digest = load_contract(path)
        self.assertEqual(contract["requirements"]["minimum_workload_seconds"], 1800)
        self.assertEqual(contract["journal"]["prune_batch"], 128)
        self.assertEqual(len(digest), 64)

    def test_contract_validation_rejects_unknown_fractional_and_incoherent_values(self) -> None:
        cases = []
        unknown = contract_fixture()
        unknown["extra"] = True
        cases.append(unknown)
        fractional = contract_fixture()
        fractional["tail_intervals"] = 1.5
        cases.append(fractional)
        latency = contract_fixture()
        latency["ceilings"]["tail_latency_ms"]["run_start"] = {
            "p95": 30,
            "p99": 20,
            "max": 40,
        }
        cases.append(latency)
        prune = contract_fixture()
        prune["journal"]["prune_batch"] = 101
        cases.append(prune)
        for value in cases:
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    validate_contract(value)

    def test_contract_loader_rejects_empty_and_oversized_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "contract.json"
            path.write_bytes(b"")
            with self.assertRaises(ValueError):
                load_contract(path)
            path.write_bytes(b" " * (64 * 1024 + 1))
            with self.assertRaises(ValueError):
                load_contract(path)

    def test_interval_recorder_keeps_baseline_and_final_and_drains_metrics(self) -> None:
        operations = iter(({"setup": {}}, {"final": {}}))
        recorder = IntervalRecorder(lambda: {"journal_accounting": {}}, lambda: next(operations), 10)
        recorder.start({"journal_accounting": {"actual_rows": 0}})
        rows = recorder.stop()
        self.assertEqual([row["label"] for row in rows], ["baseline", "final"])
        self.assertEqual(rows[0]["operations"], {"setup": {}})
        self.assertEqual(rows[1]["operations"], {"final": {}})

    def test_every_contract_dimension_passes_with_complete_evidence(self) -> None:
        criteria = evaluate(passing_evidence())
        self.assertTrue(criteria["overall_passed"])
        for name, item in criteria.items():
            if name != "overall_passed":
                self.assertTrue(item["passed"], name)

    def test_contract_fails_closed_across_boundary_tail_replay_and_lifecycle(self) -> None:
        mutations = {
            "retained_rows": lambda p: p[1][-1]["postgres"]["journal_accounting"].update(actual_rows=21),
            "retained_bytes": lambda p: p[1][-1]["postgres"]["journal_accounting"].update(accounted_bytes=21_000),
            "expired_physical_rows": lambda p: p[1][-1]["postgres"]["journal_accounting"].update(expired_physical_rows=9),
            "physical_prune_progress": lambda p: [
                row["postgres"]["tables"][0].update(tuples_deleted=0) for row in p[1]
            ],
            "readiness_failures": lambda p: p[1][-1]["operations"]["health_readiness_probe"].update(errors=1),
            "liveness_failures": lambda p: p[1][-1]["operations"]["health_liveness_probe"].update(errors=1),
            "rss_peak": lambda p: p[2]["replicas"]["a"].update(sampled_peak_rss_bytes=1_001),
            "tail_rss_growth": lambda p: p[2]["replicas"]["a"]["timeline"][-1].update(rss_bytes=900),
            "resource_sampler_stopped": lambda p: p[2].update(
                sampler_thread_stopped=False
            ),
            "tail_latency": lambda p: p[1][-2]["operations"]["run_start"]["latency_ms"].update(p99=21),
            "tail_cadence": lambda p: p[1][-2].update(elapsed_seconds=61),
            "explicit_replay_gap": lambda p: p[3]["gap_probe"].update(count=0, reasons=[]),
            "expired_cursor": lambda p: p[3]["cursor_probe"].update(status=200, code=None),
            "replay_anchor": lambda p: p[3]["anchor"].update(physical_rows=1),
            "workload_duration": lambda p: p[4].update(elapsed_seconds=59),
            "cleanup": lambda p: p[5]["cleanup"].update(database_cleanup_performed=False),
            "graceful_shutdown": lambda p: p[5]["replica_shutdown"]["a"].update(forced_kill=True),
            "post_cleanup_inventory": lambda p: p[5]["post_cleanup_inventory"].update(relations=1),
            "source_stable": lambda p: p[5].update(
                source_at_finish={"worktree_manifest_sha256": "changed"}
            ),
        }
        for criterion_name, mutate in mutations.items():
            with self.subTest(criterion=criterion_name):
                parts = copy.deepcopy(passing_evidence())
                mutate(parts)
                criteria = evaluate(parts)
                self.assertFalse(criteria[criterion_name]["passed"])
                self.assertFalse(criteria["overall_passed"])

    def test_contract_fails_on_journal_mismatch_and_missing_base_pass(self) -> None:
        parts = passing_evidence()
        args = list(parts)
        criteria = evaluate_contract(
            args[0],
            args[1],
            args[2],
            args[3],
            {**args[0]["journal"], "prune_batch": 7},
            args[4],
            args[5],
            False,
        )
        self.assertFalse(criteria["journal_configuration"]["passed"])
        self.assertFalse(criteria["graceful_shutdown"]["passed"])

    def test_missing_observation_fails_closed(self) -> None:
        parts = list(passing_evidence())
        del parts[1][2]["postgres"]
        criteria = evaluate(tuple(parts))
        self.assertFalse(criteria["observation_intervals"]["passed"])
        self.assertFalse(criteria["overall_passed"])

    def test_literal_1800_second_duration_and_duration_stop_are_both_required(self) -> None:
        parts = list(passing_evidence())
        parts[0]["requirements"]["minimum_workload_seconds"] = 1800.0
        parts[4]["elapsed_seconds"] = 1799.999
        self.assertFalse(evaluate(tuple(parts))["workload_duration"]["passed"])
        parts[4]["elapsed_seconds"] = 1800.0
        self.assertTrue(evaluate(tuple(parts))["workload_duration"]["passed"])
        parts[4]["stop_reason"] = "run_cap"
        self.assertFalse(evaluate(tuple(parts))["workload_duration"]["passed"])

    def test_snapshot_and_cleanup_sql_use_exact_required_signals(self) -> None:
        snapshot = soak.postgres_snapshot_sql("soak_a1_")
        self.assertIn("expired_physical_rows", snapshot)
        self.assertIn("eviction_reason = 'retention'", snapshot)
        self.assertIn("dropped_sequences", snapshot)
        inventory = post_cleanup_inventory_sql("soak_a1_")
        self.assertIn("left(class.relname, length('soak_a1_')) = 'soak_a1_'", inventory)
        self.assertNotIn("LIKE", inventory)
        anchor = retention_anchor_sql("soak_a1_", "123e4567-e89b-12d3-a456-426614174000")
        self.assertIn("WHERE run_id = '123e4567-e89b-12d3-a456-426614174000'", anchor)

    def test_delayed_replay_probe_records_expiry_gap_terminal_and_anchor(self) -> None:
        class Client:
            def request(self, *_args, **_kwargs):
                return SimpleNamespace(
                    status=409,
                    body=json.dumps({"error": {"code": "cursor_expired"}}).encode(),
                )

            def sse_until(self, *_args, **_kwargs):
                return {
                    "event": "run_complete",
                    "id": None,
                    "data": {
                        "event": "run_complete",
                        "data": {
                            "status": "success",
                            "journal_complete": False,
                            "synthesized_from_run_record": True,
                        },
                    },
                    "journal_gaps": [
                        {"first_sequence": 1, "last_sequence": 4, "reason": "retention"}
                    ],
                }

        class Postgres:
            def json(self, sql):
                self.sql = sql
                return {
                    "physical_rows": 0,
                    "retained_events": 0,
                    "dropped_through": 4,
                    "eviction_reason": "retention",
                }

        run_id = "123e4567-e89b-12d3-a456-426614174000"
        report = delayed_replay_probe(
            [
                {
                    "index": 0,
                    "success": True,
                    "run_id": run_id,
                    "peer_replica": "b",
                    "_replay_cursor": f"{run_id}:1",
                }
            ],
            ("http://a", "http://b"),
            Client(),
            Postgres(),
            "soak_a1_",
            1024,
        )
        self.assertEqual(report["cursor_probe"], {"status": 409, "code": "cursor_expired"})
        self.assertEqual(report["gap_probe"]["reasons"], ["retention"])
        self.assertIsNone(report["gap_probe"]["terminal"]["id"])
        self.assertEqual(report["anchor"]["physical_rows"], 0)

    def test_operation_metrics_interval_drain_does_not_reset_cumulative_report(self) -> None:
        metrics = soak.OperationMetrics()
        metrics.record("run_start", 2.0, 200, True)
        self.assertEqual(metrics.interval_report()["run_start"]["count"], 1)
        self.assertEqual(metrics.interval_report(), {})
        self.assertEqual(metrics.report()["run_start"]["count"], 1)

    def test_allocator_reports_duration_and_run_cap_without_guessing(self) -> None:
        duration = soak.RunAllocator(10, 0)
        self.assertIsNone(duration.take())
        self.assertEqual(duration.stop_reason(), "duration")
        run_cap = soak.RunAllocator(1, float("inf"))
        self.assertEqual(run_cap.take(), 0)
        self.assertEqual(run_cap.stop_reason(), "run_cap")

    def test_child_environment_applies_all_declared_journal_knobs(self) -> None:
        args = SimpleNamespace(
            database_url="postgres://user:pass@db/test",
            db_pool_size=2,
            max_active_runs=2,
            hitl_poll_ms=1000,
            hitl_pg_reads=2,
            journal_poll_ms=500,
            max_events=200,
            event_replay_max_bytes=4_194_304,
            event_max_bytes=262_144,
            journal_retention_seconds=600,
            journal_max_total_events=100_000,
            journal_max_total_bytes=268_435_456,
            journal_page_max_bytes=524_288,
            journal_read_timeout_ms=2_000,
            journal_write_timeout_ms=1_500,
            journal_prune_batch=128,
            log_level="warn",
        )
        with patch.dict(
            soak.os.environ,
            {"UNRELATED_SECRET": "must-not-cross", "ANTHROPIC_API_KEY": "live-secret"},
        ):
            environment = soak.child_environment(
                args, "replica-a", "token", "soak_a1_", "{}"
            )
        self.assertNotIn("UNRELATED_SECRET", environment)
        self.assertEqual(environment["ANTHROPIC_API_KEY"], "")
        self.assertEqual(environment["IRONCREW_EVENT_JOURNAL_RETENTION_SECS"], "600")
        self.assertEqual(environment["IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS"], "100000")
        self.assertEqual(environment["IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES"], "268435456")
        self.assertEqual(environment["IRONCREW_EVENT_JOURNAL_PRUNE_BATCH"], "128")
        self.assertEqual(environment["IRONCREW_EVENT_REPLAY_MAX_BYTES"], "4194304")
        self.assertEqual(environment["IRONCREW_EVENT_MAX_BYTES"], "262144")
        self.assertEqual(environment["IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES"], "524288")
        self.assertEqual(environment["IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS"], "2000")
        self.assertEqual(environment["IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS"], "1500")

    def test_resource_sampler_records_a_confirmed_thread_stop(self) -> None:
        sampler = soak.ResourceSampler({"a": None, "b": None}, 0.01, 1024)
        sampler.start()
        sampler.stop()
        self.assertTrue(sampler.report()["sampler_thread_stopped"])


if __name__ == "__main__":
    unittest.main()
