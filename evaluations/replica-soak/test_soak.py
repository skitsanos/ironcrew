import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


sys.path.insert(0, str(Path(__file__).resolve().parent))
import soak  # noqa: E402


class ReplicaSoakUnitTests(unittest.TestCase):
    def test_percentiles_use_nearest_rank(self) -> None:
        values = [1.0, 2.0, 3.0, 4.0, 100.0]
        self.assertEqual(soak.percentile(values, 0.50), 3.0)
        self.assertEqual(soak.percentile(values, 0.95), 100.0)
        self.assertIsNone(soak.percentile([], 0.95))

    def test_database_label_and_errors_remove_credentials(self) -> None:
        dsn = "postgres://private-user:private-pass@db.example:5433/crew"
        self.assertEqual(
            soak.safe_database_label(dsn), "postgres://db.example:5433/crew"
        )
        error = soak.sanitize_error(f"failed {dsn} private-pass", (dsn, "private-pass"))
        self.assertNotIn("private-user", error)
        self.assertNotIn("private-pass", error)
        self.assertIn("<redacted>", error)

    def test_sse_event_payload_unwraps_tagged_crew_event(self) -> None:
        tagged = {
            "event": "run_complete",
            "data": {
                "event": "run_complete",
                "data": {"status": "success", "total_tokens": 0},
            },
        }
        bare = {"event": "run_complete", "data": {"status": "success"}}
        self.assertEqual(
            soak.sse_event_payload(tagged),
            {"status": "success", "total_tokens": 0},
        )
        self.assertEqual(soak.sse_event_payload(bare), {"status": "success"})

    def test_prefix_validation_and_cleanup_are_exact(self) -> None:
        self.assertEqual(soak.validate_prefix("soak_a1_"), "soak_a1_")
        for invalid in ("", "UPPER_", "../bad", "x", "1starts_wrong"):
            with self.assertRaises(ValueError):
                soak.validate_prefix(invalid)
        cleanup = soak.cleanup_sql("soak_a1_")
        self.assertIn("DROP TABLE IF EXISTS soak_a1_run_events;", cleanup)
        self.assertIn("DROP TABLE IF EXISTS soak_a1_runs;", cleanup)
        self.assertNotIn("DROP SCHEMA", cleanup)
        self.assertNotIn("CASCADE", cleanup)

    def test_docker_pid_is_unavailable_outside_linux(self) -> None:
        with patch.object(soak.platform, "system", return_value="Darwin"):
            self.assertIsNone(soak.docker_pid("container-in-linux-vm"))

    def test_snapshot_sql_is_prefix_scoped_and_collects_required_signals(self) -> None:
        sql = soak.postgres_snapshot_sql("soak_a1_")
        for token in (
            "soak_a1_run_events",
            "accounted_bytes",
            "pg_indexes_size",
            "n_dead_tup",
            "autovacuum_count",
            "pg_current_wal_lsn",
            "pg_stat_database",
        ):
            self.assertIn(token, sql)
        self.assertNotIn("postgres://", sql)

        statements_sql = soak.pg_stat_statements_sql("soak_a1_")
        self.assertIn("%soak\\_a1\\_%", statements_sql)
        self.assertIn("query NOT ILIKE '%pg_stat_statements%'", statements_sql)
        self.assertIn("query NOT ILIKE '%pg_stat_database%'", statements_sql)

    def test_postgres_json_preserves_multiline_psql_output(self) -> None:
        client = soak.PostgresClient(
            "postgres://user:password@127.0.0.1/database", None, "postgres-test"
        )
        output = '{"items": [\n{"name": "first"},\n{"name": "second"}\n]}\n'
        with patch.object(
            soak.subprocess, "run", return_value=SimpleNamespace(stdout=output)
        ):
            self.assertEqual(
                client.json("SELECT json_agg(record);"),
                {"items": [{"name": "first"}, {"name": "second"}]},
            )

    def test_postgres_delta_separates_relation_and_database_metrics(self) -> None:
        before = {
            "wal_bytes_from_origin": 100,
            "database_size_bytes": 1000,
            "tables": [
                {
                    "relname": "soak_a1_run_events",
                    "exact_rows": 1,
                    "estimated_live_rows": 1,
                    "estimated_dead_rows": 0,
                    "seq_scan": 0,
                    "idx_scan": 1,
                    "tuples_inserted": 1,
                    "tuples_updated": 0,
                    "tuples_deleted": 0,
                    "autovacuum_count": 0,
                    "autoanalyze_count": 0,
                    "heap_bytes": 8192,
                    "index_bytes": 8192,
                    "total_bytes": 16384,
                }
            ],
            "indexes": [],
            "database_activity": {"deadlocks": 0, "xact_commit": 5, "stats_reset": "t0"},
            "journal_accounting": {
                "retained_events": 1,
                "retained_bytes": 100,
                "actual_rows": 1,
                "payload_bytes": 80,
                "accounted_bytes": 256,
            },
            "human_input_rows": {"total": 0, "pending": 0, "answered": 0},
        }
        after = json.loads(json.dumps(before))
        after["wal_bytes_from_origin"] = 140
        after["database_size_bytes"] = 1200
        after["tables"][0]["exact_rows"] = 3
        after["tables"][0]["estimated_dead_rows"] = 1
        after["database_activity"]["xact_commit"] = 9
        after["journal_accounting"]["accounted_bytes"] = 768
        after["human_input_rows"] = {"total": 1, "pending": 0, "answered": 1}
        delta = soak.postgres_delta(before, after)
        self.assertEqual(delta["wal_bytes"], 40)
        self.assertEqual(delta["tables"][0]["exact_rows"], 2)
        self.assertEqual(delta["tables"][0]["estimated_dead_rows"], 1)
        self.assertEqual(delta["database_activity"]["xact_commit"], 4)
        self.assertEqual(delta["journal_accounting"]["accounted_bytes"], 512)
        self.assertEqual(delta["human_input_rows"]["total"], 1)
        self.assertEqual(delta["human_input_rows"]["answered"], 1)
        self.assertFalse(delta["stats_reset_changed"])

    def test_operation_metrics_are_bounded_and_machine_readable(self) -> None:
        metrics = soak.OperationMetrics(max_samples=3)
        for latency in range(10):
            metrics.record("health", float(latency), 200, True, response_bytes=2)
        report = metrics.report()["health"]
        self.assertEqual(report["count"], 10)
        self.assertEqual(report["latency_ms"]["sampled_count"], 3)
        self.assertTrue(report["latency_ms"]["percentiles_approximate"])
        self.assertEqual(report["response_bytes"], 20)
        json.dumps(report)

    def test_pass_criteria_use_host_rss_comparator_without_enforcement_claim(self) -> None:
        metrics = soak.OperationMetrics()
        metrics.record("health_liveness_probe", 1.0, 200, True)
        metrics.record("health_readiness_probe", 1.0, 200, True)
        report = {
            "workload": {"attempted_runs": 2, "failed_runs": 0},
            "postgres": {
                "delta": {
                    "database_activity": {"deadlocks": 0},
                    "stats_reset_changed": False,
                }
            },
            "resources": {
                "replicas": {
                    "a": {"sampled_peak_rss_bytes": 100},
                    "b": {"sampled_peak_rss_bytes": 200},
                }
            },
        }
        args = SimpleNamespace(mode="target", memory_comparator_mib=1)
        criteria = soak.build_pass_criteria(
            report, metrics, args, SimpleNamespace(processes={})
        )
        self.assertTrue(criteria["overall_passed"])
        self.assertFalse(
            criteria["host_process_rss_comparator"]["comparator_enforced_by_runner"]
        )


if __name__ == "__main__":
    unittest.main()
