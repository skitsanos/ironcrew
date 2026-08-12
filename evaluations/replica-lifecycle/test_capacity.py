from __future__ import annotations

import json
import sys
import threading
import unittest
import urllib.request
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


sys.path.insert(0, str(Path(__file__).resolve().parent))

from capacity_config import (  # noqa: E402
    ACTIVE_RUNS_PER_REPLICA,
    EVENT_PAYLOAD_ENVELOPE_PER_PROCESS,
    REPLAY_BYTES_PER_RUN,
    child_environment,
)
from capacity_assertions import validate_process_metrics  # noqa: E402
from harness_runtime import parse_metric  # noqa: E402
from mock_provider import ProviderFixture  # noqa: E402
from phase_control import wait_quiescent  # noqa: E402
from postgres_observer import safe_database_label, validate_prefix  # noqa: E402
from reporting import sanitize_failure  # noqa: E402


class CapacityContractTests(unittest.TestCase):
    def test_logical_event_envelope_includes_replay_and_durable_queue(self) -> None:
        self.assertEqual(
            EVENT_PAYLOAD_ENVELOPE_PER_PROCESS,
            ACTIVE_RUNS_PER_REPLICA * REPLAY_BYTES_PER_RUN * 2,
        )

    def test_child_environment_overrides_live_provider_configuration(self) -> None:
        environment = child_environment(
            "postgres://user:password@127.0.0.1:55432/test",
            "ic020cap_1234abcd_",
            "replica-1",
            "test-token-with-at-least-32-bytes-value",
            "http://127.0.0.1:32123/v1",
        )
        self.assertEqual(environment["OPENAI_API_KEY"], "ic020-loopback-not-a-secret")
        self.assertEqual(environment["OPENAI_BASE_URL"], "http://127.0.0.1:32123/v1")
        self.assertEqual(environment["IC020_PROVIDER_BASE_URL"], "http://127.0.0.1:32123/v1")
        self.assertEqual(environment["IRONCREW_ENV_ALLOWLIST"], "IC020_PROVIDER_BASE_URL")
        self.assertEqual(environment["IRONCREW_ALLOW_PRIVATE_IPS"], "true")
        self.assertLessEqual(
            int(environment["IRONCREW_EVENT_JOURNAL_PRUNE_BATCH"]),
            int(environment["IRONCREW_EVENT_JOURNAL_MAX_TOTAL_EVENTS"]),
        )

    def test_database_label_and_prefix_do_not_expose_credentials(self) -> None:
        label = safe_database_label("postgres://user:secret@127.0.0.1:55432/capacity")
        self.assertEqual(label, "postgres://127.0.0.1:55432/capacity")
        self.assertEqual(validate_prefix("ic020cap_deadbeef_"), "ic020cap_deadbeef_")
        with self.assertRaises(ValueError):
            validate_prefix("shared_")

    def test_failure_sanitizer_preserves_metric_names(self) -> None:
        database_url = (
            "postgres://ironcrew:ic020-capacity-local-password@127.0.0.1:55433/capacity"
        )
        token = "ic020-token-with-at-least-32-bytes"
        message = (
            f"failed for {database_url}; token={token}; "
            "password=ic020-capacity-local-password; "
            "ironcrew_process_eventbus_instances=4"
        )
        redacted = sanitize_failure(
            message,
            database_url=database_url,
            secret_canaries=(token, "ic020-capacity-local-password"),
        )
        self.assertNotIn(database_url, redacted)
        self.assertNotIn(token, redacted)
        self.assertNotIn("ic020-capacity-local-password", redacted)
        self.assertIn("ironcrew_process_eventbus_instances=4", redacted)

    def test_metric_parser_requires_one_unlabelled_sample(self) -> None:
        self.assertEqual(parse_metric("sample 2\n", "sample"), 2)
        with self.assertRaises(RuntimeError):
            parse_metric("sample{x=\"a\"} 2\n", "sample")

    def test_phase_cleanup_waits_for_eventbus_capacity_to_reach_zero(self) -> None:
        zero = {
            "ironcrew_process_active_runs": 0,
            "ironcrew_process_active_sse_connections": 0,
            "ironcrew_process_active_provider_calls": 0,
            "ironcrew_process_eventbus_instances": 0,
            "ironcrew_process_eventbus_retained_events": 0,
            "ironcrew_process_eventbus_retained_bytes": 0,
            "ironcrew_process_eventbus_retained_events_capacity": 0,
            "ironcrew_process_eventbus_retained_bytes_capacity": 0,
        }
        retained = {**zero, "ironcrew_process_eventbus_instances": 1}
        replicas = SimpleNamespace(bases={"replica-1": "http://127.0.0.1:1"})
        with patch("phase_control.replica_metrics", side_effect=(retained, zero)) as metrics:
            result = wait_quiescent(replicas, "test-token", timeout=0.2)
        self.assertEqual(metrics.call_count, 2)
        self.assertEqual(result["metrics_by_replica"], {"replica-1": zero})

    def test_process_metric_contract_accepts_non_linux_memory_unavailable(self) -> None:
        metrics = {
            "ironcrew_process_active_runs": 2,
            "ironcrew_process_active_runs_limit": 2,
            "ironcrew_process_active_sse_connections": 2,
            "ironcrew_process_active_sse_connections_limit": 2,
            "ironcrew_process_memory_measurement_available": 0,
            "ironcrew_postgres_pool_open_connections": 2,
            "ironcrew_postgres_pool_in_use_connections": 1,
            "ironcrew_postgres_pool_connections_limit": 2,
            "ironcrew_process_active_provider_calls": 2,
            "ironcrew_process_peak_active_provider_calls": 2,
            "ironcrew_process_eventbus_instances": 2,
            "ironcrew_process_eventbus_retained_events": 6,
            "ironcrew_process_eventbus_retained_bytes": 4096,
            "ironcrew_process_eventbus_retained_events_capacity": 64,
            "ironcrew_process_eventbus_retained_bytes_capacity": 512 * 1024,
        }
        validate_process_metrics("replica-1", metrics)
        metrics["ironcrew_process_active_provider_calls"] = 1
        with self.assertRaises(RuntimeError):
            validate_process_metrics("replica-1", metrics)

    def test_loopback_provider_holds_exact_concurrency(self) -> None:
        with ProviderFixture() as provider:
            provider.gate.begin("test", 2)
            results: list[int] = []

            def invoke() -> None:
                payload = json.dumps(
                    {"model": "ic020-loopback", "messages": [{"role": "user", "content": "x"}]}
                ).encode()
                request = urllib.request.Request(
                    f"{provider.base_url}/chat/completions",
                    data=payload,
                    headers={"Content-Type": "application/json"},
                )
                with urllib.request.urlopen(request, timeout=5) as response:
                    results.append(response.status)

            threads = [threading.Thread(target=invoke) for _ in range(2)]
            for thread in threads:
                thread.start()
            provider.gate.wait_saturated(5)
            held = provider.gate.snapshot()
            self.assertEqual(held["peak_active_calls"], 2)
            self.assertEqual(held["arrivals"], 2)
            provider.gate.release()
            for thread in threads:
                thread.join(5)
            provider.gate.wait_idle(5)
            self.assertEqual(results, [200, 200])


if __name__ == "__main__":
    unittest.main()
