from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from validate_release_request import (  # noqa: E402
    Request,
    RequestError,
    load_event,
    validate,
    write_outputs,
)


def event(
    *,
    label: str = "release-request",
    body: str = '{"target":"release","tag":"v2.24.0","mode":"validate"}',
) -> dict:
    return {
        "action": "labeled",
        "issue": {
            "author_association": "OWNER",
            "body": body,
            "labels": [{"name": label}],
            "number": 14,
            "state": "open",
            "title": "IronCrew release request",
            "user": {"login": "skitsanos"},
        },
        "label": {"name": label},
        "repository": {
            "default_branch": "main",
            "full_name": "skitsanos/ironcrew",
            "owner": {"login": "skitsanos"},
        },
        "sender": {"login": "skitsanos"},
    }


class ReleaseRequestValidationTests(unittest.TestCase):
    def validate(self, payload: object) -> Request:
        return validate(
            payload,
            "skitsanos/ironcrew",
            "skitsanos",
            "skitsanos",
            "skitsanos",
        )

    def test_accepts_each_fixed_target_and_mode(self) -> None:
        cases = [
            ("release", "validate"),
            ("release", "publish"),
            ("docker", "validate"),
            ("docker", "publish"),
        ]
        for target, mode in cases:
            with self.subTest(target=target, mode=mode):
                body = json.dumps(
                    {"target": target, "tag": "v2.24.0", "mode": mode},
                    separators=(",", ":"),
                )
                self.assertEqual(
                    self.validate(event(body=body)),
                    Request(True, target, "v2.24.0", mode),
                )

    def test_irrelevant_label_is_a_safe_noop_before_actor_and_issue_parsing(self) -> None:
        payload = event(label="documentation", body="not JSON")
        payload["issue"]["labels"] = [{"name": "release-request"}, {"name": "documentation"}]
        payload["sender"] = None
        self.assertEqual(
            validate(
                payload,
                "skitsanos/ironcrew",
                "maintainer",
                "rerunning-maintainer",
                "skitsanos",
            ),
            Request(False),
        )

    def test_rejects_non_owner_actor_author_and_association(self) -> None:
        cases = []
        actor = event()
        actor["sender"]["login"] = "maintainer"
        cases.append(actor)
        author = event()
        author["issue"]["user"]["login"] = "maintainer"
        cases.append(author)
        association = event()
        association["issue"]["author_association"] = "MEMBER"
        cases.append(association)
        for payload in cases:
            with self.subTest(payload=payload):
                with self.assertRaises(RequestError):
                    self.validate(payload)
        with self.assertRaisesRegex(RequestError, "only the configured owner"):
            validate(
                event(),
                "skitsanos/ironcrew",
                "maintainer",
                "skitsanos",
                "skitsanos",
            )
        with self.assertRaisesRegex(RequestError, "only the configured owner"):
            validate(
                event(),
                "skitsanos/ironcrew",
                "skitsanos",
                "maintainer",
                "skitsanos",
            )

    def test_rejects_wrong_repository_branch_owner_and_event(self) -> None:
        mutations = [
            ("repository", "full_name", "other/ironcrew"),
            ("repository", "default_branch", "develop"),
            ("repository", "owner", {"login": "other"}),
        ]
        for parent, key, value in mutations:
            payload = event()
            payload[parent][key] = value
            with self.subTest(key=key):
                with self.assertRaisesRegex(RequestError, "repository identity"):
                    self.validate(payload)
        payload = event()
        payload["action"] = "closed"
        with self.assertRaisesRegex(RequestError, "issues:labeled"):
            self.validate(payload)

    def test_rejects_pull_request_closed_issue_and_non_exact_metadata(self) -> None:
        cases = []
        pull_request = event()
        pull_request["issue"]["pull_request"] = {"url": "https://invalid.example"}
        cases.append(pull_request)
        closed = event()
        closed["issue"]["state"] = "closed"
        cases.append(closed)
        title = event()
        title["issue"]["title"] = "Release request"
        cases.append(title)
        labels = event()
        labels["issue"]["labels"].append({"name": "extra"})
        cases.append(labels)
        for payload in cases:
            with self.subTest(payload=payload):
                with self.assertRaises(RequestError):
                    self.validate(payload)

    def test_rejects_noncanonical_extra_duplicate_and_oversized_body(self) -> None:
        bodies = [
            '{"target": "release", "tag":"v2.24.0","mode":"validate"}',
            '{"tag":"v2.24.0","target":"release","mode":"validate"}',
            '{"target":"release","tag":"v2.24.0","mode":"validate","extra":true}',
            '{"target":"release","tag":"v2.24.0","mode":"validate","mode":"publish"}',
            "x" * 513,
            "\ud800",
            "[]",
        ]
        for body in bodies:
            with self.subTest(body=body[:80]):
                with self.assertRaises(RequestError):
                    self.validate(event(body=body))

    def test_rejects_unsafe_target_tag_mode_and_types(self) -> None:
        requests = [
            {"target": "both", "tag": "v2.24.0", "mode": "validate"},
            {"target": "release", "tag": "latest", "mode": "validate"},
            {"target": "release", "tag": "v01.2.3", "mode": "validate"},
            {"target": "release", "tag": "v1.2.3\nunsafe", "mode": "validate"},
            {"target": "release", "tag": "v2.24.0", "mode": "dry-run"},
            {"target": True, "tag": "v2.24.0", "mode": "validate"},
            {"target": "release", "tag": 2240, "mode": "validate"},
            {"target": "release", "tag": "v2.24.0", "mode": True},
        ]
        for request in requests:
            with self.subTest(request=request):
                body = json.dumps(request, separators=(",", ":"))
                with self.assertRaises(RequestError):
                    self.validate(event(body=body))

    def test_event_read_is_bounded_and_outputs_are_canonical(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            event_path, output_path = root / "event.json", root / "output"
            event_path.write_text(json.dumps(event()), encoding="utf-8")
            self.assertEqual(load_event(event_path), event())
            write_outputs(output_path, Request(False))
            write_outputs(output_path, Request(True, "docker", "v2.24.0", "publish"))
            self.assertEqual(
                output_path.read_text(),
                "relevant=false\n"
                "relevant=true\ntarget=docker\ntag=v2.24.0\nmode=publish\n",
            )
            event_path.write_bytes(b"x" * (256 * 1024 + 1))
            with self.assertRaisesRegex(RequestError, "exceeds"):
                load_event(event_path)


if __name__ == "__main__":
    unittest.main()
