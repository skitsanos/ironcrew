import json
import sys
import unittest
import urllib.request
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


sys.path.insert(0, str(Path(__file__).resolve().parent))
import soak  # noqa: E402


class StubHttpResponse:
    def __init__(self, payload: object, status: int = 200) -> None:
        self.payload = payload
        self.status = status
        self.headers: dict[str, str] = {}

    def read(self, _maximum: int) -> bytes:
        return json.dumps(self.payload).encode()

    def __enter__(self) -> "StubHttpResponse":
        return self

    def __exit__(self, *_args: object) -> None:
        return None


class ReplicaTopologyTests(unittest.TestCase):
    def test_capabilities_are_authenticated_and_only_ids_are_recorded(self) -> None:
        payloads = iter(
            [
                {"instance_id": "replica-a", "future_secret": "never-record-this"},
                {"instance_id": "replica-b", "future_secret": "never-record-this"},
            ]
            * 3
        )
        requests: list[urllib.request.Request] = []

        def open_request(
            request: urllib.request.Request, timeout: float
        ) -> StubHttpResponse:
            self.assertGreater(timeout, 0)
            requests.append(request)
            return StubHttpResponse(next(payloads))

        client = soak.HttpClient("test-bearer-secret", 1.0, soak.OperationMetrics())
        with patch.object(soak.urllib.request, "urlopen", side_effect=open_request):
            observation = soak.sample_replica_topology(
                client,
                (("route", "https://crew.example.test"),),
                sample_count=6,
                expected_instance_count=2,
                load_balanced_route=True,
            )

        self.assertTrue(observation["passed"])
        self.assertEqual(observation["observed_instance_count"], 2)
        self.assertEqual(observation["total_samples"], 6)
        self.assertEqual(
            observation["instance_id_distribution"],
            {"replica-a": 3, "replica-b": 3},
        )
        self.assertEqual(observation["recorded_capability_fields"], ["instance_id"])
        serialized = json.dumps(observation)
        self.assertNotIn("test-bearer-secret", serialized)
        self.assertNotIn("never-record-this", serialized)
        for request in requests:
            self.assertEqual(request.get_method(), "GET")
            self.assertEqual(request.full_url, "https://crew.example.test/capabilities")
            self.assertEqual(
                request.get_header("Authorization"), "Bearer test-bearer-secret"
            )

    def test_direct_sampling_round_robins_one_total_budget(self) -> None:
        class DirectClient:
            def __init__(self) -> None:
                self.urls: list[str] = []

            def request(
                self, _operation: str, _method: str, url: str, **_kwargs: object
            ) -> SimpleNamespace:
                self.urls.append(url)
                instance_id = "launch-a" if ":3311" in url else "launch-b"
                return SimpleNamespace(
                    status=200, json=lambda: {"instance_id": instance_id}
                )

        client = DirectClient()
        observation = soak.sample_replica_topology(
            client,
            (("a", "http://127.0.0.1:3311"), ("b", "http://127.0.0.1:3312")),
            sample_count=3,
            expected_instance_count=2,
            load_balanced_route=False,
        )
        self.assertEqual(
            client.urls,
            [
                "http://127.0.0.1:3311/capabilities",
                "http://127.0.0.1:3312/capabilities",
                "http://127.0.0.1:3311/capabilities",
            ],
        )
        self.assertEqual(observation["routes"]["a"]["samples"], 2)
        self.assertEqual(observation["routes"]["b"]["samples"], 1)
        self.assertEqual(observation["total_samples"], 3)

    def test_count_gate_and_cli_contract(self) -> None:
        class OneReplicaClient:
            def request(self, *_args: object, **_kwargs: object) -> SimpleNamespace:
                return SimpleNamespace(
                    status=200, json=lambda: {"instance_id": "only-replica"}
                )

        observation = soak.sample_replica_topology(
            OneReplicaClient(),
            (("route", "https://crew.example.test"),),
            sample_count=4,
            expected_instance_count=2,
            load_balanced_route=True,
        )
        self.assertFalse(observation["passed"])
        self.assertFalse(soak.topology_pass_criterion(observation)["passed"])
        self.assertFalse(soak.topology_pass_criterion(None)["passed"])

        with patch.dict(
            soak.os.environ, {"IRONCREW_API_TOKEN": "test-token"}, clear=False
        ):
            args = soak.parse_args(
                [
                    "--mode",
                    "target",
                    "--database-url",
                    "postgres://user:pass@db.example/crew",
                    "--table-prefix",
                    "soak_a1_",
                    "--base-a",
                    "https://crew.example.test",
                    "--load-balanced-route",
                    "--capability-samples",
                    "8",
                ]
            )
        self.assertTrue(args.load_balanced_route)
        self.assertIsNone(args.base_b)
        self.assertEqual(args.expected_instance_count, 2)

        with patch.dict(
            soak.os.environ, {"IRONCREW_API_TOKEN": "test-token"}, clear=False
        ):
            with patch("sys.stderr"), self.assertRaises(SystemExit):
                soak.parse_args(
                    [
                        "--mode",
                        "target",
                        "--database-url",
                        "postgres://user:pass@db.example/crew",
                        "--table-prefix",
                        "soak_a1_",
                        "--base-a",
                        "https://crew.example.test",
                        "--load-balanced-route",
                        "--expected-instance-count",
                        "1",
                    ]
                )
            with patch("sys.stderr"), self.assertRaises(SystemExit):
                soak.parse_args(
                    [
                        "--mode",
                        "target",
                        "--database-url",
                        "postgres://user:pass@db.example/crew",
                        "--table-prefix",
                        "soak_a1_",
                        "--base-a",
                        "https://crew.example.test",
                        "--base-b",
                        "https://crew.example.test/",
                    ]
                )
        with patch.dict(soak.os.environ, {}, clear=True):
            with patch("sys.stderr"), self.assertRaises(SystemExit):
                soak.parse_args(
                    [
                        "--mode",
                        "target",
                        "--database-url",
                        "postgres://user:pass@db.example/crew",
                        "--table-prefix",
                        "soak_a1_",
                        "--base-a",
                        "https://crew.example.test",
                        "--load-balanced-route",
                    ]
                )

    def test_launch_defaults_and_invalid_capability_ids(self) -> None:
        args = soak.parse_args(
            [
                "--database-url",
                "postgres://user:pass@db.example/crew",
                "--binary",
                __file__,
            ]
        )
        self.assertEqual(args.mode, "launch")
        self.assertFalse(args.load_balanced_route)
        self.assertEqual(args.expected_instance_count, 2)
        self.assertEqual(args.capability_samples, 32)
        with patch("sys.stderr"), self.assertRaises(SystemExit):
            soak.parse_args(
                [
                    "--database-url",
                    "postgres://user:pass@db.example/crew",
                    "--binary",
                    __file__,
                    "--expected-instance-count",
                    "1",
                ]
            )

        invalid_ids: tuple[object, ...] = (
            None,
            42,
            "",
            "line\nbreak",
            "non-ascii-☃",
            "x" * 256,
        )
        for instance_id in invalid_ids:
            with self.subTest(instance_id=type(instance_id).__name__):
                client = SimpleNamespace(
                    request=lambda *_args, **_kwargs: SimpleNamespace(
                        status=200, json=lambda: {"instance_id": instance_id}
                    )
                )
                with self.assertRaisesRegex(RuntimeError, "instance_id"):
                    soak.sample_replica_topology(
                        client,
                        (("route", "https://crew.example.test"),),
                        1,
                        1,
                        True,
                    )

    def test_unavailable_rss_is_not_platform_resource_proof(self) -> None:
        metrics = soak.OperationMetrics()
        metrics.record("health_liveness_probe", 1.0, 200, True)
        metrics.record("health_readiness_probe", 1.0, 200, True)
        report = {
            "workload": {"attempted_runs": 1, "failed_runs": 0},
            "replica_topology": {
                "passed": True,
                "expected_instance_count": 2,
                "observed_instance_count": 2,
                "total_samples": 8,
            },
            "postgres": {
                "delta": {
                    "database_activity": {"deadlocks": 0},
                    "stats_reset_changed": False,
                }
            },
            "resources": {"replicas": {"a": {}, "b": {}}},
        }
        criteria = soak.build_pass_criteria(
            report,
            metrics,
            SimpleNamespace(mode="target", memory_comparator_mib=1024),
            SimpleNamespace(processes={}),
        )
        rss = criteria["host_process_rss_comparator"]
        self.assertEqual(rss["status"], "not_available")
        self.assertFalse(rss["applicable"])
        self.assertIsNone(rss["passed"])
        self.assertFalse(rss["platform_resource_proof"])
        self.assertEqual(rss["reason"], "no host-process RSS samples were available")
        self.assertTrue(criteria["overall_passed"])


if __name__ == "__main__":
    unittest.main()
