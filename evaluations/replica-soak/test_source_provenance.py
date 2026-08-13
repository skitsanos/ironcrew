import hashlib
import json
import subprocess
import sys
import unittest
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))
from source_provenance import safe_binary_path, worktree_provenance  # noqa: E402


class SourceProvenanceTests(unittest.TestCase):
    def test_binary_paths_are_repository_relative_or_basename_only(self) -> None:
        with __import__("tempfile").TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            local = root / "target" / "release" / "ironcrew"
            local.parent.mkdir(parents=True)
            local.write_bytes(b"local")
            self.assertEqual(
                safe_binary_path(root, local),
                ("target/release/ironcrew", "repository_relative"),
            )

            with __import__("tempfile").TemporaryDirectory() as external_directory:
                external = Path(external_directory) / "external-ironcrew"
                external.write_bytes(b"external")
                self.assertEqual(
                    safe_binary_path(root, external),
                    ("external-ironcrew", "external_basename"),
                )

    def test_manifest_binds_tracked_untracked_and_deleted_paths(self) -> None:
        with __import__("tempfile").TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(
                ["git", "config", "user.email", "test@example.invalid"],
                cwd=root,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.name", "Replica Soak Test"],
                cwd=root,
                check=True,
            )
            (root / "changed.txt").write_text("old\n", encoding="utf-8")
            (root / "deleted.txt").write_text("gone\n", encoding="utf-8")
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "fixture"], cwd=root, check=True)
            (root / "changed.txt").write_text("new\n", encoding="utf-8")
            (root / "deleted.txt").unlink()
            (root / "untracked.txt").write_text("fresh\n", encoding="utf-8")

            first = worktree_provenance(root)
            second = worktree_provenance(root)

            self.assertEqual(first, second)
            self.assertTrue(first["dirty"])
            self.assertEqual(first["changed_path_count"], 3)
            entries = {entry["path"]: entry for entry in first["changed_paths"]}
            self.assertEqual(entries["deleted.txt"], {"path": "deleted.txt", "state": "deleted"})
            self.assertEqual(entries["changed.txt"]["sha256"], hashlib.sha256(b"new\n").hexdigest())
            json.dumps(first)


if __name__ == "__main__":
    unittest.main()
