import contextlib
import io
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import pre_push_tempdir as isolation  # noqa: E402


class TemporaryIsolationFailureTests(unittest.TestCase):
    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix="ironcrew-tempdir-test-")
        self.addCleanup(self.fixture.cleanup)
        self.root = Path(self.fixture.name).resolve()
        self.repository = self.root / "checkout"
        self.base = self.root / "temporary"
        self.repository.mkdir()
        self.base.mkdir()

    def run_admission(self, tmpdir):
        stdout, stderr = io.StringIO(), io.StringIO()
        environment = {"TEMP": str(self.root), "TMP": str(self.root)}
        if tmpdir is not None:
            environment["TMPDIR"] = str(tmpdir)
        with (
            patch.dict(os.environ, environment, clear=True),
            patch.object(sys, "argv", ["pre_push_tempdir.py", str(self.repository)]),
            contextlib.redirect_stdout(stdout),
            contextlib.redirect_stderr(stderr),
        ):
            code = isolation.main()
        return code, stdout.getvalue(), stderr.getvalue()

    def test_allocation_failure_has_no_fallback_or_sensitive_error_output(self):
        with patch.object(isolation.tempfile, "mkdtemp", side_effect=PermissionError("private-detail")) as allocate:
            code, stdout, stderr = self.run_admission(self.base)
        self.assertEqual(code, 1)
        self.assertEqual(stdout, "")
        self.assertIn("no fallback was attempted", stderr)
        self.assertNotIn("private-detail", stderr)
        self.assertEqual(allocate.call_count, 1)
        self.assertEqual(allocate.call_args.kwargs["dir"], self.base)
        self.assertEqual(list(self.base.iterdir()), [])

    def test_uninspectable_ancestry_stops_before_allocation(self):
        original_lstat = Path.lstat

        def deny_dotenv(path, *args, **kwargs):
            if path == self.base / ".env":
                raise PermissionError("private-detail")
            return original_lstat(path, *args, **kwargs)

        with (
            patch.object(Path, "lstat", deny_dotenv),
            patch.object(isolation.tempfile, "mkdtemp") as allocate,
        ):
            code, stdout, stderr = self.run_admission(self.base)
        self.assertEqual(code, 1)
        self.assertEqual(stdout, "")
        self.assertIn("temporary-directory isolation failed", stderr)
        self.assertNotIn("private-detail", stderr)
        allocate.assert_not_called()

    def test_failed_post_allocation_check_removes_only_its_owned_root(self):
        original_allocate = tempfile.mkdtemp
        keep = self.base / "keep"
        keep.write_text("caller-owned sibling")

        def contaminate_after_allocation(*args, **kwargs):
            scratch = original_allocate(*args, **kwargs)
            (self.base / ".env").write_text("SYNTHETIC_TEST_ONLY=never-read")
            return scratch

        with patch.object(isolation.tempfile, "mkdtemp", contaminate_after_allocation):
            code, stdout, stderr = self.run_admission(self.base)
        self.assertEqual(code, 1)
        self.assertEqual(stdout, "")
        self.assertIn(".env entry", stderr)
        self.assertNotIn("SYNTHETIC_TEST_ONLY", stderr)
        self.assertEqual({path.name for path in self.base.iterdir()}, {".env", "keep"})
        self.assertEqual(keep.read_text(), "caller-owned sibling")

    def test_unset_and_empty_tmpdir_select_only_the_validated_default(self):
        for tmpdir in (None, ""):
            with self.subTest(tmpdir=tmpdir), patch.object(
                isolation, "create_temporary_root", side_effect=isolation.IsolationError("blocked default")
            ) as create:
                code, stdout, stderr = self.run_admission(tmpdir)
            self.assertEqual(code, 1)
            self.assertEqual(stdout, "")
            self.assertIn("blocked default", stderr)
            create.assert_called_once_with(self.repository, "/tmp")


if __name__ == "__main__":
    unittest.main()
