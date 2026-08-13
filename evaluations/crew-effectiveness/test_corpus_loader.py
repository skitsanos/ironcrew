from __future__ import annotations

import hashlib
import json
import shutil
import tempfile
import unittest
from pathlib import Path

import corpus_loader
import evaluate


BASE_DIR = Path(__file__).resolve().parent
MANIFESTS = sorted((BASE_DIR / "domain-packs").glob("*/manifest.v1.json"))


def load(manifests: list[Path] | None = None) -> corpus_loader.LoadedCorpus:
    return corpus_loader.load_corpus(
        base_cases_path=BASE_DIR / "cases.v1.jsonl",
        base_oracle_path=BASE_DIR / "oracle.v1.jsonl",
        manifest_paths=MANIFESTS if manifests is None else manifests,
        validate_dataset=evaluate.validate_dataset,
    )


class CorpusLoaderTests(unittest.TestCase):
    def test_loads_base_and_frozen_domain_packs_without_prompt_metadata(self) -> None:
        corpus = load()

        self.assertEqual(len(corpus.cases), 12)
        self.assertEqual(set(corpus.oracle_by_id), {case["case_id"] for case in corpus.cases})
        self.assertEqual(
            {item["pack_id"] for item in corpus.receipt["packs"]},
            {"synthetic-core-v1", "software-delivery", "security-operations"},
        )
        self.assertEqual(
            {corpus.case_pack_ids[case["case_id"]] for case in corpus.cases},
            {"synthetic-core-v1", "software-delivery", "security-operations"},
        )
        self.assertTrue(all(set(case) == {"case_id", "evidence", "questions"} for case in corpus.cases))
        self.assertRegex(corpus.receipt["aggregate_sha256"], r"^[a-f0-9]{64}$")

    def test_manifest_order_does_not_change_aggregate_identity(self) -> None:
        forward = load(MANIFESTS)
        reverse = load(list(reversed(MANIFESTS)))

        self.assertEqual(forward.receipt["aggregate_sha256"], reverse.receipt["aggregate_sha256"])
        self.assertEqual(forward.cases, reverse.cases)

    def test_rejects_manifest_file_hash_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            pack = Path(temporary) / "pack"
            shutil.copytree(MANIFESTS[0].parent, pack)
            (pack / "cases.v1.jsonl").write_text("{}\n", encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "cases SHA-256 mismatch"):
                load([pack / "manifest.v1.json"])

    def test_rejects_duplicate_case_ids_across_packs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            pack = Path(temporary) / "pack"
            shutil.copytree(MANIFESTS[0].parent, pack)
            manifest_path = pack / "manifest.v1.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["pack_id"] = "duplicate-case-pack"
            base_case_id = evaluate.load_jsonl(BASE_DIR / "cases.v1.jsonl")[0]["case_id"]
            for filename in ("cases.v1.jsonl", "oracle.v1.jsonl"):
                path = pack / filename
                records = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
                records[0]["case_id"] = base_case_id
                path.write_text(
                    "".join(json.dumps(record, sort_keys=True) + "\n" for record in records),
                    encoding="utf-8",
                )
                digest = hashlib.sha256(path.read_bytes()).hexdigest()
                key = "cases" if filename.startswith("cases") else "oracle"
                manifest["files"][key]["sha256"] = digest
            manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

            with self.assertRaisesRegex(ValueError, f"duplicate corpus case_id {base_case_id}"):
                load([manifest_path])

    def test_rejects_unreviewed_or_path_ambiguous_manifest(self) -> None:
        for mutate, expected in (
            (
                lambda manifest: manifest["independent_review"].__setitem__(
                    "result", "pending"
                ),
                "must have passed",
            ),
            (
                lambda manifest: manifest["files"]["cases"].__setitem__(
                    "path", "../cases.v1.jsonl"
                ),
                "cases.path must be cases.v1.jsonl",
            ),
        ):
            with self.subTest(expected=expected), tempfile.TemporaryDirectory() as temporary:
                pack = Path(temporary) / "pack"
                shutil.copytree(MANIFESTS[0].parent, pack)
                manifest_path = pack / "manifest.v1.json"
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                mutate(manifest)
                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

                with self.assertRaisesRegex(ValueError, expected):
                    load([manifest_path])

    def test_rejects_duplicate_pack_identity(self) -> None:
        with self.assertRaisesRegex(ValueError, "duplicate corpus pack_id"):
            load([MANIFESTS[0], MANIFESTS[0]])


if __name__ == "__main__":
    unittest.main()
