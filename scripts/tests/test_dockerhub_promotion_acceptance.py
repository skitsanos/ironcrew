from __future__ import annotations

import io
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from dockerhub_acceptance_api import (  # noqa: E402
    AcceptanceError,
    DockerHubApi,
    description,
    fingerprint,
    repository_name,
)
from dockerhub_immutability import SEMVER_IMMUTABILITY_RULE  # noqa: E402
from dockerhub_promotion_acceptance import (  # noqa: E402
    VERSION_A,
    VERSION_B,
    read_bound_evidence,
    require_registry_tags_absent,
    run_acceptance,
    stage_image,
)
from release_promotion_protocol import ReleaseImage  # noqa: E402

RUN_ID = "20260812t120000z-deadbeef"
DIGEST_A = "sha256:" + "a" * 64
DIGEST_B = "sha256:" + "b" * 64


class FakeRegistry:
    def __init__(self, *, reject_overwrite: bool = True,
                 fail_mutable_control: bool = False):
        self.tags: dict[str, str] = {}
        self.copies: list[tuple[str, str, str]] = []
        self.reject_overwrite = reject_overwrite
        self.fail_mutable_control = fail_mutable_control

    def inspect(self, tag: str) -> str | None:
        return self.tags.get(tag)

    def copy(self, image: ReleaseImage, tag: str) -> bool:
        self.copies.append((image.tag, image.digest, tag))
        if tag == "latest" and self.fail_mutable_control and image.digest == DIGEST_B:
            return False
        if tag in {"0.0.1", "0.0.2"} and tag in self.tags \
                and self.tags[tag] != image.digest and self.reject_overwrite:
            return False
        self.tags[tag] = image.digest
        return True


class PromotionAcceptanceTests(unittest.TestCase):
    def images(self) -> tuple[ReleaseImage, ReleaseImage]:
        return (
            ReleaseImage(VERSION_A, Path("a.tar"), DIGEST_A),
            ReleaseImage(VERSION_B, Path("b.tar"), DIGEST_B),
        )

    def test_full_protocol_publishes_replays_rejects_and_repairs_latest(self) -> None:
        registry = FakeRegistry()
        policy_checks = 0

        def verify_policy() -> None:
            nonlocal policy_checks
            policy_checks += 1

        evidence = run_acceptance(  # type: ignore[arg-type]
            registry, *self.images(), verify_policy=verify_policy
        )
        self.assertEqual(
            registry.tags,
            {"0.0.1": DIGEST_A, "0.0.2": DIGEST_B, "latest": DIGEST_B},
        )
        self.assertEqual(evidence["initial_version_promotion"], "published")
        self.assertEqual(evidence["identical_replay"], "no-op")
        self.assertEqual(evidence["conflicting_replay"], "protocol-refused")
        self.assertEqual(
            evidence["registry_overwrite"],
            "semver-only-rejected-with-mutable-control",
        )
        self.assertEqual(evidence["newer_version_promotion"], "published")
        self.assertEqual(evidence["latest_attempts"], 2)
        self.assertEqual(policy_checks, 2)
        self.assertEqual(
            [copy[2] for copy in registry.copies],
            ["0.0.1", "0.0.1", "latest", "latest", "0.0.2", "latest"],
        )

    def test_registry_that_allows_overwrite_fails_closed(self) -> None:
        registry = FakeRegistry(reject_overwrite=False)
        with self.assertRaisesRegex(AcceptanceError, "did not reject"):
            run_acceptance(  # type: ignore[arg-type]
                registry, *self.images(), verify_policy=lambda: None
            )

    def test_unrelated_copy_failure_does_not_count_as_immutability(self) -> None:
        registry = FakeRegistry(fail_mutable_control=True)
        with self.assertRaisesRegex(AcceptanceError, "mutable-tag control failed"):
            run_acceptance(  # type: ignore[arg-type]
                registry, *self.images(), verify_policy=lambda: None
            )

    def test_identical_source_digests_fail_before_any_copy(self) -> None:
        registry = FakeRegistry()
        first, second = self.images()
        second = ReleaseImage(second.tag, second.archive, first.digest)
        with self.assertRaisesRegex(AcceptanceError, "different digests"):
            run_acceptance(  # type: ignore[arg-type]
                registry, first, second, verify_policy=lambda: None
            )
        self.assertEqual(registry.copies, [])

    def test_cleanup_requires_every_acceptance_tag_to_be_absent(self) -> None:
        registry = FakeRegistry()
        require_registry_tags_absent(  # type: ignore[arg-type]
            registry, ["0.0.1", "0.0.2", "latest"]
        )
        registry.tags["latest"] = DIGEST_B
        with self.assertRaisesRegex(AcceptanceError, "still exist"):
            require_registry_tags_absent(  # type: ignore[arg-type]
                registry, ["0.0.1", "0.0.2", "latest"]
            )

    def test_stage_image_requires_pinned_source_and_checks_archive_digest(self) -> None:
        commands: list[list[str]] = []
        raw_index = '{"manifests":[],"schemaVersion":2}'
        digest = "sha256:" + hashlib.sha256(raw_index.encode()).hexdigest()

        class Runner:
            @staticmethod
            def _run(command: list[str]):
                commands.append(command)
                output = raw_index if command[1] == "inspect" else ""
                return subprocess.CompletedProcess(command, 0, output, "")

        source = f"docker://example.invalid/image@{digest}"
        image = stage_image(Runner(), source, Path("a.tar"), VERSION_A)  # type: ignore[arg-type]
        self.assertEqual(image.digest, digest)
        self.assertEqual(commands[0][-2:], [source, "oci-archive:a.tar"])
        self.assertEqual(commands[1][1:3], ["inspect", "--raw"])
        with self.assertRaisesRegex(AcceptanceError, "pinned"):
            stage_image(Runner(), "docker://example.invalid/image:latest", Path("x"), VERSION_A)  # type: ignore[arg-type]


