from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from validate_release_dispatch import DispatchError, load_event, validate, write_outputs  # noqa: E402

DEFAULT_PAYLOAD = object()


def event(*, event_type="ironcrew_release_v1", payload=DEFAULT_PAYLOAD, branch="main"):
    if payload is DEFAULT_PAYLOAD:
        payload = {"tag": "v2.24.0", "mode": "publish"}
    return {
        "action": event_type,
        "client_payload": payload,
        "repository": {"default_branch": branch, "full_name": "skitsanos/ironcrew"},
        "sender": {"login": "release-operator"},
    }


class ReleaseDispatchValidationTests(unittest.TestCase):
    def test_accepts_publish_and_safe_validate_modes(self) -> None:
        self.assertEqual(
            validate(
                event(), "ironcrew_release_v1", "skitsanos/ironcrew", "release-operator"
            ),
            ("v2.24.0", "publish"),
        )
        docker = event(
            event_type="ironcrew_docker_publish_v1",
            payload={"tag": "v2.24.0", "mode": "validate"},
        )
        self.assertEqual(
            validate(
                docker,
                "ironcrew_docker_publish_v1",
                "skitsanos/ironcrew",
                "release-operator",
            ),
            ("v2.24.0", "validate"),
        )

    def test_rejects_wrong_event_type_and_non_main_default(self) -> None:
        with self.assertRaisesRegex(DispatchError, "did not match"):
            validate(
                event(),
                "ironcrew_docker_publish_v1",
                "skitsanos/ironcrew",
                "release-operator",
            )
        with self.assertRaisesRegex(DispatchError, "default branch"):
            validate(
                event(branch="develop"),
                "ironcrew_release_v1",
                "skitsanos/ironcrew",
                "release-operator",
            )
        with self.assertRaisesRegex(DispatchError, "unsupported"):
            validate(event(), "untrusted_type", "skitsanos/ironcrew", "release-operator")

    def test_rejects_extra_missing_and_non_mapping_payloads(self) -> None:
        cases = [
            {"tag": "v2.24.0"},
            {"tag": "v2.24.0", "mode": "publish", "extra": True},
            {},
            None,
            [],
            ["v2.24.0", "publish"],
        ]
        for payload in cases:
            with self.subTest(payload=payload):
                with self.assertRaisesRegex(DispatchError, "exactly mode and tag"):
                    validate(
                        event(payload=payload),
                        "ironcrew_release_v1",
                        "skitsanos/ironcrew",
                        "release-operator",
                    )

    def test_rejects_unsafe_tags_modes_and_types(self) -> None:
        cases = [
            {"tag": "latest", "mode": "publish"},
            {"tag": "v01.2.3", "mode": "publish"},
            {"tag": "v1.2.3\nmalicious", "mode": "publish"},
            {"tag": "v1.2.3", "mode": "dry-run"},
            {"tag": 123, "mode": "publish"},
            {"tag": "v1.2.3", "mode": True},
        ]
        for payload in cases:
            with self.subTest(payload=payload):
                with self.assertRaises(DispatchError):
                    validate(
                        event(payload=payload),
                        "ironcrew_release_v1",
                        "skitsanos/ironcrew",
                        "release-operator",
                    )

    def test_binds_repository_and_sender_to_trusted_context(self) -> None:
        with self.assertRaisesRegex(DispatchError, "repository identity"):
            validate(event(), "ironcrew_release_v1", "other/repo", "release-operator")
        with self.assertRaisesRegex(DispatchError, "sender"):
            validate(event(), "ironcrew_release_v1", "skitsanos/ironcrew", "other")
        with self.assertRaisesRegex(DispatchError, "repository identity is invalid"):
            validate(event(), "ironcrew_release_v1", "unsafe repo", "release-operator")
        with self.assertRaisesRegex(DispatchError, "sender identity is invalid"):
            validate(event(), "ironcrew_release_v1", "skitsanos/ironcrew", "bad actor")

    def test_event_read_is_bounded_and_outputs_are_canonical(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            event_path, output_path = root / "event.json", root / "output"
            event_path.write_text(json.dumps(event()), encoding="utf-8")
            self.assertEqual(load_event(event_path), event())
            write_outputs(output_path, "v2.24.0", "validate")
            self.assertEqual(output_path.read_text(), "tag=v2.24.0\nmode=validate\n")
            event_path.write_bytes(b"x" * (128 * 1024 + 1))
            with self.assertRaisesRegex(DispatchError, "exceeds"):
                load_event(event_path)


if __name__ == "__main__":
    unittest.main()
