from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from promote_release_image import (  # noqa: E402
    ASSET_SIZE_LIMITS,
    COMMAND_OUTPUT_MAX_BYTES,
    CommandBackend,
    PromotionError,
)

TAG = "v1.2.3"
DIGEST = "sha256:" + "a" * 64
TAG_OBJECT = "b" * 40
COMMIT = "c" * 40


class CommandBackendTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="ironcrew-promotion-test-")
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        self.backend = CommandBackend(
            repository="skitsanos/ironcrew",
            image="skitsanos/ironcrew",
            work=self.work,
            validator=Path("scripts/verify_release_image.py"),
        )
        archive = f"ironcrew-{TAG}-linux-oci.tar"
        receipt = f"ironcrew-{TAG}-image-receipt.v1.json"
        self.contents = {
            archive: b"signed OCI archive",
            f"{archive}.bundle": b"archive signature bundle",
            receipt: json.dumps(
                {"commit_sha": COMMIT, "oci_archive": {"index_digest": DIGEST}}
            ).encode(),
            f"{receipt}.bundle": b"receipt signature bundle",
        }
        for name in (archive, receipt):
            checksum = hashlib.sha256(self.contents[name]).hexdigest()
            self.contents[f"{name}.sha256"] = f"{checksum}  {name}\n".encode()
        self.commands: list[list[str]] = []
        self.metadata_sizes = {name: len(value) for name, value in self.contents.items()}

    def fake_run(
        self,
        command: list[str],
        *,
        allow_failure: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        del allow_failure
        self.commands.append(command)
        if command[:3] == ["gh", "release", "view"]:
            state = {
                "tagName": TAG,
                "isDraft": False,
                "isPrerelease": False,
                "assets": [
                    {"name": name, "size": size}
                    for name, size in self.metadata_sizes.items()
                ],
            }
            return subprocess.CompletedProcess(command, 0, json.dumps(state), "")
        if command[:3] == ["gh", "release", "download"]:
            destination = Path(command[command.index("--dir") + 1])
            for name, contents in self.contents.items():
                (destination / name).write_bytes(contents)
            return subprocess.CompletedProcess(command, 0, "", "")
        if command[:3] == ["gh", "api", "repos/skitsanos/ironcrew/git/ref/tags/v1.2.3"]:
            return subprocess.CompletedProcess(
                command, 0, json.dumps({"object": {"type": "tag", "sha": TAG_OBJECT}}), ""
            )
        if command[:3] == ["gh", "api", f"repos/skitsanos/ironcrew/git/tags/{TAG_OBJECT}"]:
            return subprocess.CompletedProcess(
                command,
                0,
                json.dumps({"tag": TAG, "object": {"type": "commit", "sha": COMMIT}}),
                "",
            )
        return subprocess.CompletedProcess(command, 0, "", "")

    def install_fake(self) -> None:
        self.backend._run = self.fake_run  # type: ignore[method-assign]

    def test_release_image_checks_metadata_checksum_signatures_and_receipt(self) -> None:
        self.install_fake()
        image = self.backend.release_image(TAG)
        self.assertEqual((image.tag, image.digest), (TAG, DIGEST))
        download = next(command for command in self.commands if command[:3] == ["gh", "release", "download"])
        self.assertEqual(download.count("--pattern"), 6)
        cosign = [command for command in self.commands if command[:2] == ["cosign", "verify-blob"]]
        self.assertEqual(len(cosign), 2)
        identity = (
            "https://github.com/skitsanos/ironcrew/.github/workflows/"
            "release.yml@refs/tags/v1.2.3"
        )
        self.assertTrue(all(identity in command for command in cosign))
        validator = next(
            command
            for command in self.commands
            if any(item.endswith("verify_release_image.py") for item in command)
        )
        self.assertEqual(validator[-2:], ["--tag", TAG])
        command_count = len(self.commands)
        self.assertIs(self.backend.release_image(TAG), image)
        self.assertEqual(len(self.commands), command_count)

    def test_missing_or_oversized_metadata_fails_before_download(self) -> None:
        archive = f"ironcrew-{TAG}-linux-oci.tar"
        cases = [
            lambda: self.metadata_sizes.pop(archive),
            lambda: self.metadata_sizes.__setitem__(archive, ASSET_SIZE_LIMITS["archive"] + 1),
        ]
        for mutate in cases:
            with self.subTest(case=mutate):
                self.setUp()
                mutate()
                self.install_fake()
                with self.assertRaises(PromotionError):
                    self.backend.release_image(TAG)
                self.assertFalse(
                    any(command[:3] == ["gh", "release", "download"] for command in self.commands)
                )

    def test_checksum_or_download_set_mismatch_fails_before_signature_check(self) -> None:
        archive = f"ironcrew-{TAG}-linux-oci.tar"
        self.contents[archive] = b"x" * len(self.contents[archive])
        self.install_fake()
        with self.assertRaisesRegex(PromotionError, "checksum"):
            self.backend.release_image(TAG)
        self.assertFalse(any(command[0] == "cosign" for command in self.commands))

        self.setUp()
        self.contents["unexpected"] = b"file"
        self.install_fake()
        with self.assertRaisesRegex(PromotionError, "incomplete or unexpected"):
            self.backend.release_image(TAG)

    def test_downloaded_size_must_exactly_match_metadata_before_read(self) -> None:
        receipt = f"ironcrew-{TAG}-image-receipt.v1.json"
        self.metadata_sizes[receipt] += 1
        self.install_fake()
        with self.assertRaisesRegex(PromotionError, "size did not match"):
            self.backend.release_image(TAG)
        self.assertFalse(any(command[0] == "cosign" for command in self.commands))

    def test_registry_inspection_distinguishes_absence_from_errors(self) -> None:
        responses = iter([
            subprocess.CompletedProcess([], 1, "", "manifest unknown"),
            subprocess.CompletedProcess([], 1, "", "helper executable not found"),
            subprocess.CompletedProcess([], 1, "", "proxy returned HTTP 404"),
            subprocess.CompletedProcess([], 1, "", "permission denied"),
            subprocess.CompletedProcess([], 0, "not-a-digest\n", ""),
            subprocess.CompletedProcess([], 0, DIGEST + "\n", ""),
        ])

        def run(command: list[str], *, allow_failure: bool = False) -> subprocess.CompletedProcess[str]:
            self.assertTrue(allow_failure)
            self.commands.append(command)
            return next(responses)

        self.backend._run = run  # type: ignore[method-assign]
        self.assertIsNone(self.backend.inspect("1.2.3"))
        with self.assertRaisesRegex(PromotionError, "inspection failed"):
            self.backend.inspect("1.2.3")
        with self.assertRaisesRegex(PromotionError, "inspection failed"):
            self.backend.inspect("1.2.3")
        with self.assertRaisesRegex(PromotionError, "inspection failed"):
            self.backend.inspect("1.2.3")
        with self.assertRaisesRegex(PromotionError, "invalid digest"):
            self.backend.inspect("1.2.3")
        self.assertEqual(self.backend.inspect("1.2.3"), DIGEST)

    def test_copy_uses_archive_all_platforms_and_preserves_digests(self) -> None:
        self.install_fake()
        image = self.backend.release_image(TAG)
        self.commands.clear()
        self.assertTrue(self.backend.copy(image, "1.2.3"))
        self.assertEqual(
            self.commands,
            [[
                "skopeo", "copy", "--quiet", "--all", "--preserve-digests",
                f"oci-archive:{image.archive}", "docker://skitsanos/ironcrew:1.2.3",
            ]],
        )

    def test_command_output_is_bounded_before_parsing(self) -> None:
        def oversized(_command, **options):
            options["stdout"].write(b"x" * (COMMAND_OUTPUT_MAX_BYTES + 1))
            return subprocess.CompletedProcess([], 0)

        with mock.patch("subprocess.run", side_effect=oversized):
            with self.assertRaisesRegex(PromotionError, "output exceeded"):
                self.backend._run(["gh", "api", "fixture"])

    def test_tag_commit_rejects_lightweight_nested_mismatch_and_invalid_json(self) -> None:
        responses = iter(
            [
                {"object": {"type": "commit", "sha": COMMIT}},
                {"object": {"type": "tag", "sha": TAG_OBJECT}},
                {"tag": TAG, "object": {"type": "tag", "sha": TAG_OBJECT}},
                {"object": {"type": "tag", "sha": TAG_OBJECT}},
                {"tag": "v9.9.9", "object": {"type": "commit", "sha": COMMIT}},
            ]
        )

        def run_json(command: list[str], *, allow_failure: bool = False) -> subprocess.CompletedProcess[str]:
            del allow_failure
            return subprocess.CompletedProcess(command, 0, json.dumps(next(responses)), "")

        self.backend._run = run_json  # type: ignore[method-assign]
        with self.assertRaisesRegex(PromotionError, "annotated tag object"):
            self.backend.tag_commit(TAG)
        with self.assertRaisesRegex(PromotionError, "directly to one commit"):
            self.backend.tag_commit(TAG)
        with self.assertRaisesRegex(PromotionError, "directly to one commit"):
            self.backend.tag_commit(TAG)

        self.backend._run = lambda command, allow_failure=False: subprocess.CompletedProcess(  # type: ignore[method-assign]
            command, 0, "not-json", ""
        )
        with self.assertRaisesRegex(PromotionError, "metadata was invalid"):
            self.backend.tag_commit(TAG)


if __name__ == "__main__":
    unittest.main()