class FakeResponse:
    def __init__(self, status: int, document: object):
        self.status = status
        self.body = json.dumps(document).encode()

    def __enter__(self):
        return self

    def __exit__(self, *_args: object) -> None:
        return None

    def read(self, limit: int) -> bytes:
        return self.body[:limit]


def http_error(url: str, status: int, document: object) -> urllib.error.HTTPError:
    return urllib.error.HTTPError(
        url, status, "error", {}, io.BytesIO(json.dumps(document).encode())
    )


class DockerHubApiTests(unittest.TestCase):
    def state(self) -> dict[str, object]:
        return {
            "namespace": "skitsanos",
            "name": repository_name(RUN_ID),
            "description": description(RUN_ID),
            "repository_type": "image",
            "is_private": False,
            "permissions": {"admin": True},
            "date_registered": "2026-08-12T12:00:01Z",
            "immutable_tags_settings": {
                "enabled": True,
                "rules": [SEMVER_IMMUTABILITY_RULE],
            },
        }

    def api(self, responses: list[object]) -> tuple[DockerHubApi, mock.Mock]:
        patcher = mock.patch("urllib.request.urlopen", side_effect=responses)
        opened = patcher.start()
        self.addCleanup(patcher.stop)
        api = DockerHubApi(
            namespace="skitsanos", run_id=RUN_ID,
            username="publisher", secret="never-log-this",
            api_base="http://127.0.0.1:12345",
        )
        return api, opened

    def test_repository_name_can_only_target_disposable_prefix(self) -> None:
        self.assertEqual(
            repository_name(RUN_ID),
            "ironcrew-ic015-acceptance-20260812t120000z-deadbeef",
        )
        for invalid in (
            "ironcrew", "20260812", "20260812t120000z-../../x",
            "20260230t120000z-deadbeef", "20260812t250000z-deadbeef",
        ):
            with self.subTest(invalid=invalid), self.assertRaises(AcceptanceError):
                repository_name(invalid)

    def test_create_uses_exact_identity_and_semver_policy(self) -> None:
        state = self.state()
        missing = http_error("http://fixture/repository", 404, {"detail": "missing"})
        api, opened = self.api([
            FakeResponse(200, {"access_token": "safe-token"}),
            missing,
            FakeResponse(201, state),
            FakeResponse(200, state),
            FakeResponse(200, state),
        ])
        self.assertEqual(api.create(), state)
        requests = [call.args[0] for call in opened.call_args_list]
        create_body = json.loads(requests[2].data)
        policy_body = json.loads(requests[3].data)
        self.assertEqual(create_body["name"], repository_name(RUN_ID))
        self.assertEqual(create_body["description"], description(RUN_ID))
        self.assertFalse(create_body["is_private"])
        self.assertEqual(policy_body, {
            "immutable_tags": True,
            "immutable_tags_rules": [SEMVER_IMMUTABILITY_RULE],
        })
        self.assertEqual(requests[4].headers["Authorization"], "Bearer safe-token")

    def test_identity_policy_permission_and_inventory_fail_closed(self) -> None:
        api, _ = self.api([FakeResponse(200, {"access_token": "safe-token"})])
        cases = [
            {"name": "ironcrew"},
            {"permissions": {"admin": False}},
            {"immutable_tags_settings": {"enabled": False, "rules": []}},
            {"date_registered": ""},
        ]
        for change in cases:
            with self.subTest(change=change):
                state = self.state() | change
                with mock.patch.object(api, "repository", return_value=state):
                    with self.assertRaises(AcceptanceError):
                        api.require_identity()
        with mock.patch.object(
            api, "_request", return_value={"count": 2, "results": [{"name": "one"}]}
        ):
            with self.assertRaisesRegex(AcceptanceError, "incomplete"):
                api.tags()
        with mock.patch.object(
            api,
            "_request",
            return_value={
                "count": 0,
                "next": None,
                "previous": None,
                "results": [{"name": "latest"}, {"name": "0.0.1"}],
            },
        ):
            self.assertEqual(api.tags(), ["0.0.1", "latest"])

    def test_api_errors_do_not_expose_credentials(self) -> None:
        error = http_error(
            "http://127.0.0.1:12345/v2/auth/token", 401,
            {"detail": "never-log-this"},
        )
        with mock.patch("urllib.request.urlopen", side_effect=error):
            with self.assertRaises(AcceptanceError) as raised:
                DockerHubApi(
                    namespace="skitsanos", run_id=RUN_ID,
                    username="publisher", secret="never-log-this",
                    api_base="http://127.0.0.1:12345",
                )
        self.assertNotIn("never-log-this", str(raised.exception))


class EvidenceBindingTests(unittest.TestCase):
    def test_bound_evidence_requires_same_run_repository_phase_and_identity(self) -> None:
        api = object.__new__(DockerHubApi)
        api.namespace = "skitsanos"
        api.run_id = RUN_ID
        api.name = repository_name(RUN_ID)
        api.image = f"skitsanos/{api.name}"
        identity = fingerprint(DockerHubApiTests().state())
        document = {
            "schema": "ironcrew.ic015.dockerhub-acceptance.v1",
            "phase": "prepared",
            "run_id": RUN_ID,
            "repository": api.image,
            "repository_fingerprint": identity,
            "semver_rule": SEMVER_IMMUTABILITY_RULE,
        }
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "prepare.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            self.assertEqual(
                read_bound_evidence(path, phase="prepared", api=api, identity=identity),
                document,
            )
            document["repository"] = "skitsanos/ironcrew"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(AcceptanceError, "did not match"):
                read_bound_evidence(path, phase="prepared", api=api, identity=identity)


if __name__ == "__main__":
    unittest.main()
