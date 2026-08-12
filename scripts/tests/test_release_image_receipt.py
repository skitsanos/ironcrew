import argparse
import hashlib
import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from release_image_receipt import (  # noqa: E402
    MAX_ARCHIVE_BYTES,
    MAX_ARCHIVE_MEMBERS,
    ReceiptError,
    archive_data,
    generate,
    validate,
)
from verify_release_bindings import verify as verify_bindings  # noqa: E402


def encoded(value):
    return json.dumps(value, separators=(",", ":")).encode()


def digest(value):
    return "sha256:" + hashlib.sha256(value).hexdigest()


class ReleaseImageReceiptTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.archive = self.root / "ironcrew-v1.2.3-linux-oci.tar"
        self.receipt = self.root / "ironcrew-v1.2.3-image-receipt.v1.json"
        self.dockerfile = self.root / "runtime.Dockerfile"
        self.dockerfile.write_text(
            "FROM example.invalid/base@sha256:" + "a" * 64 + "\n", encoding="utf-8"
        )
        self.amd64 = self.root / "amd64.tar.gz"
        self.arm64 = self.root / "arm64.tar.gz"
        self.amd64.write_bytes(b"amd64")
        self.arm64.write_bytes(b"arm64")
        self._write_oci_archive()

    def tearDown(self):
        self.temporary.cleanup()

    def _write_oci_archive(self):
        blobs = {}
        descriptors = []
        for architecture in ("amd64", "arm64"):
            config = encoded({"architecture": architecture, "os": "linux"})
            config_digest = digest(config)
            blobs[config_digest] = config
            manifest = encoded(
                {
                    "schemaVersion": 2,
                    "config": {
                        "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": config_digest,
                        "size": len(config),
                    },
                    "layers": [],
                }
            )
            manifest_digest = digest(manifest)
            blobs[manifest_digest] = manifest
            descriptors.append(
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": manifest_digest,
                    "size": len(manifest),
                    "platform": {"architecture": architecture, "os": "linux"},
                }
            )
        image_index = encoded({"schemaVersion": 2, "manifests": descriptors})
        image_index_digest = digest(image_index)
        blobs[image_index_digest] = image_index
        layout_index = encoded(
            {
                "schemaVersion": 2,
                "manifests": [
                    {
                        "mediaType": "application/vnd.oci.image.index.v1+json",
                        "digest": image_index_digest,
                        "size": len(image_index),
                    }
                ],
            }
        )
        with tarfile.open(self.archive, "w") as archive:
            for name, content in {
                "oci-layout": encoded({"imageLayoutVersion": "1.0.0"}),
                "index.json": layout_index,
                **{f"blobs/sha256/{key[7:]}": value for key, value in blobs.items()},
            }.items():
                info = tarfile.TarInfo(name)
                info.size = len(content)
                archive.addfile(info, __import__("io").BytesIO(content))

    def _arguments(self):
        return argparse.Namespace(
            tag="v1.2.3",
            commit_sha="1" * 40,
            source_date_epoch=1_700_000_000,
            archive=self.archive,
            receipt=self.receipt,
            dockerfile=self.dockerfile,
            base_reference="example.invalid/base",
            base_digest="sha256:" + "a" * 64,
            builder_implementation="test-builder",
            builder_version="1.0",
            binary=[
                ("linux/amd64", "ironcrew-linux-amd64.tar.gz", self.amd64),
                ("linux/arm64", "ironcrew-linux-arm64.tar.gz", self.arm64),
            ],
        )

    def test_generates_and_revalidates_archive_content(self):
        generate(self._arguments())
        receipt = json.loads(self.receipt.read_text(encoding="utf-8"))
        validate(receipt, self.receipt, self.archive, "v1.2.3")
        self.assertEqual(receipt["platforms"], ["linux/amd64", "linux/arm64"])
        self.assertRegex(receipt["oci_archive"]["index_digest"], r"^sha256:[0-9a-f]{64}$")

    def test_rejects_unknown_receipt_fields(self):
        generate(self._arguments())
        receipt = json.loads(self.receipt.read_text(encoding="utf-8"))
        receipt["unexpected"] = True
        with self.assertRaisesRegex(ReceiptError, "must contain exactly"):
            validate(receipt, self.receipt, self.archive, "v1.2.3")

    def test_rejects_boolean_integer_fields(self):
        generate(self._arguments())
        receipt = json.loads(self.receipt.read_text(encoding="utf-8"))
        receipt["schema_version"] = True
        with self.assertRaisesRegex(ReceiptError, "unsupported receipt schema"):
            validate(receipt, self.receipt, self.archive, "v1.2.3")
        receipt["schema_version"] = 1
        receipt["source_date_epoch"] = True
        with self.assertRaisesRegex(ReceiptError, "positive integer"):
            validate(receipt, self.receipt, self.archive, "v1.2.3")

    def test_rejects_archive_mutation(self):
        generate(self._arguments())
        receipt = json.loads(self.receipt.read_text(encoding="utf-8"))
        with self.archive.open("ab") as archive:
            archive.write(b"changed")
        with self.assertRaisesRegex(ReceiptError, "archive SHA-256 mismatch"):
            validate(receipt, self.receipt, self.archive, "v1.2.3")

    def test_rejects_archive_over_byte_limit_before_opening_tar(self):
        with self.archive.open("r+b") as archive:
            archive.truncate(MAX_ARCHIVE_BYTES + 1)
        with self.assertRaisesRegex(ReceiptError, "archive exceeds"):
            archive_data(self.archive)

    def test_rejects_tar_member_fanout_before_storing_more_metadata(self):
        with tarfile.open(self.archive, "w") as archive:
            for index in range(MAX_ARCHIVE_MEMBERS + 1):
                archive.addfile(tarfile.TarInfo(f"empty-{index}"))
        with self.assertRaisesRegex(ReceiptError, "archive exceeds.*members"):
            archive_data(self.archive)

    def test_rejects_tag_and_commit_shape_drift(self):
        generate(self._arguments())
        receipt = json.loads(self.receipt.read_text(encoding="utf-8"))
        with self.assertRaisesRegex(ReceiptError, "does not match expected tag"):
            validate(receipt, self.receipt, self.archive, "v1.2.4")
        receipt["commit_sha"] = "A" * 40
        with self.assertRaisesRegex(ReceiptError, "Git object ID"):
            validate(receipt, self.receipt, self.archive, "v1.2.3")

    def test_rejects_moving_or_mismatched_dockerfile_base(self):
        arguments = self._arguments()
        self.dockerfile.write_text("FROM example.invalid/base:latest\n", encoding="utf-8")
        with self.assertRaisesRegex(ReceiptError, "digest-pinned base"):
            generate(arguments)

    def test_final_verification_binds_source_commit_epoch_and_binaries(self):
        arguments = self._arguments()
        generate(arguments)
        receipt = json.loads(self.receipt.read_text(encoding="utf-8"))
        options = {
            "commit_sha": arguments.commit_sha,
            "source_date_epoch": arguments.source_date_epoch,
            "dockerfile": self.dockerfile,
            "binary_paths": arguments.binary,
        }
        verify_bindings(self.receipt, **options)

        for key, value, message in (
            ("commit_sha", "2" * 40, "authorized tag commit"),
            ("source_date_epoch", 1, "epoch"),
        ):
            changed = dict(options)
            changed[key] = value
            with self.subTest(key=key), self.assertRaisesRegex(ReceiptError, message):
                verify_bindings(self.receipt, **changed)

        self.amd64.write_bytes(b"tampered")
        with self.assertRaisesRegex(ReceiptError, "binary SHA-256 mismatch"):
            verify_bindings(self.receipt, **options)
        self.amd64.write_bytes(b"amd64")
        self.dockerfile.write_text(
            "FROM example.invalid/base@sha256:" + "b" * 64 + "\n", encoding="utf-8"
        )
        with self.assertRaisesRegex(ReceiptError, "digest-pinned base"):
            verify_bindings(self.receipt, **options)

    def test_accepts_sha1_and_sha256_commit_ids_only(self):
        arguments = self._arguments()
        arguments.commit_sha = "2" * 64
        generate(arguments)
        arguments.commit_sha = "3" * 41
        with self.assertRaisesRegex(ReceiptError, "full lowercase commit SHA"):
            generate(arguments)


if __name__ == "__main__":
    unittest.main()
