import hashlib
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))
from source_provenance import (  # noqa: E402
    MAX_CHANGED_PATHS,
    MAX_PATH_BYTES,
    _paths,
    require_unchanged_provenance,
    safe_binary_path,
    worktree_provenance,
)


def _git(root: Path, *args: str) -> None:
    subprocess.run(["git", *args], cwd=root, check=True, capture_output=True)


def _fixture_repo(root: Path) -> None:
    _git(root, "init", "-q")
    _git(root, "config", "user.email", "test@example.invalid")
    _git(root, "config", "user.name", "Crew Evaluation Test")
    (root / ".gitignore").write_text("*.ignored\n", encoding="utf-8")
    (root / "changed.txt").write_text("old\n", encoding="utf-8")
    (root / "deleted.txt").write_text("gone\n", encoding="utf-8")
    (root / "tracked.ignored").write_text("tracked old\n", encoding="utf-8")
    _git(root, "add", ".gitignore", "changed.txt", "deleted.txt")
    _git(root, "add", "-f", "tracked.ignored")
    _git(root, "commit", "-qm", "fixture")


class SourceProvenanceTests(unittest.TestCase):
    def test_binary_labels_never_expose_an_absolute_path(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            local = root / "target" / "release" / "ironcrew"
            local.parent.mkdir(parents=True)
            local.write_bytes(b"local")
            self.assertEqual(
                safe_binary_path(root, local),
                ("target/release/ironcrew", "repository_relative"),
            )
            with tempfile.TemporaryDirectory() as external_directory:
                external = Path(external_directory) / "external-ironcrew"
                external.write_bytes(b"external")
                self.assertEqual(
                    safe_binary_path(root, external),
                    ("external-ironcrew", "external_basename"),
                )

    def test_manifest_binds_all_tracked_changes_and_nonignored_untracked_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _fixture_repo(root)
            (root / "changed.txt").write_text("new\n", encoding="utf-8")
            (root / "deleted.txt").unlink()
            (root / "tracked.ignored").write_text("tracked new\n", encoding="utf-8")
            (root / "visible.txt").write_text("fresh\n", encoding="utf-8")
            (root / "untracked.ignored").write_text("excluded\n", encoding="utf-8")
            _git(root, "add", "changed.txt")

            first = worktree_provenance(root)
            second = worktree_provenance(root)

            self.assertEqual(first, second)
            self.assertTrue(first["dirty"])
            self.assertEqual(first["tracked_changed_path_count"], 3)
            self.assertEqual(first["untracked_path_count"], 1)
            entries = {entry["path"]: entry for entry in first["changed_paths"]}
            self.assertEqual(
                set(entries), {"changed.txt", "deleted.txt", "tracked.ignored", "visible.txt"}
            )
            self.assertEqual(
                entries["deleted.txt"],
                {"path": "deleted.txt", "source": "tracked", "state": "deleted"},
            )
            self.assertEqual(entries["tracked.ignored"]["source"], "tracked")
            self.assertEqual(
                entries["changed.txt"]["sha256"], hashlib.sha256(b"new\n").hexdigest()
            )
            self.assertNotEqual(first["tracked_diff_sha256"], hashlib.sha256(b"").hexdigest())

    def test_start_and_end_must_be_identical(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _fixture_repo(root)
            (root / "new.txt").write_text("one\n", encoding="utf-8")
            start = worktree_provenance(root)
            require_unchanged_provenance(start, worktree_provenance(root))
            (root / "new.txt").write_text("two\n", encoding="utf-8")
            end = worktree_provenance(root)
            with self.assertRaisesRegex(ValueError, "changed during execution"):
                require_unchanged_provenance(start, end)

    def test_changed_symlink_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _fixture_repo(root)
            target = root / "target.txt"
            target.write_text("target\n", encoding="utf-8")
            link = root / "link.txt"
            try:
                os.symlink(target.name, link)
            except (OSError, NotImplementedError) as error:
                self.skipTest(f"symlinks unavailable: {error}")
            with self.assertRaisesRegex(ValueError, "symlink"):
                worktree_provenance(root)
            with self.assertRaisesRegex(ValueError, "binary path must not be a symlink"):
                safe_binary_path(root, link)

    def test_path_count_length_and_traversal_are_bounded(self) -> None:
        with self.assertRaisesRegex(ValueError, "path exceeds"):
            _paths((b"a" * (MAX_PATH_BYTES + 1)) + b"\0")
        too_many = b"\0".join(
            f"file-{index}".encode() for index in range(MAX_CHANGED_PATHS + 1)
        ) + b"\0"
        with self.assertRaisesRegex(ValueError, "path count exceeds"):
            _paths(too_many)
        with self.assertRaisesRegex(ValueError, "non-relative"):
            _paths(b"../escape\0")

    def test_changed_file_bytes_are_bounded_before_hashing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _fixture_repo(root)
            (root / "large.txt").write_bytes(b"12345")
            with mock.patch("source_provenance.MAX_CHANGED_FILE_BYTES", 4):
                with self.assertRaisesRegex(ValueError, "file exceeds"):
                    worktree_provenance(root)


if __name__ == "__main__":
    unittest.main()
