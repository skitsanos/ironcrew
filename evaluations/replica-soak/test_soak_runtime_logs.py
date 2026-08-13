import hashlib
import io
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch


sys.path.insert(0, str(Path(__file__).resolve().parent))
from soak_runtime_logs import (  # noqa: E402
    READ_CHUNK_BYTES,
    ReplicaLauncher,
    StreamingLogCollector,
    runtime_log_criterion,
    validate_canaries,
)
from soak_runtime_directory import verify_runtime_directory  # noqa: E402


class SoakRuntimeLogTests(unittest.TestCase):
    def test_streaming_collector_hashes_all_bytes_and_finds_split_canary(self) -> None:
        secret = "split-secret-canary"
        payload = b"x" * (READ_CHUNK_BYTES - 5) + secret.encode() + b"tail"
        collector = StreamingLogCollector(
            "a", io.BytesIO(payload), {"api_token": secret}
        )
        collector.start()
        report = collector.finish()

        self.assertEqual(report["observed_bytes"], len(payload))
        self.assertEqual(report["sha256"], hashlib.sha256(payload).hexdigest())
        self.assertTrue(report["drain_completed"])
        self.assertFalse(report["secret_scan_passed"])
        self.assertEqual(report["canary_labels_detected"], ["api_token"])
        self.assertFalse(report["raw_content_retained"])
        self.assertNotIn(secret, repr(report))
        self.assertLessEqual(
            report["maximum_scan_buffer_bytes"], READ_CHUNK_BYTES + len(secret) - 1
        )

    def test_clean_stream_produces_passable_non_retaining_evidence(self) -> None:
        payload = b"bounded runtime output\n" * 10_000
        replicas = {}
        for name in ("a", "b"):
            collector = StreamingLogCollector(
                name, io.BytesIO(payload), {"database_url": "postgres://secret"}
            )
            collector.start()
            replicas[name] = collector.finish()

        criterion = runtime_log_criterion(replicas, applicable=True)
        self.assertTrue(criterion["passed"])
        self.assertTrue(all(row["raw_content_retained"] is False for row in replicas.values()))

    def test_runtime_log_criterion_fails_closed_and_target_is_not_applicable(self) -> None:
        failed = {
            "a": {
                "drain_completed": True,
                "secret_scan_passed": True,
                "raw_content_retained": False,
                "error": None,
            }
        }
        self.assertFalse(runtime_log_criterion(failed, applicable=True)["passed"])
        target = runtime_log_criterion({}, applicable=False)
        self.assertIsNone(target["passed"])
        self.assertFalse(target["applicable"])

    def test_collector_join_timeout_fails_without_retaining_stream_content(self) -> None:
        release = threading.Event()

        class BlockingStream:
            def read(self, _size: int) -> bytes:
                release.wait(2)
                return b""

            def close(self) -> None:
                return

        collector = StreamingLogCollector("a", BlockingStream(), {"token": "secret"})
        collector.start()
        report = collector.finish(timeout_seconds=0.01)
        self.assertFalse(report["drain_completed"])
        self.assertFalse(report["secret_scan_passed"])
        self.assertEqual(report["error"], "log collector thread did not stop")
        self.assertIsNone(report["sha256"])
        release.set()
        collector.thread.join(timeout=1)

    def test_canary_validation_is_bounded_and_omits_empty_values(self) -> None:
        self.assertEqual(validate_canaries({"empty": "", "token": "value"}), {"token": b"value"})
        with self.assertRaises(ValueError):
            validate_canaries({"token": "x" * (64 * 1024 + 1)})

    def test_launcher_refuses_to_start_without_run_specific_canaries(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            launcher = ReplicaLauncher(root, root)
            with self.assertRaisesRegex(RuntimeError, "canaries were not configured"):
                launcher.start("a", root / "missing", "127.0.0.1", 3311, {})

    def test_runtime_directory_rejects_dotenv_in_any_parent(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            for relative_env in (Path("external/.env"), Path(".env")):
                with self.subTest(relative_env=relative_env):
                    runtime = root / "external" / "runtime"
                    runtime.mkdir(parents=True, exist_ok=True)
                    env_path = root / relative_env
                    env_path.parent.mkdir(parents=True, exist_ok=True)
                    env_path.touch()
                    with self.assertRaisesRegex(RuntimeError, "ancestor chain"):
                        verify_runtime_directory(runtime, source)
                    env_path.unlink()

    def test_launcher_rejects_parent_dotenv_before_spawn_and_cleans(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            flows = source / "flows"
            runtime = root / "external" / "runtime"
            flows.mkdir(parents=True)
            runtime.mkdir(parents=True)
            (runtime.parent / ".env").touch()
            cleanup = Mock()
            temporary = SimpleNamespace(name=str(runtime), cleanup=cleanup)
            launcher = ReplicaLauncher(source, flows)
            launcher.configure_log_canaries({"token": "secret"})
            with (
                patch(
                    "soak_runtime_directory.tempfile.TemporaryDirectory",
                    return_value=temporary,
                ),
                patch("soak_runtime_logs.subprocess.Popen") as popen,
                self.assertRaisesRegex(RuntimeError, "ancestor chain"),
            ):
                launcher.start("a", root / "missing", "127.0.0.1", 3311, {})
            popen.assert_not_called()
            cleanup.assert_called_once_with()

    def test_launcher_rejects_exact_flow_root_dotenv_before_allocating_runtime(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            flows = source / "flows"
            flows.mkdir(parents=True)
            (flows / ".env").touch()
            launcher = ReplicaLauncher(source, flows)
            launcher.configure_log_canaries({"token": "secret"})
            with (
                patch("soak_runtime_directory.tempfile.TemporaryDirectory") as temporary,
                patch("soak_runtime_logs.subprocess.Popen") as popen,
                self.assertRaisesRegex(RuntimeError, "flow root"),
            ):
                launcher.start("a", root / "missing", "127.0.0.1", 3311, {})
            temporary.assert_not_called()
            popen.assert_not_called()

    def test_launcher_uses_external_directory_and_removes_it_after_logs_drain(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            flows = source / "flows"
            flows.mkdir(parents=True)
            launcher = ReplicaLauncher(source, flows)
            launcher.configure_log_canaries({"token": "secret"})
            process = Mock()
            process.stdout = io.BytesIO(b"bounded output\n")
            process.poll.return_value = 0
            process.wait.return_value = 0
            process.returncode = 0
            with patch("soak_runtime_logs.subprocess.Popen", return_value=process) as popen:
                launcher.start(
                    "a", root / "binary", "127.0.0.1", 3311, {}
                )
            runtime = launcher.runtime_directory.current
            self.assertIsNotNone(runtime)
            assert runtime is not None
            with self.assertRaises(ValueError):
                runtime.relative_to(source)
            self.assertEqual(popen.call_args.kwargs["cwd"], runtime)

            outcome = launcher.stop_all()

            self.assertEqual(outcome["a"]["exit_code"], 0)
            self.assertFalse(runtime.exists())
            self.assertIsNone(launcher.runtime_directory.current)
            self.assertTrue(launcher.log_evidence["a"]["drain_completed"])
            self.assertFalse(launcher.log_evidence["a"]["raw_content_retained"])


if __name__ == "__main__":
    unittest.main()
