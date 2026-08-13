from __future__ import annotations

import json
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from dockerhub_immutability import (  # noqa: E402
    ImmutabilityPolicyError,
    MAX_API_RESPONSE_BYTES,
    SEMVER_IMMUTABILITY_RULE,
    require_semver_immutability,
)
from promote_release_image import (  # noqa: E402
    PromotionError,
    ReleaseImage,
    promote_version,
    reconcile_latest,
    stable_version,
)

DIGEST_A = "sha256:" + "a" * 64
DIGEST_B = "sha256:" + "b" * 64


class FakeBackend:
    def __init__(self) -> None:
        self.tags: dict[str, str] = {}
        self.images = {
            "v1.2.3": ReleaseImage("v1.2.3", Path("v1.tar"), DIGEST_A),
            "v1.2.4": ReleaseImage("v1.2.4", Path("v2.tar"), DIGEST_B),
        }
        self.latest_sequence = ["v1.2.3"]
        self.latest_reads = 0
        self.copies: list[tuple[str, str]] = []
        self.copy_success = True
        self.post_copy_digest: str | None = None

    def inspect(self, tag: str) -> str | None:
        return self.tags.get(tag)

    def copy(self, image: ReleaseImage, tag: str) -> bool:
        self.copies.append((image.tag, tag))
        if self.copy_success:
            self.tags[tag] = self.post_copy_digest or image.digest
        return self.copy_success

    def latest_release_tag(self) -> str:
        position = min(self.latest_reads, len(self.latest_sequence) - 1)
        self.latest_reads += 1
        return self.latest_sequence[position]

    def release_image(self, tag: str) -> ReleaseImage:
        return self.images[tag]


class PromotionProtocolTests(unittest.TestCase):
    def test_stable_version_is_exact_and_unambiguous(self) -> None:
        self.assertEqual(stable_version("v0.0.0"), "0.0.0")
        self.assertEqual(stable_version("v12.34.56"), "12.34.56")
        for value in ("1.2.3", "v1.2", "v01.2.3", "v1.02.3", "v1.2.03", "v1.2.3-rc1", "v1.2.3\n"):
            with self.subTest(value=value), self.assertRaises(PromotionError):
                stable_version(value)

    def test_absent_version_is_copied_and_verified(self) -> None:
        backend = FakeBackend()
        result = promote_version(backend, backend.images["v1.2.3"])
        self.assertEqual(result, "published")
        self.assertEqual(backend.tags["1.2.3"], DIGEST_A)
        self.assertEqual(backend.copies, [("v1.2.3", "1.2.3")])

    def test_existing_identical_version_is_a_strict_no_op(self) -> None:
        backend = FakeBackend()
        backend.tags["1.2.3"] = DIGEST_A
        self.assertEqual(promote_version(backend, backend.images["v1.2.3"]), "no-op")
        self.assertEqual(backend.copies, [])

    def test_existing_different_version_fails_before_copy(self) -> None:
        backend = FakeBackend()
        backend.tags["1.2.3"] = DIGEST_B
        with self.assertRaisesRegex(PromotionError, "different digest"):
            promote_version(backend, backend.images["v1.2.3"])
        self.assertEqual(backend.copies, [])

    def test_concurrent_identical_version_after_rejected_copy_is_no_op(self) -> None:
        backend = FakeBackend()

        def concurrent_copy(image: ReleaseImage, tag: str) -> bool:
            backend.copies.append((image.tag, tag))
            backend.tags[tag] = image.digest
            return False

        backend.copy = concurrent_copy  # type: ignore[method-assign]
        self.assertEqual(
            promote_version(backend, backend.images["v1.2.3"]), "concurrent-no-op"
        )

    def test_failed_or_wrong_version_copy_fails_postcondition(self) -> None:
        backend = FakeBackend()
        backend.copy_success = False
        with self.assertRaisesRegex(PromotionError, "copy failed"):
            promote_version(backend, backend.images["v1.2.3"])
        backend = FakeBackend()
        backend.post_copy_digest = DIGEST_B
        with self.assertRaisesRegex(PromotionError, "did not match"):
            promote_version(backend, backend.images["v1.2.3"])

    def test_latest_no_op_still_revalidates_github_latest(self) -> None:
        backend = FakeBackend()
        backend.tags["latest"] = DIGEST_A
        self.assertEqual(reconcile_latest(backend, max_attempts=3), ("v1.2.3", 1))
        self.assertEqual(backend.copies, [])
        self.assertEqual(backend.latest_reads, 2)

    def test_latest_copy_is_digest_verified(self) -> None:
        backend = FakeBackend()
        self.assertEqual(reconcile_latest(backend, max_attempts=3), ("v1.2.3", 1))
        self.assertEqual(backend.tags["latest"], DIGEST_A)
        self.assertEqual(backend.copies, [("v1.2.3", "latest")])

    def test_newer_release_after_write_is_verified_and_repaired(self) -> None:
        backend = FakeBackend()
        backend.latest_sequence = ["v1.2.3", "v1.2.4", "v1.2.4"]
        self.assertEqual(reconcile_latest(backend, max_attempts=3), ("v1.2.4", 2))
        self.assertEqual(
            backend.copies,
            [("v1.2.3", "latest"), ("v1.2.4", "latest")],
        )
        self.assertEqual(backend.tags["latest"], DIGEST_B)

    def test_latest_churn_is_bounded_and_fails(self) -> None:
        backend = FakeBackend()
        backend.latest_sequence = [
            "v1.2.3", "v1.2.4", "v1.2.3", "v1.2.4", "v1.2.3", "v1.2.4"
        ]
        with self.assertRaisesRegex(PromotionError, "bounded reconciliation"):
            reconcile_latest(backend, max_attempts=3)
        self.assertEqual(backend.latest_reads, 6)

    def test_latest_rejects_failed_copy_wrong_digest_and_receipt_tag(self) -> None:
        backend = FakeBackend()
        backend.copy_success = False
        with self.assertRaisesRegex(PromotionError, "copy failed"):
            reconcile_latest(backend, max_attempts=1)
        backend = FakeBackend()
        backend.post_copy_digest = DIGEST_B
        with self.assertRaisesRegex(PromotionError, "did not match"):
            reconcile_latest(backend, max_attempts=1)
        backend = FakeBackend()
        backend.images["v1.2.3"] = ReleaseImage("v1.2.4", Path("bad.tar"), DIGEST_A)
        with self.assertRaisesRegex(PromotionError, "did not match GitHub latest"):
            reconcile_latest(backend, max_attempts=1)

    def test_latest_attempt_bound_is_enforced(self) -> None:
        for attempts in (0, 11):
            with self.subTest(attempts=attempts), self.assertRaises(PromotionError):
                reconcile_latest(FakeBackend(), max_attempts=attempts)


