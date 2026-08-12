from __future__ import annotations

import importlib.util
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_module_size.py"
SPEC = importlib.util.spec_from_file_location("check_module_size", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
CHECKER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHECKER
SPEC.loader.exec_module(CHECKER)


class ModuleSizeCheckerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "src").mkdir()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_module(self, relative: str, lines: int) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"// physical line\n" * lines)
        return path

    def write_policy(
        self,
        exceptions: list[dict[str, object]],
        *,
        budget: int | None = None,
        version: object = 1,
    ) -> Path:
        policy = self.root / "policy.json"
        policy.write_text(
            json.dumps(
                {
                    "version": version,
                    "exception_budget": len(exceptions) if budget is None else budget,
                    "exceptions": exceptions,
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        return policy

    def check(self, exceptions: list[dict[str, object]]) -> tuple[int, str]:
        output = io.StringIO()
        result = CHECKER.run_check(
            self.root,
            self.write_policy(exceptions),
            output,
            maximum_budget=len(exceptions),
        )
        return result, output.getvalue()

    @staticmethod
    def exception(path: str, max_lines: object) -> dict[str, object]:
        return {
            "path": path,
            "max_lines": max_lines,
            "rationale": "Reviewed cohesive responsibility with an exact legacy ceiling.",
        }

    def test_clean_report_is_deterministic_bounded_and_explains_review(self) -> None:
        self.write_module("src/alpha.rs", 300)
        self.write_module("src/beta.rs", 300)
        for index in range(19):
            self.write_module(f"src/module_{index:02}.rs", index + 1)

        result, output = self.check([])

        self.assertEqual(result, 0)
        self.assertLess(output.index("src/alpha.rs"), output.index("src/beta.rs"))
        self.assertEqual(output.count("src/"), CHECKER.REPORT_COUNT)
        self.assertNotIn("src/module_00.rs", output)
        self.assertIn("responsibility boundaries", output)
        self.assertIn("cognitive complexity", output)

    def test_unlisted_target_and_new_module_ceiling_violations_are_distinct(self) -> None:
        self.write_module("src/above_target.rs", 301)
        self.write_module("src/above_ceiling.rs", 401)

        result, output = self.check([])

        self.assertEqual(result, 1)
        self.assertIn("exceeds the 300-line target", output)
        self.assertIn("violates the 400-line new-module ceiling", output)
        self.assertLess(output.index("src/above_ceiling.rs:"), output.index("src/above_target.rs:"))

    def test_exact_legacy_ceiling_can_exceed_new_module_ceiling(self) -> None:
        self.write_module("src/legacy.rs", 501)

        result, output = self.check([self.exception("src/legacy.rs", 501)])

        self.assertEqual(result, 0)
        self.assertIn("[reviewed ceiling: 501]", output)

    def test_growth_and_shrink_both_require_policy_updates(self) -> None:
        module = self.write_module("src/legacy.rs", 502)
        exception = self.exception("src/legacy.rs", 501)

        result, output = self.check([exception])
        self.assertEqual(result, 1)
        self.assertIn("grew to 502 lines above its reviewed 501-line ceiling", output)

        module.write_bytes(b"// line\n" * 500)
        result, output = self.check([exception])
        self.assertEqual(result, 1)
        self.assertIn("lower its reviewed 501-line ceiling", output)

    def test_target_boundary_passes_and_now_small_exception_is_stale(self) -> None:
        self.write_module("src/target.rs", 300)
        result, _ = self.check([])
        self.assertEqual(result, 0)

        result, output = self.check([self.exception("src/target.rs", 301)])
        self.assertEqual(result, 1)
        self.assertIn("remove its stale exception", output)

    def test_missing_exception_is_stale_and_tests_directory_is_not_scanned(self) -> None:
        self.write_module("src/small.rs", 10)
        self.write_module("tests/large_fixture.rs", 900)

        result, output = self.check([self.exception("src/missing.rs", 320)])

        self.assertEqual(result, 1)
        self.assertIn("exception is stale because the module does not exist", output)
        self.assertNotIn("large_fixture.rs", output)

    def test_physical_line_count_handles_crlf_unterminated_and_empty_files(self) -> None:
        (self.root / "src" / "unterminated.rs").write_bytes(b"first\nsecond")
        (self.root / "src" / "crlf.rs").write_bytes(b"first\r\nsecond\r\n")
        (self.root / "src" / "empty.rs").write_bytes(b"")

        sizes = {item.path: item.lines for item in CHECKER.scan_modules(self.root)}

        self.assertEqual(sizes["src/unterminated.rs"], 2)
        self.assertEqual(sizes["src/crlf.rs"], 2)
        self.assertEqual(sizes["src/empty.rs"], 0)
        self.assertEqual(CHECKER.count_physical_lines(b"one\ntwo\n"), 2)

    @unittest.skipIf(os.name == "nt", "symlink creation requires Unix privileges")
    def test_file_directory_and_source_root_symlinks_are_rejected(self) -> None:
        outside = self.root / "outside.rs"
        outside.write_text("// outside\n", encoding="utf-8")
        link = self.root / "src" / "linked.rs"
        link.symlink_to(outside)
        with self.assertRaisesRegex(CHECKER.PolicyError, "symlinks are not allowed"):
            CHECKER.scan_modules(self.root)

        link.unlink()
        outside_directory = self.root / "outside"
        outside_directory.mkdir()
        (self.root / "src" / "linked_directory").symlink_to(
            outside_directory, target_is_directory=True
        )
        with self.assertRaisesRegex(CHECKER.PolicyError, "symlinks are not allowed"):
            CHECKER.scan_modules(self.root)

        (self.root / "src" / "linked_directory").unlink()
        source = self.root / "src"
        real_source = self.root / "real-source"
        source.rename(real_source)
        source.symlink_to(real_source, target_is_directory=True)
        with self.assertRaisesRegex(CHECKER.PolicyError, "regular directory"):
            CHECKER.scan_modules(self.root)

    def test_invalid_paths_and_nonpositive_or_target_ceilings_are_rejected(self) -> None:
        invalid_paths = [
            "../src/module.rs",
            "/src/module.rs",
            "src/../module.rs",
            "src//module.rs",
            "src\\module.rs",
            "tests/module.rs",
            "src/module.txt",
        ]
        for path in invalid_paths:
            with self.subTest(path=path), self.assertRaisesRegex(
                CHECKER.PolicyError, "normalized src/"
            ):
                CHECKER.load_policy(
                    self.write_policy([self.exception(path, 320)]), 1
                )

        for ceiling in [0, -1, 300, True, 301.0]:
            with self.subTest(ceiling=ceiling), self.assertRaisesRegex(
                CHECKER.PolicyError, "integer greater than 300"
            ):
                CHECKER.load_policy(
                    self.write_policy([self.exception("src/module.rs", ceiling)]), 1
                )

    def test_schema_version_rationale_and_malformed_json_are_rejected(self) -> None:
        exception = self.exception("src/module.rs", 320)
        exception["rationale"] = "TBD"
        with self.assertRaisesRegex(CHECKER.PolicyError, "rationale"):
            CHECKER.load_policy(self.write_policy([exception]), 1)

        with self.assertRaisesRegex(CHECKER.PolicyError, "unsupported"):
            CHECKER.load_policy(self.write_policy([], version=True), 0)

        policy = self.write_policy([])
        policy.write_text("{", encoding="utf-8")
        with self.assertRaisesRegex(CHECKER.PolicyError, "cannot read"):
            CHECKER.load_policy(policy, 0)

    def test_duplicate_json_keys_entry_paths_and_unsorted_paths_are_rejected(self) -> None:
        policy = self.root / "policy.json"
        policy.write_text(
            '{"version":1,"version":1,"exception_budget":0,"exceptions":[]}',
            encoding="utf-8",
        )
        with self.assertRaisesRegex(CHECKER.PolicyError, "duplicate JSON key: version"):
            CHECKER.load_policy(policy, 0)

        duplicate = self.exception("src/a.rs", 320)
        with self.assertRaisesRegex(CHECKER.PolicyError, "duplicate exception path"):
            CHECKER.load_policy(self.write_policy([duplicate, duplicate]), 2)

        entries = [self.exception("src/z.rs", 320), self.exception("src/a.rs", 320)]
        with self.assertRaisesRegex(CHECKER.PolicyError, "sorted by path"):
            CHECKER.load_policy(self.write_policy(entries), 2)

    def test_budget_has_no_slack_and_must_match_entries(self) -> None:
        baseline = CHECKER.MAX_EXCEPTION_BUDGET
        entries = [
            self.exception(f"src/module_{index:02}.rs", 301)
            for index in range(baseline)
        ]
        policy = self.write_policy(entries)
        self.assertEqual(CHECKER.load_policy(policy).exception_budget, baseline)

        with self.assertRaisesRegex(CHECKER.PolicyError, "fixed reviewed cap"):
            CHECKER.load_policy(self.write_policy(entries[:-1]))
        with self.assertRaisesRegex(CHECKER.PolicyError, "fixed reviewed cap"):
            CHECKER.load_policy(self.write_policy(entries + [self.exception("src/z.rs", 301)]))

        with self.assertRaisesRegex(CHECKER.PolicyError, "has 1 exceptions"):
            CHECKER.load_policy(
                self.write_policy([self.exception("src/a.rs", 301)], budget=2), 2
            )


if __name__ == "__main__":
    unittest.main()
