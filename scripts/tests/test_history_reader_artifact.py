"""Negative acceptance for immutable CI artifact identity and extraction boundaries."""
import io
import gzip
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import history_reader_artifact as artifact
import check_history_reader_artifact as probe


class HistoryArtifactTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="ic-artifact-policy-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.binary = self.root / "binary"
        data = bytearray(128)
        data[:6], data[18:20] = b"\x7fELF\x02\x01", b"\x3e\x00"
        self.binary.write_bytes(data)
        self.binding = artifact.identity("a" * 40, "skitsanos/ironcrew", "123", "1")
        self.bundle = self.root / "bundle"
        self.receipt, self.sha = artifact.package(self.binary, self.bundle, self.binding, 100)

    def verify(self, binding=None, sha=None):
        return artifact.verify(self.bundle, self.root / "extracted", binding or self.binding,
                               sha or self.sha)

    def rewrite_receipt(self):
        raw = (json.dumps(self.receipt, sort_keys=True, indent=2) + "\n").encode()
        self.sha = artifact.digest(raw)
        (self.bundle / "artifact.json").write_bytes(raw)
        (self.bundle / "checksums.txt").write_text(
            f"{self.receipt['archive_sha256']}  {artifact.ARCHIVE}\n{self.sha}  artifact.json\n")

    def test_exact_archive_roundtrip_is_bound_and_reproducible(self):
        self.assertEqual(self.verify(), self.receipt)
        self.assertEqual((self.root / "extracted/ironcrew").read_bytes(), self.binary.read_bytes())
        self.assertEqual((self.root / "extracted/ironcrew").stat().st_mode & 0o777, 0o700)
        other = self.root / "other"
        self.assertEqual(artifact.package(self.binary, other, self.binding, 100),
                         (self.receipt, self.sha))
        self.assertEqual((other / artifact.ARCHIVE).read_bytes(),
                         (self.bundle / artifact.ARCHIVE).read_bytes())

    def test_source_repository_run_attempt_and_target_cannot_be_substituted(self):
        for key in self.binding:
            changed = dict(self.binding, **{key: "wrong"})
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "identity"):
                self.verify(changed)
        self.assertFalse((self.root / "extracted").exists())

    def test_corruption_and_receipt_replacement_fail_before_extraction(self):
        with self.assertRaisesRegex(ValueError, "Receipt digest"):
            self.verify(sha="b" * 64)
        path = self.bundle / artifact.ARCHIVE
        path.write_bytes(path.read_bytes() + b"corrupt")
        with self.assertRaisesRegex(ValueError, "Archive digest"):
            self.verify()
        self.assertFalse((self.root / "extracted").exists())

    def test_binary_hash_and_declared_size_are_checked(self):
        self.receipt["binary_sha256"] = "c" * 64
        self.rewrite_receipt()
        with self.assertRaisesRegex(ValueError, "Executable digest"):
            self.verify()
        self.receipt["binary_bytes"] = True
        self.rewrite_receipt()
        with self.assertRaisesRegex(ValueError, "declared executable size"):
            self.verify()

    def test_links_paths_and_extra_archive_members_are_rejected(self):
        for kind in ["path", "symlink", "extra"]:
            with self.subTest(kind=kind):
                with tarfile.open(self.bundle / artifact.ARCHIVE, "w:gz") as archive:
                    member = tarfile.TarInfo("../escape" if kind == "path" else "ironcrew")
                    member.size, member.mode = 128, 0o755
                    if kind == "symlink":
                        member.type, member.linkname, member.size = tarfile.SYMTYPE, "/etc/passwd", 0
                        archive.addfile(member)
                    else:
                        archive.addfile(member, io.BytesIO(self.binary.read_bytes()))
                    if kind == "extra":
                        archive.addfile(tarfile.TarInfo("extra"))
                self.receipt["archive_sha256"] = artifact.digest(
                    (self.bundle / artifact.ARCHIVE).read_bytes())
                self.rewrite_receipt()
                with self.assertRaises(ValueError):
                    self.verify()
                self.assertFalse((self.root / "escape").exists())

    def test_wrong_architecture_invalid_identity_and_checksum_manifest_fail(self):
        with self.assertRaisesRegex(ValueError, "ELF"):
            artifact.native_binary(b"not an ELF" * 10)
        with self.assertRaises(ValueError):
            artifact.identity("bad", "skitsanos/ironcrew", "123", "1")
        (self.bundle / "checksums.txt").write_text("forged")
        with self.assertRaisesRegex(ValueError, "Checksum manifest"):
            self.verify()

    def test_oversized_binary_receipt_and_checksum_files_fail_before_materialization(self):
        with self.binary.open("r+b") as stream:
            stream.truncate(artifact.MAX_BYTES + 1)
        with self.assertRaisesRegex(ValueError, "size limit"):
            artifact.package(self.binary, self.root / "oversized", self.binding, 100)
        self.assertFalse((self.root / "oversized").exists())
        (self.bundle / "artifact.json").write_bytes(b" " * 4097)
        with self.assertRaisesRegex(ValueError, "size limit"):
            self.verify()
        self.rewrite_receipt()
        (self.bundle / "checksums.txt").write_bytes(b" " * 513)
        with self.assertRaisesRegex(ValueError, "size limit"):
            self.verify()

    def test_gnu_and_pax_metadata_cannot_request_unbounded_expansion(self):
        for kind in [tarfile.GNUTYPE_LONGNAME, tarfile.XHDTYPE, tarfile.XGLTYPE]:
            with self.subTest(kind=kind):
                header = tarfile.TarInfo("ironcrew")
                header.type, header.size = kind, 1024 * 1024 * 1024
                raw = gzip.compress(header.tobuf(format=tarfile.USTAR_FORMAT) + b"\0" * 1024)
                (self.bundle / artifact.ARCHIVE).write_bytes(raw)
                self.receipt["archive_sha256"] = artifact.digest(raw)
                self.rewrite_receipt()
                with self.assertRaises(ValueError):
                    self.verify()
                self.assertFalse((self.root / "extracted").exists())

    def test_fixture_distinguishes_unknown_zero_and_invalid_usage(self):
        rows = probe.fixtures()
        self.assertNotIn("usage", rows[0][2][0])
        self.assertEqual(rows[0][3]["coverage"], "unavailable")
        self.assertEqual(rows[1][2][0]["usage"]["settled"]["total_tokens"]["known"], "0")
        self.assertIsNone(rows[3][2][0]["usage"])
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            artifact.unique_object([("commit", "a"), ("commit", "b")])


if __name__ == "__main__":
    unittest.main()