class _DockerHubHandler(BaseHTTPRequestHandler):
    policy = {"enabled": True, "rules": [SEMVER_IMMUTABILITY_RULE]}
    auth_status = 200
    repository_status = 200
    auth_body: dict[str, object] = {"access_token": "safe-token"}
    raw_auth_body: bytes | None = None
    observed_authorization = ""

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/v2/auth/token":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        self.send_response(self.auth_status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(
            self.raw_auth_body
            if self.raw_auth_body is not None
            else json.dumps(self.auth_body).encode()
        )

    def do_GET(self) -> None:  # noqa: N802
        if self.path != "/v2/namespaces/skitsanos/repositories/ironcrew":
            self.send_error(404)
            return
        type(self).observed_authorization = self.headers.get("Authorization", "")
        self.send_response(self.repository_status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(
            json.dumps({"immutable_tags_settings": self.policy}).encode()
        )

    def log_message(self, _format: str, *_args: object) -> None:
        return


class DockerHubPolicyTests(unittest.TestCase):
    def setUp(self) -> None:
        _DockerHubHandler.policy = {"enabled": True, "rules": [SEMVER_IMMUTABILITY_RULE]}
        _DockerHubHandler.auth_status = 200
        _DockerHubHandler.repository_status = 200
        _DockerHubHandler.auth_body = {"access_token": "safe-token"}
        _DockerHubHandler.raw_auth_body = None
        _DockerHubHandler.observed_authorization = ""
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _DockerHubHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.api = f"http://127.0.0.1:{self.server.server_port}"

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def verify(self) -> None:
        require_semver_immutability(
            image="skitsanos/ironcrew",
            username="publisher",
            secret="never-log-this",
            api_base=self.api,
        )

    def test_exact_enabled_semver_rule_passes_with_authenticated_query(self) -> None:
        self.verify()
        self.assertEqual(_DockerHubHandler.observed_authorization, "Bearer safe-token")

    def test_disabled_missing_broad_or_malformed_policy_fails(self) -> None:
        policies: list[object] = [
            {"enabled": False, "rules": [SEMVER_IMMUTABILITY_RULE]},
            {"enabled": True, "rules": []},
            {"enabled": True, "rules": [".*"]},
            {"enabled": True, "rules": [SEMVER_IMMUTABILITY_RULE, ".*"]},
            {"enabled": True, "rules": [SEMVER_IMMUTABILITY_RULE, 7]},
            "invalid",
        ]
        for policy in policies:
            with self.subTest(policy=policy):
                _DockerHubHandler.policy = policy  # type: ignore[assignment]
                with self.assertRaises(ImmutabilityPolicyError):
                    self.verify()

    def test_auth_and_repository_failures_are_redacted(self) -> None:
        _DockerHubHandler.auth_body = {}
        with self.assertRaises(ImmutabilityPolicyError) as missing:
            self.verify()
        self.assertNotIn("never-log-this", str(missing.exception))
        _DockerHubHandler.auth_status = 401
        with self.assertRaises(ImmutabilityPolicyError) as denied:
            self.verify()
        self.assertNotIn("never-log-this", str(denied.exception))
        _DockerHubHandler.auth_status = 200
        _DockerHubHandler.auth_body = {"access_token": "safe-token"}
        _DockerHubHandler.repository_status = 403
        with self.assertRaises(ImmutabilityPolicyError):
            self.verify()

    def test_oversized_api_response_fails_before_json_materialization(self) -> None:
        _DockerHubHandler.raw_auth_body = b" " * (MAX_API_RESPONSE_BYTES + 1)
        with self.assertRaisesRegex(ImmutabilityPolicyError, "byte limit"):
            self.verify()

    def test_credentials_image_and_api_transport_fail_closed(self) -> None:
        with self.assertRaises(ImmutabilityPolicyError):
            require_semver_immutability(
                image="skitsanos/ironcrew", username="", secret="", api_base=self.api
            )
        for image in ("ironcrew", "Docker.IO/skitsanos/ironcrew", "skitsanos/../ironcrew"):
            with self.subTest(image=image), self.assertRaises(ImmutabilityPolicyError):
                require_semver_immutability(
                    image=image, username="user", secret="token", api_base=self.api
                )
        with self.assertRaises(ImmutabilityPolicyError):
            require_semver_immutability(
                image="skitsanos/ironcrew",
                username="user",
                secret="token",
                api_base="http://example.com",
            )


if __name__ == "__main__":
    unittest.main()
