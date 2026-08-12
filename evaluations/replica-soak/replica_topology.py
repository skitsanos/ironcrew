"""Bounded replica-identity evidence for the deployed soak target."""

from __future__ import annotations

from collections import Counter
from collections.abc import Sequence
from typing import Any, Protocol


MAX_INSTANCE_ID_BYTES = 255


class JsonResponse(Protocol):
    status: int

    def json(self) -> Any: ...


class CapabilityClient(Protocol):
    def request(
        self,
        operation: str,
        method: str,
        url: str,
        payload: Any | None = None,
        headers: dict[str, str] | None = None,
    ) -> JsonResponse: ...


def _instance_id(payload: Any) -> str:
    if not isinstance(payload, dict):
        raise RuntimeError("capabilities response must be a JSON object")
    instance_id = payload.get("instance_id")
    if not isinstance(instance_id, str) or not instance_id:
        raise RuntimeError("capabilities response omitted instance_id")
    encoded = instance_id.encode("utf-8")
    if len(encoded) > MAX_INSTANCE_ID_BYTES or any(
        not 0x20 <= ord(character) <= 0x7E for character in instance_id
    ):
        raise RuntimeError("capabilities response contained an invalid instance_id")
    return instance_id


def sample_replica_topology(
    client: CapabilityClient,
    routes: Sequence[tuple[str, str]],
    sample_count: int,
    expected_instance_count: int,
    load_balanced_route: bool,
) -> dict[str, Any]:
    """Sample only instance IDs from authenticated capability responses.

    The caller supplies the normal authenticated HTTP client. Full capability
    payloads are deliberately discarded so keyring/control configuration and
    future secret-bearing fields cannot enter the report accidentally.
    """

    if not routes:
        raise ValueError("at least one capability route is required")
    if sample_count < 1:
        raise ValueError("capability sample count must be positive")
    if expected_instance_count < 1:
        raise ValueError("expected instance count must be positive")

    aggregate: Counter[str] = Counter()
    by_route = {label: Counter() for label, _ in routes}
    for index in range(sample_count):
        label, base_url = routes[index % len(routes)]
        response = client.request(
            "capabilities_probe", "GET", f"{base_url}/capabilities"
        )
        if response.status != 200:
            raise RuntimeError(
                f"capabilities probe returned HTTP {response.status}"
            )
        instance_id = _instance_id(response.json())
        aggregate[instance_id] += 1
        by_route[label][instance_id] += 1

    observed = len(aggregate)
    return {
        "mode": "load_balanced_route" if load_balanced_route else "direct",
        "expected_instance_count": expected_instance_count,
        "observed_instance_count": observed,
        "total_samples": sample_count,
        "passed": observed >= expected_instance_count,
        "instance_id_distribution": dict(sorted(aggregate.items())),
        "routes": {
            label: {
                "samples": sum(distribution.values()),
                "instance_id_distribution": dict(sorted(distribution.items())),
            }
            for label, distribution in sorted(by_route.items())
        },
        "recorded_capability_fields": ["instance_id"],
    }


def topology_pass_criterion(observation: dict[str, Any] | None) -> dict[str, Any]:
    if not observation:
        return {
            "status": "not_available",
            "applicable": True,
            "passed": False,
            "reason": "capability identity sampling did not complete",
        }
    passed = observation.get("passed") is True
    return {
        "status": "passed" if passed else "failed",
        "applicable": True,
        "passed": passed,
        "expected_instance_count": observation.get("expected_instance_count"),
        "observed_instance_count": observation.get("observed_instance_count"),
        "total_samples": observation.get("total_samples"),
    }


def host_rss_pass_criterion(
    resources: dict[str, Any], comparator_bytes: int
) -> dict[str, Any]:
    available_peaks = {
        name: values.get("sampled_peak_rss_bytes")
        for name, values in resources.items()
        if isinstance(values.get("sampled_peak_rss_bytes"), int)
    }
    if not available_peaks:
        return {
            "status": "not_available",
            "applicable": False,
            "passed": None,
            "sampled_peaks_bytes": {},
            "comparator_bytes": comparator_bytes,
            "comparator_enforced_by_runner": False,
            "platform_resource_proof": False,
            "scope": "host_process_rss",
            "reason": "no host-process RSS samples were available",
        }

    passed = all(peak < comparator_bytes for peak in available_peaks.values())
    return {
        "status": "passed" if passed else "failed",
        "applicable": True,
        "passed": passed,
        "sampled_peaks_bytes": available_peaks,
        "comparator_bytes": comparator_bytes,
        "comparator_enforced_by_runner": False,
        "platform_resource_proof": False,
        "scope": "host_process_rss",
        "reason": (
            "host-process RSS is not pod/container cgroup or platform limit evidence"
        ),
    }
