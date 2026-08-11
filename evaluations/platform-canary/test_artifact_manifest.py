from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from artifact_manifest import (  # noqa: E402
    ManifestError,
    canonical_json,
    create_receipt,
)
from fingerprints import flow_tree_fingerprint  # noqa: E402


def git(repository: Path, *arguments: str) -> str:
    process = subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return process.stdout.strip()


def write(path: Path, body: bytes | str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(body, bytes):
        path.write_bytes(body)
    else:
        path.write_text(body, encoding="utf-8")


def fixture(root: Path) -> tuple[Path, Path, Path, str]:
    repository = root / "repository"
    repository.mkdir()
    files: dict[str, str] = {
        "Cargo.toml": "[package]\nname='fixture'\nversion='0.1.0'\n",
        "Cargo.lock": "# fixture lock\n",
        "Dockerfile": "FROM scratch\nCOPY src /src\nCOPY examples /examples\nCOPY tests /tests\n",
        ".dockerignore": ".env\n.env.build\ndocs/\ntarget/\nexamples/clients\n*.swp\n",
        "src/main.rs": "fn main() {}\n",
        "examples/demo/crew.lua": "return true\n",
        "examples/demo/.env.example": "SAFE_PLACEHOLDER=example\n",
        "tests/smoke.rs": "#[test]\nfn smoke() {}\n",
        "docs/private.txt": "must-not-be-read\n",
        "target/private.bin": "must-not-be-read\n",
        ".env": "SECRET=must-not-be-read\n",
    }
    for name, body in files.items():
        write(repository / name, body)
    write(repository / "examples/demo/.env", "SECRET=nested-must-not-be-read\n")
    write(repository / "examples/clients/private.txt", "must-not-be-read\n")

    git(repository, "init", "-b", "main")
    git(repository, "config", "user.name", "Manifest Test")
    git(repository, "config", "user.email", "manifest-test@invalid.example")
    git(
        repository,
        "add",
        "Cargo.toml",
        "Cargo.lock",
        "Dockerfile",
        ".dockerignore",
        "src",
        "examples/demo/crew.lua",
        "examples/demo/.env.example",
        "tests",
        "docs/private.txt",
        "target/private.bin",
    )
    git(repository, "commit", "-m", "fixture")

    binary = root / "runtime" / "ironcrew"
    write(binary, b"exact-runtime-binary\0")
    flow = root / "flow"
    write(flow / "nested/agent.lua", "return 'agent'\n")
    write(flow / "crew.lua", "return 'crew'\n")
    return repository, binary, flow, flow_tree_fingerprint(flow)


class ArtifactManifestTests(unittest.TestCase):
    def test_inventory_is_sorted_exact_and_canonical(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repository, binary, flow, flow_fingerprint = fixture(Path(raw))
            first = create_receipt(repository, binary, flow, flow_fingerprint)
            second = create_receipt(repository, binary, flow, flow_fingerprint)
            self.assertEqual(first, second)

            manifest = first["manifest"]
            source = manifest["source"]
            paths = [entry["path"] for entry in source["build_inputs"]]
            self.assertEqual(paths, sorted(paths, key=lambda path: path.encode("utf-8")))
            self.assertEqual(
                set(paths),
                {
                    ".dockerignore",
                    "Cargo.lock",
                    "Cargo.toml",
                    "Dockerfile",
                    "examples/demo/.env.example",
                    "examples/demo/crew.lua",
                    "src/main.rs",
                    "tests/smoke.rs",
                },
            )
            encoded = canonical_json(first)
            self.assertNotIn(b"must-not-be-read", encoded)
            self.assertEqual(source["base_commit"], git(repository, "rev-parse", "HEAD"))
            self.assertFalse(source["dirty"])

            artifact = manifest["artifact"]
            expected_binary = hashlib.sha256(binary.read_bytes()).hexdigest()
            self.assertEqual(artifact["binary_fingerprint"], f"sha256:{expected_binary}")
            self.assertEqual(artifact["size"], binary.stat().st_size)
            self.assertEqual(manifest["flow"]["supplied_fingerprint"], flow_fingerprint)
            self.assertEqual(manifest["flow"]["verified_fingerprint"], flow_fingerprint)
            self.assertTrue(manifest["flow"]["verified"])

            inner_digest = hashlib.sha256(canonical_json(manifest)).hexdigest()
            self.assertEqual(first["manifest_sha256"], f"sha256:{inner_digest}")
            self.assertEqual(canonical_json(json.loads(encoded)), encoded)

    def test_dirty_tracks_only_nonignored_build_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repository, binary, flow, flow_fingerprint = fixture(Path(raw))
            write(repository / "docs/private.txt", "changed but excluded\n")
            write(repository / "target/private.bin", "changed but excluded\n")
            write(repository / ".env", "SECRET=changed-but-excluded\n")
            write(repository / "examples/clients/new.txt", "excluded\n")
            clean = create_receipt(repository, binary, flow, flow_fingerprint)
            self.assertFalse(clean["manifest"]["source"]["dirty"])

            write(repository / "src/main.rs", "fn main() { println!(\"changed\"); }\n")
            dirty = create_receipt(repository, binary, flow, flow_fingerprint)
            self.assertTrue(dirty["manifest"]["source"]["dirty"])
            self.assertNotEqual(clean["manifest_sha256"], dirty["manifest_sha256"])

    def test_cli_emits_one_canonical_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repository, binary, flow, fingerprint = fixture(Path(raw))
            process = subprocess.run(
                [
                    sys.executable,
                    str(HERE / "artifact_manifest.py"),
                    "--repository",
                    str(repository),
                    "--binary",
                    str(binary),
                    "--flow-root",
                    str(flow),
                    "--flow-fingerprint",
                    fingerprint,
                ],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            receipt = json.loads(process.stdout)
            self.assertEqual(process.stdout, canonical_json(receipt) + b"\n")
            self.assertEqual(process.stderr, b"")

    def test_flow_mismatch_and_forbidden_roots_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repository, binary, flow, fingerprint = fixture(Path(raw))
            mismatch = "sha256:" + "0" * 64
            self.assertNotEqual(mismatch, fingerprint)
            with self.assertRaisesRegex(ManifestError, "does not match"):
                create_receipt(repository, binary, flow, mismatch)
            with self.assertRaisesRegex(ManifestError, "excluded repository tree"):
                create_receipt(repository, repository / "target/private.bin", flow, fingerprint)
            with self.assertRaisesRegex(ManifestError, "excluded repository tree"):
                create_receipt(repository, binary, repository / "docs", fingerprint)
            with self.assertRaisesRegex(ManifestError, "environment file"):
                create_receipt(repository, repository / ".env", flow, fingerprint)

    @unittest.skipIf(os.name == "nt", "Unix file types are required")
    def test_symlinks_and_special_build_inputs_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repository, binary, flow, fingerprint = fixture(Path(raw))
            link = repository / "src/link.rs"
            link.symlink_to(repository / "src/main.rs")
            with self.assertRaisesRegex(ManifestError, "symlinks"):
                create_receipt(repository, binary, flow, fingerprint)
            link.unlink()

            fifo = repository / "tests/input.fifo"
            os.mkfifo(fifo)
            try:
                with self.assertRaisesRegex(ManifestError, "only regular files"):
                    create_receipt(repository, binary, flow, fingerprint)
            finally:
                fifo.unlink()

    def test_unsupported_dockerignore_rules_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            repository, binary, flow, fingerprint = fixture(Path(raw))
            with (repository / ".dockerignore").open("a", encoding="utf-8") as target:
                target.write("!examples/clients/allowed.txt\n")
            with self.assertRaisesRegex(ManifestError, "negation"):
                create_receipt(repository, binary, flow, fingerprint)


if __name__ == "__main__":
    unittest.main()
